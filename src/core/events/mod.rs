use std::{
    collections::HashSet,
    fmt::{Display, Formatter, Result as FmtResult},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use eyre::{Context as _, Result};
use once_cell::sync::Lazy;
use twilight_gateway::{EventTypeFlags, Shard, StreamExt as _};
use twilight_model::{
    gateway::{
        payload::outgoing::UpdatePresence,
        presence::{ActivityType, MinimalActivity, Status},
    },
    id::{marker::GuildMarker, Id},
};

use crate::util::Authored;

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

pub fn event_loop(ctx: Arc<Context>, shards: Vec<Shard>) -> Vec<tokio::task::JoinHandle<()>> {
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
        let event = match shard.next_event(EventTypeFlags::all()).await {
            Some(Ok(event)) => event,
            Some(Err(err)) => {
                warn!("Error on shard {shard_id}: {err:?}");
                continue;
            }
            None => break,
        };

        ctx.cache.update(&event);
        let ctx = Arc::clone(&ctx);

        tokio::spawn(async move {
            if let Err(err) = handle_event(ctx, event, shard_id)
                .await
                .context("error while handling event")
            {
                error!("{err:?}");
            }
        });
    }
}

#[derive(Default)]
struct PresenceTracker {
    guilds: HashSet<Id<GuildMarker>>,
    last_sent_count: Option<usize>,
    last_sent_at: Option<Instant>,
}

static PRESENCE_TRACKER: Lazy<Mutex<PresenceTracker>> =
    Lazy::new(|| Mutex::new(PresenceTracker::default()));

fn send_presence_update(ctx: &Context, force: bool) -> Result<()> {
    let count = {
        let mut tracker = PRESENCE_TRACKER
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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

async fn handle_event(
    ctx: Arc<Context>,
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
            {
                let mut tracker = PRESENCE_TRACKER
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                tracker.guilds.insert(gc.id());
            }
            send_presence_update(&ctx, false)?;
        }
        Event::GuildDelete(gd) => {
            if gd.unavailable != Some(true) {
                {
                    let mut tracker = PRESENCE_TRACKER
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    tracker.guilds.remove(&gd.id);
                }
                send_presence_update(&ctx, false)?;
            }
        }
        Event::InteractionCreate(e) => handle_interaction(ctx, e.0).await,
        Event::MessageCreate(msg) => handle_message(ctx, msg.0).await,
        Event::Ready(ready) => {
            info!("Shard {shard_id} is ready");
            {
                let mut tracker = PRESENCE_TRACKER
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                for g in &ready.guilds {
                    tracker.guilds.insert(g.id);
                }
            }
            send_presence_update(&ctx, true)?;
        }
        Event::Resumed => info!("Shard {shard_id} is resumed"),
        _ => {}
    }

    Ok(())
}
