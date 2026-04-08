use std::{
    fmt::{Display, Formatter, Result as FmtResult},
    sync::Arc,
};

use eyre::{Context as _, Result};
use twilight_gateway::{EventTypeFlags, Shard, StreamExt as _};
use twilight_model::gateway::{
    payload::outgoing::UpdatePresence,
    presence::{ActivityType, MinimalActivity, Status},
};

use crate::util::Authored;

use self::{interaction::handle_interaction, message::handle_message};

use super::Context;

mod interaction;
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

pub async fn event_loop(ctx: Arc<Context>, shards: Vec<Shard>) {
    let mut handles = Vec::new();

    for shard in shards {
        let ctx = Arc::clone(&ctx);
        handles.push(tokio::spawn(process_shard(ctx, shard)));
    }

    futures::future::join_all(handles).await;
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
        ctx.standby.process(&event);
        let ctx = Arc::clone(&ctx);

        tokio::spawn(async move {
            if let Err(err) = handle_event(ctx, event, shard_id).await.context("error while handling event") {
                error!("{err:?}");
            }
        });
    }
}

async fn handle_event(ctx: Arc<Context>, event: twilight_gateway::Event, shard_id: u32) -> Result<()> {
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
        Event::GuildCreate(_) | Event::GuildDelete(_) => {
            let stats = ctx.cache.stats();
            let count = stats.guilds() + stats.unavailable_guilds();

            let activity = MinimalActivity {
                kind: ActivityType::Watching,
                name: format!("in {count} servers"),
                url: None,
            };

            let req = UpdatePresence::new(vec![activity.into()], false, None, Status::Online)?;

            if let Some(sender) = ctx.shard_senders.get(shard_id as usize) {
                sender.command(&req).context("failed to update activity")?;
            }
        }
        Event::InteractionCreate(e) => handle_interaction(ctx, e.0).await,
        Event::MessageCreate(msg) => handle_message(ctx, msg.0).await,
        Event::Ready(_) => info!("Shard {shard_id} is ready"),
        Event::Resumed => info!("Shard {shard_id} is resumed"),
        _ => {}
    }

    Ok(())
}
