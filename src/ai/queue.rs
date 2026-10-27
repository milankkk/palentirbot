use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

pub const DEFAULT_MAX_AI_QUEUE_SIZE: usize = 15;

/// Strict FIFO queue for AI generation requests.
/// Ensures requests are processed in the exact order they entered the queue,
/// preventing server overload and race conditions.
pub struct AiQueue {
    inner: Arc<Mutex<AiQueueInner>>,
    max_size: usize,
}

struct AiQueueInner {
    running: bool,
    waiters: VecDeque<oneshot::Sender<()>>,
}

pub struct AiQueueTicket {
    inner: Arc<Mutex<AiQueueInner>>,
    receiver: Option<oneshot::Receiver<()>>,
    position: usize,
    active_runner: bool,
}

pub struct AiQueuePermit {
    inner: Arc<Mutex<AiQueueInner>>,
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
        let mut inner = self.inner.lock().await;
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
                active_runner: true,
            })
        } else {
            let (tx, rx) = oneshot::channel();
            inner.waiters.push_back(tx);
            let position = inner.waiters.len();
            Ok(AiQueueTicket {
                inner: Arc::clone(&self.inner),
                receiver: Some(rx),
                position,
                active_runner: false,
            })
        }
    }

    /// Total number of AI requests currently executing or waiting in line.
    pub async fn len(&self) -> usize {
        let inner = self.inner.lock().await;
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
        self.active_runner = false; // Transferred to permit
        if let Some(rx) = self.receiver.take() {
            let _ = rx.await;
        }

        AiQueuePermit {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for AiQueueTicket {
    fn drop(&mut self) {
        if self.active_runner {
            let inner_clone = Arc::clone(&self.inner);
            tokio::spawn(async move {
                let mut inner = inner_clone.lock().await;
                while let Some(tx) = inner.waiters.pop_front() {
                    if tx.send(()).is_ok() {
                        return; // Next waiter activated
                    }
                }
                inner.running = false;
            });
        }
    }
}

impl Drop for AiQueuePermit {
    fn drop(&mut self) {
        let inner_clone = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let mut inner = inner_clone.lock().await;
            while let Some(tx) = inner.waiters.pop_front() {
                if tx.send(()).is_ok() {
                    return; // Next waiter activated
                }
            }
            inner.running = false;
        });
    }
}
