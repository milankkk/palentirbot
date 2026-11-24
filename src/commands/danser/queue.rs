use std::{
    borrow::Cow,
    fmt::{Display, Formatter, Result as FmtResult, Write},
    sync::Arc,
};

use command_macros::SlashCommand;
use eyre::Result;
use std::time::Duration;
use time::OffsetDateTime;
use twilight_interactions::command::{CommandModel, CreateCommand};
use twilight_model::channel::message::{
    embed::{Embed, EmbedField},
    Message,
};
use twilight_model::id::{
    marker::{ChannelMarker, MessageMarker},
    Id,
};

use crate::{
    core::{Context, ReplayData, ReplayStatus},
    util::{
        builder::{EmbedBuilder, MessageBuilder},
        interaction::InteractionCommand,
        str::truncate_chars,
        InteractionCommandExt, // re-export path, not util::interaction
    },
};

#[derive(CreateCommand, CommandModel, SlashCommand)]
#[command(name = "queue")]
#[flags(SKIP_DEFER)]
/// Displays the current replay queue
pub struct Queue;

fn entry_display_label(entry: &ReplayData, max_chars: usize) -> Cow<'_, str> {
    match entry.title.as_deref() {
        Some(t) => Cow::Borrowed(truncate_chars(t, max_chars)),
        None => {
            let name = entry.replay_name();
            let len = truncate_chars(&name, max_chars).len();
            let mut owned = name;
            owned.truncate(len);
            Cow::Owned(owned)
        }
    }
}

pub async fn build_queue_embed(
    ctx: &Context,
    viewer_channel: Id<ChannelMarker>,
) -> EmbedBuilder {
    build_queue_embed_with_scope(ctx, viewer_channel).await.0
}

async fn build_queue_embed_with_scope(
    ctx: &Context,
    viewer_channel: Id<ChannelMarker>,
) -> (EmbedBuilder, bool) {
    let viewer_guild = ctx
        .cache
        .channel(viewer_channel, |c| c.guild_id)
        .ok()
        .flatten();
    let same_scope = |entry_channel: Id<ChannelMarker>| -> bool {
        if entry_channel == viewer_channel {
            return true;
        }
        let Some(vg) = viewer_guild else {
            return false;
        };
        ctx.cache
            .channel(entry_channel, |c| c.guild_id)
            .ok()
            .flatten()
            == Some(vg)
    };

    let queue_guard = ctx.replay_queue.queue.lock().await;
    let status = *ctx.replay_queue.status.lock().await;

    let mut embed = EmbedBuilder::new()
        .title("Current queue")
        .timestamp(OffsetDateTime::now_utc());

    let visible_count = queue_guard
        .iter()
        .filter(|entry| same_scope(entry.input_channel))
        .count();

    if queue_guard.is_empty() {
        return (embed.description("The queue is empty"), false);
    }
    if visible_count == 0 {
        return (embed.description("No replays queued for this server."), false);
    }

    let mut iter = queue_guard.iter();
    if let Some(data) = iter.next() {
        let mut fields = Vec::with_capacity(2);
        if same_scope(data.input_channel) {
            let map = entry_display_label(data, 250);
            let value = format!(
                "**<@{user}>**\n`{map}`\n\
                Downloading: {downloading}\n\
                Rendering: {rendering}\n\
                Uploading: {uploading}",
                user = data.user,
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
            fields.push(EmbedField {
                inline: false,
                name: "Progress".to_owned(),
                value,
            });
        } else {
            let msg = if data.web.is_some() {
                "A web render is currently processing."
            } else {
                "Another server's replay is currently processing."
            };
            fields.push(EmbedField {
                inline: false,
                name: "Progress".to_owned(),
                value: msg.to_owned(),
            });
        }

        let mut upcoming_val = String::with_capacity(128);
        const MAX_FIELD_LEN: usize = 980;
        let mut remaining = 0usize;

        for (entry, pos) in iter.zip(2usize..) {
            if !same_scope(entry.input_channel) {
                continue;
            }
            let title = entry_display_label(entry, 120);
            let line = format!("`#{pos}` <@{}>: {title}\n", entry.user);
            if upcoming_val.len() + line.len() > MAX_FIELD_LEN {
                remaining += 1;
            } else {
                upcoming_val.push_str(&line);
            }
        }
        if remaining > 0 {
            let _ = write!(upcoming_val, "...and {remaining} more");
        }
        if !upcoming_val.is_empty() {
            fields.push(EmbedField {
                inline: false,
                name: "Upcoming".to_owned(),
                value: upcoming_val,
            });
        }

        embed = embed.fields(fields);
    }

    (embed, true)
}

fn is_terminal_discord_error(err: &twilight_http::Error) -> bool {
    matches!(
        err.kind(),
        twilight_http::error::ErrorType::Response { status, .. }
            if matches!(status.get(), 403 | 404)
    )
}

struct UpdaterGuard {
    ctx: Arc<Context>,
    channel_id: Id<ChannelMarker>,
    armed: bool,
}

impl UpdaterGuard {
    fn new(ctx: Arc<Context>, channel_id: Id<ChannelMarker>) -> Self {
        Self {
            ctx,
            channel_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for UpdaterGuard {
    fn drop(&mut self) {
        if self.armed {
            self.ctx.replay_queue.finish_channel_updater(self.channel_id);
        }
    }
}

pub async fn send_queue_status(ctx: Arc<Context>, channel_id: Id<ChannelMarker>) -> Result<()> {
    if !ctx.replay_queue.try_start_channel_updater(channel_id) {
        return Ok(());
    }
    let mut guard = UpdaterGuard::new(Arc::clone(&ctx), channel_id);
    if ctx.replay_queue.finish_channel_updater_if_idle(channel_id).await {
        guard.disarm();
        return Ok(());
    }

    let embed = build_queue_embed(&ctx, channel_id).await.build();

    let msg: Message = ctx
        .http
        .create_message(channel_id)
        .embeds(&[embed])
        .await?
        .model()
        .await?;

    let message_id = msg.id;
    spawn_channel_queue_updater(ctx, channel_id, message_id, guard);
    Ok(())
}

async fn update_queue_embed_msg(
    ctx: &Context,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
    embed: Embed,
) -> bool {
    match ctx
        .http
        .update_message(channel_id, message_id)
        .embeds(Some(&[embed]))
        .await
    {
        Ok(_) => true,
        Err(err) => !is_terminal_discord_error(&err),
    }
}

fn spawn_channel_queue_updater(
    ctx: Arc<Context>,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
    mut guard: UpdaterGuard,
) {
    tokio::spawn(async move {
        const MIN_EDIT_INTERVAL: Duration = Duration::from_millis(1500);
        let notify = Arc::clone(&ctx.replay_queue.notify);
        let mut last_edit = tokio::time::Instant::now() - MIN_EDIT_INTERVAL;

        loop {
            let _ = tokio::time::timeout(Duration::from_secs(3), notify.notified()).await;

            let elapsed = last_edit.elapsed();
            if elapsed < MIN_EDIT_INTERVAL {
                tokio::time::sleep(MIN_EDIT_INTERVAL - elapsed).await;
            }

            if ctx.replay_queue.finish_channel_updater_if_idle(channel_id).await {
                guard.disarm();
                let _ = ctx.http.delete_message(channel_id, message_id).await;
                break;
            }

            let embed = build_queue_embed(&ctx, channel_id).await.build();
            if !update_queue_embed_msg(&ctx, channel_id, message_id, embed).await {
                break;
            }
            last_edit = tokio::time::Instant::now();
        }
    });
}

fn spawn_slash_queue_updater(
    ctx: Arc<Context>,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
) {
    tokio::spawn(async move {
        const MIN_EDIT_INTERVAL: Duration = Duration::from_millis(1500);
        const MAX_WATCH_DURATION: Duration = Duration::from_secs(300);
        let notify = Arc::clone(&ctx.replay_queue.notify);
        let deadline = tokio::time::Instant::now() + MAX_WATCH_DURATION;
        let mut last_edit = tokio::time::Instant::now() - MIN_EDIT_INTERVAL;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let wait = remaining.min(Duration::from_secs(3));
            let _ = tokio::time::timeout(wait, notify.notified()).await;

            let elapsed = last_edit.elapsed();
            if elapsed < MIN_EDIT_INTERVAL {
                let step = (MIN_EDIT_INTERVAL - elapsed)
                    .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
                if !step.is_zero() {
                    tokio::time::sleep(step).await;
                }
            }

            let (embed, still_visible) = build_queue_embed_with_scope(&ctx, channel_id).await;
            if !update_queue_embed_msg(&ctx, channel_id, message_id, embed.build()).await
                || !still_visible
            {
                break;
            }
            last_edit = tokio::time::Instant::now();
        }
    });
}

async fn slash_queue(ctx: Arc<Context>, command: InteractionCommand) -> Result<()> {
    let (embed, has_visible) = build_queue_embed_with_scope(&ctx, command.channel_id).await;
    let builder = MessageBuilder::new().embed(embed);
    command.callback(&ctx, builder, false).await?;

    if !has_visible {
        return Ok(());
    }

    let msg: Message = ctx
        .interaction()
        .response(&command.token)
        .await?
        .model()
        .await?;

    spawn_slash_queue_updater(ctx, msg.channel_id, msg.id);

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
