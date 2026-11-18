use std::collections::{HashSet, VecDeque};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex as StdMutex,
};
use tokio::sync::{Mutex, Notify};
use twilight_model::id::{
    marker::{ChannelMarker, UserMarker},
    Id,
};

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
    active_updaters: StdMutex<HashSet<Id<ChannelMarker>>>,
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
            active_updaters: StdMutex::new(HashSet::new()),
        }
    }

    pub fn try_start_channel_updater(&self, channel_id: Id<ChannelMarker>) -> bool {
        let mut guard = self
            .active_updaters
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        guard.insert(channel_id)
    }

    pub fn finish_channel_updater(&self, channel_id: Id<ChannelMarker>) {
        let mut guard = self
            .active_updaters
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        guard.remove(&channel_id);
    }

    /// Lock ordering invariant: ALWAYS locks `queue` BEFORE `active_updaters`.
    pub async fn finish_channel_updater_if_idle(&self, channel_id: Id<ChannelMarker>) -> bool {
        let q = self.queue.lock().await;
        let has_jobs = q.iter().any(|entry| entry.input_channel == channel_id);
        if has_jobs {
            return false;
        }
        let mut updaters = self
            .active_updaters
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        updaters.remove(&channel_id);
        true
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
