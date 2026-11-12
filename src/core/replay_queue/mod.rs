use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

pub use self::data::{is_lazer_replay, PpTriple, ReplayData, ReplaySlim, ReplayStatus, TimePoints};

mod data;
pub mod process;

pub struct ReplayQueue {
    pub queue: Mutex<VecDeque<ReplayData>>,
    pub status: Mutex<ReplayStatus>,
    pub notify: Arc<Notify>,
    work_notify: Notify,
}

impl ReplayQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn push(&self, entry: ReplayData) -> bool {
        let was_empty = {
            let mut guard = self.queue.lock().await;
            let was_empty = guard.is_empty();
            guard.push_back(entry);
            was_empty
        };
        self.work_notify.notify_one();
        was_empty
    }

    pub async fn pop(&self) -> Option<ReplayData> {
        self.queue.lock().await.pop_front()
    }

    pub async fn peek(&self) -> ReplayData {
        loop {
            let notified = self.work_notify.notified();
            if let Some(entry) = self.queue.lock().await.front().cloned() {
                return entry;
            }
            notified.await;
        }
    }

    pub async fn set_status(&self, status: ReplayStatus) {
        *self.status.lock().await = status;
        self.notify.notify_waiters();
    }

    pub async fn reset_peek(&self) {
        *self.status.lock().await = ReplayStatus::Waiting;
        let _ = self.pop().await;
        self.notify.notify_waiters();
    }
}

impl Default for ReplayQueue {
    fn default() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            status: Mutex::new(ReplayStatus::Waiting),
            notify: Arc::new(Notify::new()),
            work_notify: Notify::new(),
        }
    }
}
