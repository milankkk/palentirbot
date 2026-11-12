use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
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
    tx: oneshot::Sender<()>,
    woken: Arc<AtomicBool>,
}

struct AiQueueInner {
    running: bool,
    waiters: VecDeque<AiWaiter>,
}

pub struct AiQueueTicket {
    inner: Arc<Mutex<AiQueueInner>>,
    receiver: Option<oneshot::Receiver<()>>,
    position: usize,
    woken: Arc<AtomicBool>,
}

pub struct AiQueuePermit {
    inner: Arc<Mutex<AiQueueInner>>,
}

fn lock_inner(inner: &Mutex<AiQueueInner>) -> std::sync::MutexGuard<'_, AiQueueInner> {
    inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn advance_queue(inner_arc: &Mutex<AiQueueInner>) {
    let mut inner = lock_inner(inner_arc);
    while let Some(waiter) = inner.waiters.pop_front() {
        // Mark woken before send; if receiver dropped, send returns Err and we continue.
        waiter.woken.store(true, Ordering::Release);
        if waiter.tx.send(()).is_ok() {
            return;
        }
        waiter.woken.store(false, Ordering::Release);
    }
    inner.running = false;
}

impl AiQueue {
    pub fn new(max_size: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(AiQueueInner {
                running: false,
                waiters: VecDeque::new(),
            })),
            max_size,
        }
    }

    /// Enter the queue, reserving your exact spot in FIFO order.
    pub async fn enter(&self) -> Result<AiQueueTicket, usize> {
        let mut inner = lock_inner(&self.inner);
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
                receiver: None,
                position: 0,
                woken: Arc::new(AtomicBool::new(true)),
            })
        } else {
            let (tx, rx) = oneshot::channel();
            let woken = Arc::new(AtomicBool::new(false));
            inner.waiters.push_back(AiWaiter {
                tx,
                woken: Arc::clone(&woken),
            });
            let position = inner.waiters.len();
            Ok(AiQueueTicket {
                inner: Arc::clone(&self.inner),
                receiver: Some(rx),
                position,
                woken,
            })
        }
    }

    /// Total number of AI requests currently executing or waiting in line.
    pub async fn len(&self) -> usize {
        let inner = lock_inner(&self.inner);
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
        if let Some(rx) = self.receiver.take() {
            let _ = rx.await;
        }

        // Ownership of the running slot transfers to AiQueuePermit; disarm ticket Drop.
        self.woken.store(false, Ordering::Release);

        AiQueuePermit {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for AiQueueTicket {
    fn drop(&mut self) {
        if self.woken.swap(false, Ordering::AcqRel) {
            advance_queue(&self.inner);
        }
    }
}

impl Drop for AiQueuePermit {
    fn drop(&mut self) {
        advance_queue(&self.inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ai_queue_fifo_and_capacity() {
        let queue = AiQueue::new(2);
        assert!(queue.is_empty().await);

        let ticket1 = queue.enter().await.expect("first ticket should succeed");
        assert_eq!(ticket1.position(), 0);
        assert_eq!(queue.len().await, 1);

        let ticket2 = queue.enter().await.expect("second ticket should succeed");
        assert_eq!(ticket2.position(), 1);
        assert_eq!(queue.len().await, 2);

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
        // Release p1: this wakes t2 (sets t2.woken = true, running remains true)
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
}
