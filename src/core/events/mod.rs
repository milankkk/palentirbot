use std::{
    collections::HashSet,
    fmt::{Display, Formatter, Result as FmtResult},
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, LazyLock, Mutex,
    },
    time::{Duration, Instant},
};

use eyre::{Context as _, Result};
use twilight_gateway::{EventTypeFlags, Shard, StreamExt as _};
use twilight_model::{
    gateway::{
        payload::outgoing::UpdatePresence,
        presence::{ActivityType, MinimalActivity, Status},
    },
    id::{marker::GuildMarker, Id},
};

use crate::util::{hasher::IntBuildHasher, Authored};

use self::{interaction::handle_interaction, message::handle_message};

use super::Context;

pub mod interaction;
mod message;

#[derive(Debug)]
enum ProcessResult {
    Success,
    NoOwner,
    NoAuthority,
}

struct EventLocation<'a> {
    ctx: &'a Context,
    cmd: &'a dyn Authored,
}

impl<'a> EventLocation<'a> {
    fn new(ctx: &'a Context, cmd: &'a dyn Authored) -> Self {
        Self { ctx, cmd }
    }
}

impl Display for EventLocation<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        let guild = match self.cmd.guild_id() {
            Some(id) => id,
            None => return f.write_str("Private"),
        };

        match self.ctx.cache.guild(guild, |g| write!(f, "{}:", g.name())) {
            Ok(Ok(_)) => {
                let channel_res = self.ctx.cache.channel(self.cmd.channel_id(), |c| {
                    f.write_str(c.name.as_deref().unwrap_or("<uncached channel>"))
                });

                match channel_res {
                    Ok(Ok(_)) => Ok(()),
                    Ok(err) => err,
                    Err(_) => f.write_str("<uncached channel>"),
                }
            }
            Ok(err) => err,
            Err(_) => f.write_str("<uncached guild>"),
        }
    }
}

const SHARD_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

static LAST_GATEWAY_EVENT_UNIX: AtomicI64 = AtomicI64::new(0);

pub fn mark_gateway_alive() {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    LAST_GATEWAY_EVENT_UNIX.store(now, Ordering::Relaxed);
}

pub fn seconds_since_gateway_event() -> i64 {
    let last = LAST_GATEWAY_EVENT_UNIX.load(Ordering::Relaxed);
    if last <= 0 {
        return 0;
    }
    time::OffsetDateTime::now_utc()
        .unix_timestamp()
        .saturating_sub(last)
}

pub fn event_loop(ctx: Arc<Context>, shards: Vec<Shard>) -> Vec<tokio::task::JoinHandle<()>> {
    mark_gateway_alive();
    let mut handles = Vec::with_capacity(shards.len());

    for shard in shards {
        let ctx = Arc::clone(&ctx);
        handles.push(tokio::spawn(process_shard(ctx, shard)));
    }

    handles
}

async fn process_shard(ctx: Arc<Context>, mut shard: Shard) {
    let shard_id = shard.id().number();

    loop {
        let next_ev =
            tokio::time::timeout(SHARD_IDLE_TIMEOUT, shard.next_event(EventTypeFlags::all())).await;

        let event = match next_ev {
            Ok(Some(Ok(event))) => {
                mark_gateway_alive();
                event
            }
            Ok(Some(Err(err))) => {
                warn!("Error on shard {shard_id}: {err:?}");
                continue;
            }
            Ok(None) => {
                warn!("Shard {shard_id} event stream ended");
                break;
            }
            Err(_) => {
                error!(
                    "Shard {shard_id} received zero gateway/heartbeat events for {}s; breaking for clean restart",
                    SHARD_IDLE_TIMEOUT.as_secs()
                );
                break;
            }
        };

        ctx.cache.update(&event);

        match event {
            twilight_gateway::Event::InteractionCreate(e) => {
                let ctx = Arc::clone(&ctx);
                tokio::spawn(async move { handle_interaction(ctx, e.0).await });
            }
            twilight_gateway::Event::MessageCreate(msg) => {
                let ctx = Arc::clone(&ctx);
                tokio::spawn(async move { handle_message(ctx, msg.0).await });
            }
            other => {
                if let Err(err) = handle_sync_event(&ctx, other, shard_id)
                    .context("error while handling event")
                {
                    error!("{err:?}");
                }
            }
        }
    }
}

#[derive(Default)]
struct PresenceTracker {
    guilds: HashSet<Id<GuildMarker>, IntBuildHasher>,
    last_sent_count: Option<usize>,
    last_sent_at: Option<Instant>,
}

static PRESENCE_TRACKER: LazyLock<Mutex<PresenceTracker>> =
    LazyLock::new(|| Mutex::new(PresenceTracker::default()));

fn update_presence_guilds(
    ctx: &Context,
    force: bool,
    mutate: impl FnOnce(&mut HashSet<Id<GuildMarker>, IntBuildHasher>),
) -> Result<()> {
    let count = {
        let mut tracker = PRESENCE_TRACKER
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        mutate(&mut tracker.guilds);
        let count = tracker.guilds.len();
        if !force && tracker.last_sent_count == Some(count) {
            return Ok(());
        }
        if !force
            && tracker
                .last_sent_at
                .is_some_and(|t| t.elapsed() < Duration::from_secs(15))
        {
            return Ok(());
        }
        tracker.last_sent_count = Some(count);
        tracker.last_sent_at = Some(Instant::now());
        count
    };

    let activity = MinimalActivity {
        kind: ActivityType::Watching,
        name: format!("in {count} servers"),
        url: None,
    };

    let req = UpdatePresence::new(vec![activity.into()], false, None, Status::Online)?;

    for sender in &ctx.shard_senders {
        let _ = sender.command(&req);
    }

    Ok(())
}

fn handle_sync_event(
    ctx: &Context,
    event: twilight_gateway::Event,
    shard_id: u32,
) -> Result<()> {
    use twilight_gateway::Event;

    match event {
        Event::GatewayInvalidateSession(true) => {
            warn!("Gateway invalidated session for shard {shard_id}, but its reconnectable")
        }
        Event::GatewayInvalidateSession(false) => {
            warn!("Gateway invalidated session for shard {shard_id}")
        }
        Event::GatewayReconnect => {
            info!("Gateway requested shard {shard_id} to reconnect")
        }
        Event::GuildCreate(gc) => {
            update_presence_guilds(ctx, false, |guilds| {
                guilds.insert(gc.id());
            })?;
        }
        Event::GuildDelete(gd) if gd.unavailable != Some(true) => {
            update_presence_guilds(ctx, false, |guilds| {
                guilds.remove(&gd.id);
            })?;
        }
        Event::Ready(ready) => {
            info!("Shard {shard_id} is ready");
            update_presence_guilds(ctx, true, |guilds| {
                for g in &ready.guilds {
                    guilds.insert(g.id);
                }
            })?;
        }
        Event::Resumed => info!("Shard {shard_id} is resumed"),
        _ => {}
    }

    Ok(())
}
