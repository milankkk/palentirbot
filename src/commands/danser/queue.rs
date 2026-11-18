use std::{
    fmt::{Display, Formatter, Result as FmtResult, Write},
    sync::Arc,
};

use command_macros::SlashCommand;
use eyre::Result;
use std::time::Duration;
use time::OffsetDateTime;
use twilight_interactions::command::{CommandModel, CreateCommand};
use twilight_model::channel::message::{embed::EmbedField, Message};
use twilight_model::id::{
    marker::{ChannelMarker, MessageMarker},
    Id,
};

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
    let queue_guard: &std::collections::VecDeque<crate::core::replay_queue::ReplayData> =
        &queue_guard;

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
            map = data
                .title
                .clone()
                .unwrap_or_else(|| data.replay_name().into_owned()),
            downloading = match status {
                ReplayStatus::Waiting => ProcessStatus::Waiting,
                ReplayStatus::Downloading => ProcessStatus::Running(None),
                ReplayStatus::MapFound => ProcessStatus::MapFound,
                _ => ProcessStatus::Done,
            },
            rendering = match status {
                ReplayStatus::Waiting | ReplayStatus::Downloading | ReplayStatus::MapFound =>
                    ProcessStatus::Waiting,
                ReplayStatus::Rendering(p) => ProcessStatus::Running(Some(p)),
                _ => ProcessStatus::Done,
            },
            uploading = match status {
                ReplayStatus::Uploading => ProcessStatus::Running(None),
                _ => ProcessStatus::Waiting,
            },
        );

        let mut fields = vec![EmbedField {
            inline: false,
            name,
            value,
        }];

        if iter.len() > 0 {
            let name = "Upcoming".to_owned();
            let mut value = String::with_capacity(128);
            const MAX_FIELD_LEN: usize = 980;
            let mut remaining = 0usize;

            for (data, idx) in iter.zip(2..) {
                let title = data
                    .title
                    .clone()
                    .unwrap_or_else(|| data.replay_name().into_owned());
                let line = format!("`{idx}.` <@{}>: {title}\n", data.user);
                if value.len() + line.len() > MAX_FIELD_LEN {
                    remaining += 1;
                } else {
                    value.push_str(&line);
                }
            }
            if remaining > 0 {
                let _ = write!(value, "...and {remaining} more");
            }
            if !value.is_empty() {
                fields.push(EmbedField {
                    inline: false,
                    name,
                    value,
                });
            }
        }

        embed = embed.fields(fields);
    } else {
        embed = embed.description("The queue is empty");
    }

    embed
}

struct UpdaterGuard(Arc<Context>);

impl Drop for UpdaterGuard {
    fn drop(&mut self) {
        self.0.replay_queue.finish_updater();
    }
}

pub async fn send_queue_status(ctx: Arc<Context>, channel_id: Id<ChannelMarker>) -> Result<()> {
    if !ctx.replay_queue.try_start_updater() {
        return Ok(());
    }
    let guard = UpdaterGuard(Arc::clone(&ctx));
    let embed = build_queue_embed(&ctx).await.build();

    let msg: Message = ctx
        .http
        .create_message(channel_id)
        .embeds(&[embed])
        .await?
        .model()
        .await?;

    let message_id = msg.id;
    spawn_queue_updater_with_guard(ctx, channel_id, message_id, guard);
    Ok(())
}

pub fn spawn_queue_updater(
    ctx: Arc<Context>,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
) {
    if !ctx.replay_queue.try_start_updater() {
        return;
    }
    let guard = UpdaterGuard(Arc::clone(&ctx));
    spawn_queue_updater_with_guard(ctx, channel_id, message_id, guard);
}

fn spawn_queue_updater_with_guard(
    ctx: Arc<Context>,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
    guard: UpdaterGuard,
) {
    tokio::spawn(async move {
        let _guard = guard;
        const MIN_EDIT_INTERVAL: Duration = Duration::from_millis(1500);
        let notify = Arc::clone(&ctx.replay_queue.notify);
        let mut last_edit = tokio::time::Instant::now() - MIN_EDIT_INTERVAL;

        loop {
            // Wait for a status notification or wake periodically up to 3s
            let _ = tokio::time::timeout(Duration::from_secs(3), notify.notified()).await;

            // Read current state
            let (is_empty, status) = {
                let is_empty = {
                    let q = ctx.replay_queue.queue.lock().await;
                    let q: &std::collections::VecDeque<crate::core::ReplayData> = &q;
                    q.is_empty()
                };
                let status = *ctx.replay_queue.status.lock().await;
                (is_empty, status)
            };

            // Stop only after pop has completed: queue empty and status is Waiting
            if is_empty && matches!(status, ReplayStatus::Waiting) {
                let _ = ctx.http.delete_message(channel_id, message_id).await;
                break;
            }

            let elapsed = last_edit.elapsed();
            if elapsed < MIN_EDIT_INTERVAL {
                tokio::time::sleep(MIN_EDIT_INTERVAL - elapsed).await;
                let is_now_empty = ctx.replay_queue.queue.lock().await.is_empty();
                let now_status = *ctx.replay_queue.status.lock().await;
                if is_now_empty && matches!(now_status, ReplayStatus::Waiting) {
                    let _ = ctx.http.delete_message(channel_id, message_id).await;
                    break;
                }
            }

            let embed = build_queue_embed(&ctx).await.build();
            let _ = ctx
                .http
                .update_message(channel_id, message_id)
                .embeds(Some(&[embed]))
                .await;
            last_edit = tokio::time::Instant::now();
        }
    });
}

async fn slash_queue(ctx: Arc<Context>, command: InteractionCommand) -> Result<()> {
    let embed = build_queue_embed(&ctx).await;
    let builder = MessageBuilder::new().embed(embed);
    command.callback(&ctx, builder, false).await?;

    let is_active = ctx.replay_queue.is_active().await;

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

    spawn_queue_updater(ctx, channel_id, message_id);

    Ok(())
}

enum ProcessStatus {
    Done,
    Running(Option<u8>),
    Waiting,
    MapFound,
}

impl Display for ProcessStatus {
    #[inline]
    // Fix 4: explicit lifetime on Formatter
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            ProcessStatus::Done => write!(f, "✅"),
            ProcessStatus::Running(Some(progress)) => write!(f, "🏃 {progress}%"),
            ProcessStatus::Running(None) => write!(f, "🏃"),
            ProcessStatus::Waiting => write!(f, "🛜"),
            ProcessStatus::MapFound => write!(f, "⬇️"),
        }
    }
}
