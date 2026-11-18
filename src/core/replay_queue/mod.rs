use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use tokio::sync::{Mutex, Notify};
use twilight_model::id::{marker::UserMarker, Id};

pub use self::cache::RenderCache;
pub use self::data::{
    is_lazer_replay, PpTriple, RenderOptions, ReplayData, ReplaySlim, ReplayStatus, TimePoints,
};

pub mod cache;
mod data;
pub mod process;

pub enum CancelResult {
    CancelledRunning,
    RemovedQueued(Box<ReplayData>),
    Forbidden,
    NotFound,
}

pub struct ReplayQueue {
    pub queue: Mutex<VecDeque<ReplayData>>,
    pub status: Mutex<ReplayStatus>,
    pub notify: Arc<Notify>,
    pub cache: RenderCache,
    work_notify: Notify,
    next_job_id: AtomicU64,
    updater_running: AtomicBool,
}

impl ReplayQueue {
    pub async fn load() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            status: Mutex::new(ReplayStatus::Waiting),
            notify: Arc::new(Notify::new()),
            cache: RenderCache::load().await,
            work_notify: Notify::new(),
            next_job_id: AtomicU64::new(0),
            updater_running: AtomicBool::new(false),
        }
    }

    pub fn try_start_updater(&self) -> bool {
        self.updater_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn finish_updater(&self) {
        self.updater_running.store(false, Ordering::Release);
    }

    pub fn allocate_job_id(&self) -> u64 {
        self.next_job_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub async fn push(
        &self,
        mut entry: ReplayData,
        max_per_user: Option<usize>,
    ) -> std::result::Result<(bool, u64), usize> {
        if entry.job_id == 0 {
            entry.job_id = self.allocate_job_id();
        }
        let job_id = entry.job_id;
        let user = entry.user;
        let was_empty = {
            let mut guard = self.queue.lock().await;
            if let Some(limit) = max_per_user {
                let active = guard.iter().filter(|item| item.user == user).count();
                if active >= limit {
                    return Err(active);
                }
            }
            let was_empty = guard.is_empty();
            guard.push_back(entry);
            was_empty
        };
        self.work_notify.notify_one();
        Ok((was_empty, job_id))
    }

    pub async fn count_user_jobs(&self, user: Id<UserMarker>) -> usize {
        self.queue
            .lock()
            .await
            .iter()
            .filter(|entry| entry.user == user)
            .count()
    }

    pub async fn cancel_job(
        &self,
        job_id: u64,
        requester: Id<UserMarker>,
        is_admin: bool,
    ) -> CancelResult {
        let mut guard = self.queue.lock().await;
        let Some(idx) = guard.iter().position(|entry| entry.job_id == job_id) else {
            return CancelResult::NotFound;
        };

        if !is_admin && guard[idx].user != requester {
            return CancelResult::Forbidden;
        }

        if idx == 0 {
            guard[0].cancelled.store(true, Ordering::Release);
            guard[0].cancel_notify.notify_waiters();
            drop(guard);
            self.notify.notify_waiters();
            CancelResult::CancelledRunning
        } else {
            let removed = guard.remove(idx).expect("checked index");
            drop(guard);
            self.notify.notify_waiters();
            CancelResult::RemovedQueued(Box::new(removed))
        }
    }

    pub async fn is_active(&self) -> bool {
        let is_empty = self.queue.lock().await.is_empty();
        let status = *self.status.lock().await;
        !is_empty || !matches!(status, ReplayStatus::Waiting)
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
        let _ = self.pop().await;
        *self.status.lock().await = ReplayStatus::Waiting;
        self.notify.notify_waiters();
    }
}
