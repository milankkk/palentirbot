use std::{
    fmt::{Display, Formatter, Result as FmtResult, Write},
    sync::Arc,
};

use command_macros::SlashCommand;
use eyre::Result;
use time::OffsetDateTime;
use twilight_interactions::command::{CommandModel, CreateCommand};
use twilight_model::channel::message::{embed::EmbedField, Message};
use twilight_model::id::{marker::ChannelMarker, Id};
use std::time::Duration;

use crate::{
    core::{Context, ReplayStatus},
    util::{
        builder::{EmbedBuilder, MessageBuilder},
        interaction::InteractionCommand,
        InteractionCommandExt, // re-export path, not util::interaction
    },
};

#[derive(CreateCommand, CommandModel, SlashCommand)]
#[command(name = "queue")]
#[flags(SKIP_DEFER)]
/// Displays the current replay queue
pub struct Queue;

pub async fn build_queue_embed(ctx: &Context) -> EmbedBuilder {
    let queue_guard = ctx.replay_queue.queue.lock().await;
    let queue_guard: &std::collections::VecDeque<crate::core::replay_queue::ReplayData> = &*queue_guard;


    let status = *ctx.replay_queue.status.lock().await;

    let mut embed = EmbedBuilder::new()
        .title("Current queue")
        .timestamp(OffsetDateTime::now_utc());

    let mut iter = queue_guard.iter();
    if let Some(data) = iter.next() {
        let name = "Progress".to_owned();
        let value = format!(
            "**<@{user}>**\n`{map}`\n\
            Downloading: {downloading}\n\
            Rendering: {rendering}\n\
            Uploading: {uploading}",
            user = data.user,
            map = data.title.clone().unwrap_or_else(|| data.replay_name().into_owned()),
            downloading = match status {
                ReplayStatus::Waiting => ProcessStatus::Waiting,
                ReplayStatus::Downloading => ProcessStatus::Running(None),
                ReplayStatus::MapFound => ProcessStatus::MapFound,
                _ => ProcessStatus::Done,
            },
            rendering = match status {
                ReplayStatus::Waiting | ReplayStatus::Downloading | ReplayStatus::MapFound => ProcessStatus::Waiting,
                ReplayStatus::Rendering(p) => ProcessStatus::Running(Some(p)),
                _ => ProcessStatus::Done,
            },
            uploading = match status {
                ReplayStatus::Uploading(secs) if secs > 0 => {
                    ProcessStatus::WaitingForCache(secs)  // reuse Running for display
                },
                ReplayStatus::Uploading(_) => ProcessStatus::Waiting,
                _ => ProcessStatus::Waiting,
            },

        );

        let mut fields = vec![EmbedField {
            inline: false,
            name,
            value,
        }];


        if let Some(data) = iter.next() {
            let name = "Upcoming".to_owned();
            let mut value = String::with_capacity(128);
            let _ = writeln!(value, "`2.` <@{}>: {}", data.user, data.title.clone().unwrap_or_else(|| data.replay_name().into_owned()));
            for (data, idx) in iter.zip(3..) {
                let _ = writeln!(value, "`{idx}.` <@{}>: {}", data.user, data.title.clone().unwrap_or_else(|| data.replay_name().into_owned()));
            }
            fields.push(EmbedField { inline: false, name, value });
        }

        embed = embed.fields(fields);
    } else {
        embed = embed.description("The queue is empty");
    }

    embed
}

pub async fn send_queue_status(ctx: Arc<Context>, channel_id: Id<ChannelMarker>) -> Result<()> {

    let embed = build_queue_embed(&ctx).await.build();

    let msg: Message = ctx
        .http
        .create_message(channel_id)
        .embeds(&[embed])
        .await?
        .model()
        .await?;

    let message_id = msg.id;

    tokio::spawn(async move {
        let notify = Arc::clone(&ctx.replay_queue.notify);

        loop {
            // Wake on any status change, or after 5s as a fallback
            tokio::time::timeout(
                Duration::from_secs(5),
                notify.notified(),
            ).await.ok();

            // Read current state
            let (is_empty, status) = {
                let is_empty = {
                    let q = ctx.replay_queue.queue.lock().await;
                    let q: &std::collections::VecDeque<crate::core::ReplayData> = &*q;
                    q.is_empty()
                };
                let status = *ctx.replay_queue.status.lock().await;
                (is_empty, status)
            };

            let embed = build_queue_embed(&ctx).await.build();
            let _ = ctx
                .http
                .update_message(channel_id, message_id)
                .embeds(Some(&[embed]))
                .await;

            // Stop only after pop has completed: queue empty and status is Waiting
            if is_empty && matches!(status, ReplayStatus::Waiting) {
                let _ = ctx.http.delete_message(channel_id, message_id).await;
                break;
            }

        }
    });




    Ok(())
}

async fn slash_queue(ctx: Arc<Context>, command: InteractionCommand) -> Result<()> {
    let embed = build_queue_embed(&ctx).await;
    let builder = MessageBuilder::new().embed(embed);
    command.callback(&ctx, builder, false).await?;

    let is_active = {
        let is_empty = {
            let q = ctx.replay_queue.queue.lock().await;
            let q: &std::collections::VecDeque<crate::core::ReplayData> = &*q;
            q.is_empty()
        };
        let status = *ctx.replay_queue.status.lock().await;
        !is_empty || !matches!(status, ReplayStatus::Waiting)
    };

    if !is_active {
        return Ok(());
    }

    // Fix 1: interaction() is a method call, not a field
    // Fix 2: explicit Message type annotation to help inference
    let msg: Message = ctx
        .interaction()
        .response(&command.token)
        .await?
        .model()
        .await?;

    let channel_id = msg.channel_id;
    let message_id = msg.id;


    tokio::spawn(async move {
        let notify = Arc::clone(&ctx.replay_queue.notify);

        loop {
            // Wake on any status change, or after 5s as a fallback
            tokio::time::timeout(
                Duration::from_secs(5),
                notify.notified(),
            ).await.ok();

            // Read current state
            let (is_empty, status) = {
                let is_empty = {
                    let q = ctx.replay_queue.queue.lock().await;
                    let q: &std::collections::VecDeque<crate::core::ReplayData> = &*q;
                    q.is_empty()
                };
                let status = *ctx.replay_queue.status.lock().await;
                (is_empty, status)
            };

            let embed = build_queue_embed(&ctx).await.build();
            let _ = ctx
                .http
                .update_message(channel_id, message_id)
                .embeds(Some(&[embed]))
                .await;
            // Stop only after pop has completed: queue empty and status is Waiting
            if is_empty && matches!(status, ReplayStatus::Waiting) {
                let _ = ctx.http.delete_message(channel_id, message_id).await;
                break;
            }

        }
    });




    Ok(())
}

enum ProcessStatus {
    Done,
    Running(Option<u8>),
    Waiting,
    MapFound,
    WaitingForCache(u64),
}

impl Display for ProcessStatus {
    #[inline]
    // Fix 4: explicit lifetime on Formatter
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            ProcessStatus::Done => write!(f, "✅"),
            ProcessStatus::Running(Some(progress)) => write!(f, "🏃 {progress}%"),
            ProcessStatus::Running(None) => write!(f, "🏃"),
            ProcessStatus::WaitingForCache(secs) => write!(f, "⏳ {}s", secs),
            ProcessStatus::Waiting => write!(f, "🛜"),
            ProcessStatus::MapFound => write!(f, "⬇️"),
        }
    }
}
