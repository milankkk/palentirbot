use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

pub const DEFAULT_MAX_AI_QUEUE_SIZE: usize = 15;

/// Strict FIFO queue for AI generation requests.
/// Ensures requests are processed in the exact order they entered the queue,
/// preventing server overload and race conditions.
pub struct AiQueue {
    inner: Arc<Mutex<AiQueueInner>>,
    max_size: usize,
}

struct AiWaiter {
    id: u64,
    tx: oneshot::Sender<()>,
}

struct AiQueueInner {
    running: bool,
    next_id: u64,
    waiters: VecDeque<AiWaiter>,
}

enum TicketState {
    Active,
    Waiting {
        id: u64,
        rx: oneshot::Receiver<()>,
    },
    Consumed,
}

pub struct AiQueueTicket {
    inner: Arc<Mutex<AiQueueInner>>,
    position: usize,
    state: TicketState,
}

pub struct AiQueuePermit {
    inner: Arc<Mutex<AiQueueInner>>,
}

fn lock_inner(inner: &Mutex<AiQueueInner>) -> std::sync::MutexGuard<'_, AiQueueInner> {
    inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn advance_queue_locked(inner: &mut AiQueueInner) {
    while let Some(waiter) = inner.waiters.pop_front() {
        if waiter.tx.send(()).is_ok() {
            return;
        }
    }
    inner.running = false;
}

impl AiQueue {
    pub fn new(max_size: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(AiQueueInner {
                running: false,
                next_id: 0,
                waiters: VecDeque::new(),
            })),
            max_size,
        }
    }

    pub fn is_full(&self) -> bool {
        let mut inner = lock_inner(&self.inner);
        inner.waiters.retain(|w| !w.tx.is_closed());
        let total = if inner.running {
            1 + inner.waiters.len()
        } else {
            0
        };
        total >= self.max_size
    }

    /// Enter the queue, reserving your exact spot in FIFO order.
    pub async fn enter(&self) -> Result<AiQueueTicket, usize> {
        let mut inner = lock_inner(&self.inner);
        inner.waiters.retain(|w| !w.tx.is_closed());
        let total = if inner.running {
            1 + inner.waiters.len()
        } else {
            0
        };

        if total >= self.max_size {
            return Err(total);
        }

        if !inner.running {
            inner.running = true;
            Ok(AiQueueTicket {
                inner: Arc::clone(&self.inner),
                position: 0,
                state: TicketState::Active,
            })
        } else {
            inner.next_id = inner.next_id.wrapping_add(1);
            let id = inner.next_id;
            let (tx, rx) = oneshot::channel();
            inner.waiters.push_back(AiWaiter { id, tx });
            let position = inner.waiters.len();
            Ok(AiQueueTicket {
                inner: Arc::clone(&self.inner),
                position,
                state: TicketState::Waiting { id, rx },
            })
        }
    }

    /// Total number of AI requests currently executing or waiting in line.
    pub async fn len(&self) -> usize {
        let mut inner = lock_inner(&self.inner);
        inner.waiters.retain(|w| !w.tx.is_closed());
        if inner.running {
            1 + inner.waiters.len()
        } else {
            0
        }
    }

    /// Whether the queue is currently empty.
    #[allow(dead_code)]
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

impl AiQueueTicket {
    /// Number of requests ahead in the queue.
    pub fn position(&self) -> usize {
        self.position
    }

    /// Wait in strict FIFO order until it is this request's turn.
    pub async fn wait(mut self) -> AiQueuePermit {
        if let TicketState::Waiting { ref mut rx, .. } = self.state {
            let _ = rx.await;
        }
        self.state = TicketState::Consumed;

        AiQueuePermit {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for AiQueueTicket {
    fn drop(&mut self) {
        let state = std::mem::replace(&mut self.state, TicketState::Consumed);
        match state {
            TicketState::Active => {
                let mut inner = lock_inner(&self.inner);
                advance_queue_locked(&mut inner);
            }
            TicketState::Waiting { id, mut rx } => {
                let mut inner = lock_inner(&self.inner);
                if let Some(pos) = inner.waiters.iter().position(|w| w.id == id) {
                    inner.waiters.remove(pos);
                } else if rx.try_recv().is_ok() {
                    // Advance handed us the running permit before we dropped; pass it on.
                    advance_queue_locked(&mut inner);
                }
            }
            TicketState::Consumed => {}
        }
    }
}

impl Drop for AiQueuePermit {
    fn drop(&mut self) {
        let mut inner = lock_inner(&self.inner);
        advance_queue_locked(&mut inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ai_queue_fifo_and_capacity() {
        let queue = AiQueue::new(2);
        assert!(queue.is_empty().await);
        assert!(!queue.is_full());

        let ticket1 = queue.enter().await.expect("first ticket should succeed");
        assert_eq!(ticket1.position(), 0);
        assert_eq!(queue.len().await, 1);

        let ticket2 = queue.enter().await.expect("second ticket should succeed");
        assert_eq!(ticket2.position(), 1);
        assert_eq!(queue.len().await, 2);
        assert!(queue.is_full());

        // Queue is now at capacity (2)
        let err = queue.enter().await.err().expect("third ticket should fail");
        assert_eq!(err, 2);

        let permit1 = ticket1.wait().await;
        drop(permit1);

        // Next waiter should be woken up
        let permit2 = ticket2.wait().await;
        drop(permit2);

        assert_eq!(queue.len().await, 0);
    }

    #[tokio::test]
    async fn test_cancelled_waiter_after_wakeup_does_not_deadlock() {
        let queue = AiQueue::new(3);
        let t1 = queue.enter().await.unwrap();
        let t2 = queue.enter().await.unwrap();
        let t3 = queue.enter().await.unwrap();

        let p1 = t1.wait().await;
        // Release p1: this sends () to t2 while t2 is still Waiting
        drop(p1);

        // Now drop t2 without completing t2.wait(), simulating an aborted/cancelled task!
        drop(t2);

        // t3 must still be woken up instead of deadlocking forever
        let p3 = tokio::time::timeout(std::time::Duration::from_millis(200), t3.wait())
            .await
            .expect("t3 should not deadlock when woken t2 is dropped");
        drop(p3);

        assert_eq!(queue.len().await, 0);
    }

    #[tokio::test]
    async fn test_concurrent_cancellation_stress_no_deadlock() {
        let queue = Arc::new(AiQueue::new(64));

        for iter in 0..200 {
            let mut handles = Vec::new();
            for task_idx in 0..10 {
                let q = Arc::clone(&queue);
                handles.push(tokio::spawn(async move {
                    if let Ok(ticket) = q.enter().await {
                        if (iter + task_idx) % 3 == 0 {
                            // Cancel immediately before wait
                            drop(ticket);
                        } else if (iter + task_idx) % 3 == 1 {
                            // Race wait() against immediate yield cancellation
                            tokio::select! {
                                biased;
                                _ = tokio::task::yield_now() => {}
                                permit = ticket.wait() => {
                                    drop(permit);
                                }
                            }
                        } else {
                            let permit = ticket.wait().await;
                            tokio::task::yield_now().await;
                            drop(permit);
                        }
                    }
                }));
            }
            for h in handles {
                let _ = h.await;
            }

            let probe = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                queue.enter(),
            )
            .await
            .expect("queue enter timed out")
            .expect("queue should not be full");
            let permit = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                probe.wait(),
            )
            .await
            .expect("queue wait deadlocked after cancellation stress");
            drop(permit);
            assert_eq!(queue.len().await, 0);
        }
    }
}
