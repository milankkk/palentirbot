use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    Mutex, Notify,
};

pub use self::data::{ReplayData, ReplaySlim, ReplayStatus, TimePoints};

mod data;
pub mod process;

pub struct ReplayQueue {
    pub queue: Mutex<VecDeque<ReplayData>>,
    pub status: Mutex<ReplayStatus>,
    pub notify: Arc<Notify>,
    tx: UnboundedSender<()>,
    rx: Mutex<UnboundedReceiver<()>>,
}

impl ReplayQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn push(&self, entry: ReplayData) {
        self.queue.lock().await.push_back(entry);
        let _ = self.tx.send(());
    }

    pub async fn pop(&self) -> ReplayData {
        self.queue.lock().await.pop_front().unwrap()
    }

    pub async fn peek(&self) -> ReplayData {
        let mut guard = self.rx.lock().await;
        let _ = guard.recv().await;
        self.queue.lock().await.front().unwrap().to_owned()
    }

    pub async fn set_status(&self, status: ReplayStatus) {
        *self.status.lock().await = status;
        self.notify.notify_waiters();
    }

    pub async fn reset_peek(&self) {
        *self.status.lock().await = ReplayStatus::Waiting;
        self.pop().await;
        self.notify.notify_waiters();
    }
}

impl Default for ReplayQueue {
    fn default() -> Self {
        let (tx, rx) = unbounded_channel();
        Self {
            queue: Mutex::new(VecDeque::new()),
            status: Mutex::new(ReplayStatus::Waiting),
            notify: Arc::new(Notify::new()),
            tx,
            rx: Mutex::new(rx),
        }
    }
}
