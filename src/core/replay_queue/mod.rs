use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex as StdMutex,
};
use tokio::sync::{Mutex, Notify, RwLock};
use twilight_model::id::{
    marker::{ChannelMarker, UserMarker},
    Id,
};

pub use self::cache::RenderCache;
pub use self::data::{
    is_lazer_replay, PpTriple, RenderOptions, ReplayData, ReplaySlim, ReplayStatus, TimePoints,
    WebJobHandle, WebJobState,
};

pub mod adaptive;
pub mod cache;
pub mod danser;
mod data;
pub mod embed;
pub mod mapset;
pub mod md5;
pub mod pp;
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
    pub web_jobs: RwLock<HashMap<String, Arc<WebJobHandle>>>,
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
            web_jobs: RwLock::new(HashMap::new()),
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
        let front_web = self
            .queue
            .lock()
            .await
            .front()
            .and_then(|e| e.web.as_ref().map(Arc::clone));
        if let Some(web) = front_web {
            let mut st = web.state.write().await;
            if !st.is_terminal() {
                match status {
                    ReplayStatus::Waiting => st.status = "queued".to_owned(),
                    ReplayStatus::Downloading => st.status = "downloading".to_owned(),
                    ReplayStatus::MapFound => st.status = "map_found".to_owned(),
                    ReplayStatus::Rendering(pct) => {
                        st.status = "rendering".to_owned();
                        st.progress = pct;
                    }
                    ReplayStatus::Uploading => {
                        st.status = "uploading".to_owned();
                        st.progress = 100;
                    }
                }
            }
        }
        self.notify.notify_waiters();
    }

    pub async fn push_web(
        &self,
        mut entry: ReplayData,
        handle: Arc<WebJobHandle>,
        max_per_ip: usize,
        max_total_queue: usize,
    ) -> std::result::Result<(usize, usize), String> {
        if entry.job_id == 0 {
            entry.job_id = self.allocate_job_id();
        }
        let client_ip = handle.client_ip.clone();
        let (position, total) = {
            let mut guard = self.queue.lock().await;
            if guard.len() >= max_total_queue {
                return Err(format!(
                    "The render queue is currently full ({max_total_queue} jobs). Please try again shortly."
                ));
            }
            let ip_active = guard
                .iter()
                .filter(|item| {
                    item.web
                        .as_ref()
                        .is_some_and(|w| w.client_ip == client_ip)
                })
                .count();
            if ip_active >= max_per_ip {
                return Err(format!(
                    "You already have {ip_active} active web render(s) in the queue (limit: {max_per_ip}). Wait for one to finish or cancel it first."
                ));
            }
            guard.push_back(entry);
            (guard.len(), guard.len())
        };
        {
            let mut jobs = self.web_jobs.write().await;
            jobs.insert(handle.token.clone(), handle);
        }
        self.prune_old_web_jobs().await;
        self.work_notify.notify_one();
        self.notify.notify_waiters();
        Ok((position, total))
    }

    pub async fn get_web_job(&self, token: &str) -> Option<Arc<WebJobHandle>> {
        self.web_jobs.read().await.get(token).cloned()
    }

    pub async fn web_job_queue_info(&self, token: &str) -> (Option<usize>, usize) {
        let guard = self.queue.lock().await;
        let total = guard.len();
        let pos = guard
            .iter()
            .position(|e| e.web.as_ref().is_some_and(|w| w.token == token))
            .map(|i| i + 1);
        (pos, total)
    }

    pub async fn cancel_web_job(&self, token: &str) -> bool {
        let mut guard = self.queue.lock().await;
        let Some(idx) = guard
            .iter()
            .position(|e| e.web.as_ref().is_some_and(|w| w.token == token))
        else {
            return false;
        };

        if idx == 0 {
            guard[0].cancelled.store(true, Ordering::Release);
            guard[0].cancel_notify.notify_waiters();
            let web = guard[0].web.clone();
            drop(guard);
            if let Some(w) = web {
                let mut st = w.state.write().await;
                if !st.is_terminal() {
                    st.status = "cancelled".to_owned();
                    st.finished_at = Some(time::OffsetDateTime::now_utc().unix_timestamp());
                }
            }
            self.notify.notify_waiters();
            true
        } else {
            let removed = guard.remove(idx).expect("checked index");
            drop(guard);
            let _ = tokio::fs::remove_file(&removed.path).await;
            if let Some(w) = removed.web {
                let _ = tokio::fs::remove_file(&w.settings_path).await;
                let mut st = w.state.write().await;
                st.status = "cancelled".to_owned();
                st.finished_at = Some(time::OffsetDateTime::now_utc().unix_timestamp());
            }
            self.notify.notify_waiters();
            true
        }
    }

    pub async fn prune_old_web_jobs(&self) {
        let cutoff = time::OffsetDateTime::now_utc().unix_timestamp() - 3600;
        let handles: Vec<(String, Arc<WebJobHandle>)> = {
            let jobs = self.web_jobs.read().await;
            jobs.iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect()
        };
        let mut expired = Vec::new();
        for (k, h) in handles {
            let st = h.state.read().await;
            if st.is_terminal() && st.finished_at.unwrap_or(st.created_at) < cutoff {
                expired.push(k);
            }
        }
        if !expired.is_empty() {
            let mut jobs = self.web_jobs.write().await;
            for k in expired {
                jobs.remove(&k);
            }
        }
    }

    pub async fn reset_peek(&self) {
        let _ = self.pop().await;
        *self.status.lock().await = ReplayStatus::Waiting;
        self.notify.notify_waiters();
    }
}
