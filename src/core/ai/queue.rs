use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Mutex, MutexGuard};

pub const DEFAULT_MAX_AI_QUEUE_SIZE: usize = 15;

/// Fair FIFO queue for AI generation requests.
/// Ensures only one request is actively processed by the AI backend at a time
/// to prevent server overload, while preserving strict request order.
pub struct AiQueue {
    lock: Mutex<()>,
    total_queued: AtomicUsize,
    max_size: usize,
}

pub struct AiQueueTicket<'a> {
    queue: &'a AiQueue,
    position: usize,
    active: bool,
}

pub struct AiQueuePermit<'a> {
    _guard: MutexGuard<'a, ()>,
    queue: &'a AiQueue,
}

impl AiQueue {
    pub fn new(max_size: usize) -> Self {
        Self {
            lock: Mutex::new(()),
            total_queued: AtomicUsize::new(0),
            max_size,
        }
    }

    /// Try to enter the queue.
    ///
    /// Returns `Ok(ticket)` if a slot is available, or `Err(queue_length)`
    /// if the queue has reached `max_size`.
    pub fn enter(&self) -> Result<AiQueueTicket<'_>, usize> {
        let mut current = self.total_queued.load(Ordering::SeqCst);
        loop {
            if current >= self.max_size {
                return Err(current);
            }
            match self.total_queued.compare_exchange_weak(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }

        let position = current;
        Ok(AiQueueTicket {
            queue: self,
            position,
            active: true,
        })
    }

    /// Total number of AI requests currently executing or waiting in line.
    pub fn len(&self) -> usize {
        self.total_queued.load(Ordering::SeqCst)
    }

    /// Whether the queue is currently empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<'a> AiQueueTicket<'a> {
    /// Number of requests ahead in the queue.
    ///
    /// 0 means no one is ahead and the request will run immediately.
    /// Values > 0 indicate the position in line waiting for the AI.
    pub fn position(&self) -> usize {
        self.position
    }

    /// Wait in fair FIFO order until it is this request's turn.
    pub async fn wait(mut self) -> AiQueuePermit<'a> {
        let guard = self.queue.lock.lock().await;
        self.active = false;
        AiQueuePermit {
            _guard: guard,
            queue: self.queue,
        }
    }
}

impl<'a> Drop for AiQueueTicket<'a> {
    fn drop(&mut self) {
        if self.active {
            self.queue.total_queued.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl<'a> Drop for AiQueuePermit<'a> {
    fn drop(&mut self) {
        self.queue.total_queued.fetch_sub(1, Ordering::SeqCst);
    }
}
