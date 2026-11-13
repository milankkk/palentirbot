use std::{collections::HashMap, sync::Arc, time::Duration};

use command_macros::SlashCommand;
use eyre::Result;
use tokio::sync::mpsc::{self, UnboundedSender};
use twilight_interactions::command::{CommandModel, CommandOption, CreateCommand, CreateOption};
use twilight_model::{
    channel::{
        message::{embed::Embed, MessageFlags},
        Attachment, Message,
    },
    guild::Permissions,
    id::{
        marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker},
        Id,
    },
};

use crate::{
    core::{
        ai::{
            base64_encode, describe_ai_error, sanitize_llm_text, AiAttachmentFile, AiMessage,
            AiPersonaMode, AiStreamEvent, SearchMode,
        },
        Context,
    },
    util::{
        builder::MessageBuilder, interaction::InteractionCommand, Authored, ChannelExt,
        InteractionCommandExt, MessageExt,
    },
};

/// Default number of previous Discord messages sent to the AI.
const DEFAULT_HISTORY_LIMIT: u16 = 25;
/// Hard ceiling for history messages.
const MAX_HISTORY_LIMIT: u16 = 50;
/// Maximum number of characters taken from a single historical message.
const HISTORY_MESSAGE_CHAR_LIMIT: usize = 1_500;
/// Max image size (8 MB) and count (3) per request for local vision VLM.
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_IMAGES_PER_REQUEST: usize = 3;
/// Max uploaded text/code file size (512 KB) and count (3) per request.
const MAX_TEXT_FILE_BYTES: u64 = 512 * 1024;
const MAX_TEXT_FILES_PER_REQUEST: usize = 3;
/// Keep some room below Discord's 2,000-character message limit.
const DISCORD_RESPONSE_CHAR_LIMIT: usize = 1_900;
const MAX_DISCORD_CHUNKS: usize = 5;

#[derive(CommandModel, CreateCommand, SlashCommand)]
#[command(name = "ask")]
#[flags(SKIP_DEFER)]
/// Ask the AI a question
pub struct Ask {
    /// What you want to ask
    question: String,
    /// Optional image attachment for vision analysis
    image: Option<Attachment>,
    /// Optional text, code, log, or config file to include
    file: Option<Attachment>,
    /// Keep the reply private to you (ephemeral)
    ephemeral: Option<bool>,
    /// Override web search behavior for this question
    search: Option<SlashSearchMode>,
}

#[derive(Clone, Copy, CommandOption, CreateOption)]
pub enum SlashSearchMode {
    #[option(name = "Auto (model decides)", value = "auto")]
    Auto,
    #[option(name = "Force web search", value = "force")]
    Force,
    #[option(name = "Off (local knowledge only)", value = "off")]
    Off,
}

impl From<SlashSearchMode> for SearchMode {
    fn from(mode: SlashSearchMode) -> Self {
        match mode {
            SlashSearchMode::Auto => Self::Auto,
            SlashSearchMode::Force => Self::Force,
            SlashSearchMode::Off => Self::Off,
        }
    }
}

pub struct AttachedTextFile {
    pub filename: String,
    pub content: String,
    pub truncated: bool,
}

pub struct AiQuestionOutput {
    pub chunks: Vec<String>,
    pub attachments: Vec<AiAttachmentFile>,
}

pub struct AiQuestionOptions<'a> {
    pub exclude_message_id: Option<Id<MessageMarker>>,
    pub referenced_msg: Option<&'a Message>,
    pub images: Vec<String>,
    pub text_files: Vec<AttachedTextFile>,
    pub search_mode: SearchMode,
    pub stream_tx: Option<UnboundedSender<AiStreamEvent>>,
}

impl Default for AiQuestionOptions<'_> {
    fn default() -> Self {
        Self {
            exclude_message_id: None,
            referenced_msg: None,
            images: Vec::new(),
            text_files: Vec::new(),
            search_mode: SearchMode::Auto,
            stream_tx: None,
        }
    }
}

enum StreamTarget {
    Message {
        ctx: Arc<Context>,
        msg_id: Id<MessageMarker>,
        channel_id: Id<ChannelMarker>,
    },
    Interaction {
        ctx: Arc<Context>,
        token: String,
    },
}

enum StreamPhase {
    Status(String),
    Thinking { chars: usize, tail: String },
    Content(String),
}

struct StreamUiState {
    started_at: tokio::time::Instant,
    last_edit_at: Option<tokio::time::Instant>,
    last_sent: String,
    phase: Option<StreamPhase>,
    dirty_urgent: bool,
}

impl StreamUiState {
    fn new(initial_sent: String) -> Self {
        Self {
            started_at: tokio::time::Instant::now(),
            last_edit_at: (!initial_sent.is_empty()).then(tokio::time::Instant::now),
            last_sent: initial_sent,
            phase: None,
            dirty_urgent: false,
        }
    }

    fn apply_event(&mut self, ev: AiStreamEvent) {
        match ev {
            AiStreamEvent::Status(status) => {
                self.phase = Some(StreamPhase::Status(status));
                self.dirty_urgent = true;
            }
            AiStreamEvent::Thinking { chars, tail } => {
                let first_think = !matches!(self.phase, Some(StreamPhase::Thinking { .. }));
                self.phase = Some(StreamPhase::Thinking { chars, tail });
                if first_think {
                    self.dirty_urgent = true;
                }
            }
            AiStreamEvent::Content(raw) => {
                let preview = format_streaming_preview(&raw);
                if !preview.is_empty() {
                    let first_content = !matches!(self.phase, Some(StreamPhase::Content(_)));
                    self.phase = Some(StreamPhase::Content(preview));
                    if first_content {
                        self.dirty_urgent = true;
                    }
                }
            }
        }
    }

    fn render(&self, elapsed_secs: u64) -> String {
        let Some(ref phase) = self.phase else {
            return String::new();
        };
        match phase {
            StreamPhase::Status(status) => {
                if elapsed_secs >= 2 {
                    format!("{status} ({elapsed_secs}s)")
                } else {
                    status.clone()
                }
            }
            StreamPhase::Thinking { chars, tail } => {
                let mut text = format!("🧠 **Thinking...** ({elapsed_secs}s · {chars} chars)");
                let clean_tail = tail.trim();
                if !clean_tail.is_empty() {
                    let safe_tail = truncate_chars(clean_tail, 160);
                    text.push_str("\n> *");
                    text.push_str(&safe_tail);
                    text.push('*');
                }
                text
            }
            StreamPhase::Content(preview) => preview.clone(),
        }
    }

    fn should_flush_urgent(&self, cooldown: Duration) -> bool {
        self.dirty_urgent
            && self
                .last_edit_at
                .is_none_or(|t| t.elapsed() >= cooldown)
    }

    fn next_changed_text(&mut self) -> Option<String> {
        let rendered = self.render(self.started_at.elapsed().as_secs());
        if rendered.is_empty() || rendered == self.last_sent {
            return None;
        }
        self.last_sent = rendered.clone();
        self.last_edit_at = Some(tokio::time::Instant::now());
        self.dirty_urgent = false;
        Some(rendered)
    }
}

fn format_streaming_preview(raw: &str) -> String {
    let cleaned = clean_ai_response(raw);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let truncated = truncate_chars(trimmed, 1_940);
    format!("{truncated} ▌")
}

async fn send_stream_edit(target: &StreamTarget, content: &str) {
    match target {
        StreamTarget::Message {
            ctx,
            msg_id,
            channel_id,
        } => {
            let _ = ctx
                .http
                .update_message(*channel_id, *msg_id)
                .content(Some(content))
                .await;
        }
        StreamTarget::Interaction { ctx, token } => {
            let client = ctx.interaction();
            let _ = client
                .update_response(token)
                .content(Some(content))
                .await;
        }
    }
}

fn spawn_stream_updater(
    target: StreamTarget,
    initial_sent: impl Into<String>,
) -> (UnboundedSender<AiStreamEvent>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let initial_sent = initial_sent.into();
    let handle = tokio::spawn(async move {
        const PERIODIC_TICK: Duration = Duration::from_millis(1_100);
        const MIN_URGENT_COOLDOWN: Duration = Duration::from_millis(450);

        let mut tick = tokio::time::interval(PERIODIC_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await;

        let mut ui = StreamUiState::new(initial_sent);

        loop {
            tokio::select! {
                maybe_ev = rx.recv() => {
                    match maybe_ev {
                        Some(ev) => {
                            ui.apply_event(ev);
                            while let Ok(next_ev) = rx.try_recv() {
                                ui.apply_event(next_ev);
                            }
                            if ui.should_flush_urgent(MIN_URGENT_COOLDOWN) {
                                if let Some(text) = ui.next_changed_text() {
                                    send_stream_edit(&target, &text).await;
                                }
                            }
                        }
                        None => break,
                    }
                }
                _ = tick.tick() => {
                    while let Ok(ev) = rx.try_recv() {
                        ui.apply_event(ev);
                    }
                    if let Some(text) = ui.next_changed_text() {
                        send_stream_edit(&target, &text).await;
                    }
                }
            }
        }
    });
    (tx, handle)
}

/// Remove `<@bot_id>` / `<@!bot_id>` mentions if present in content, returning `None` if unmentioned.
pub fn strip_bot_mention(content: &str, bot_id: Id<UserMarker>) -> Option<String> {
    let plain = format!("<@{bot_id}>");
    let nick = format!("<@!{bot_id}>");
    if !content.contains(&plain) && !content.contains(&nick) {
        return None;
    }
    let cleaned = content.replace(&plain, "").replace(&nick, "");
    Some(cleaned.trim().to_owned())
}

fn resolve_discord_mentions(
    content: &str,
    mentions: &[twilight_model::channel::message::Mention],
) -> String {
    if mentions.is_empty() || !content.contains("<@") {
        return content.to_owned();
    }
    let mut out = content.to_owned();
    for user in mentions {
        let name = sanitize_llm_text(user.name.trim());
        if name.is_empty() {
            continue;
        }
        let replacement = format!("@{name} (Discord user)");
        out = out
            .replace(&format!("<@{}>", user.id), &replacement)
            .replace(&format!("<@!{}>", user.id), &replacement);
    }
    out
}

async fn slash_ask(ctx: Arc<Context>, mut command: InteractionCommand) -> Result<()> {
    let Ask {
        question,
        image,
        file,
        ephemeral,
        search,
    } = Ask::from_interaction(command.input_data())?;
    let is_ephemeral = ephemeral.unwrap_or(false);

    command.defer(&ctx, is_ephemeral).await?;

    let images = if let Some(ref att) = image {
        collect_images_from_attachments(&ctx, std::slice::from_ref(att)).await
    } else {
        Vec::new()
    };

    if image.is_some() && images.is_empty() {
        command
            .error(&ctx, "Supported images: PNG/JPEG/WebP up to 8 MB.")
            .await?;
        return Ok(());
    }

    let text_files = if let Some(ref att) = file {
        collect_text_files_from_attachments(
            &ctx,
            std::slice::from_ref(att),
            max_attached_text_chars(ctx.ai().num_ctx()),
        )
        .await
    } else {
        Vec::new()
    };

    if file.is_some() && text_files.is_empty() {
        command
            .error(
                &ctx,
                "Supported files: UTF-8 text, logs, markdown, configs, or source code up to 512 KB (use `image` for PNG/JPEG/WebP).",
            )
            .await?;
        return Ok(());
    }

    let attached_file_names: Vec<String> = text_files.iter().map(|f| f.filename.clone()).collect();

    // Enter the fair FIFO AI queue to avoid overloading the backend
    let ticket = match ctx.ai().queue().enter().await {
        Ok(ticket) => ticket,
        Err(len) => {
            let content = format!(
                "⚠️ The AI queue is currently full ({len} requests in line). Please try again in a few moments."
            );
            let builder = MessageBuilder::new().content(content);
            command.update(&ctx, &builder).await?;
            return Ok(());
        }
    };

    let position = ticket.position();
    if position > 0 {
        let content = format!("⏳ Waiting in AI queue (position #{position})...");
        let builder = MessageBuilder::new().content(content);
        let _ = command.update(&ctx, &builder).await;
    }

    // Wait until it is this request's turn
    let _permit = ticket.wait().await;

    // Get the person currently talking to the AI.
    let user = command.user()?;

    let (stream_tx, updater_handle) = spawn_stream_updater(
        StreamTarget::Interaction {
            ctx: Arc::clone(&ctx),
            token: command.token.clone(),
        },
        String::new(),
    );

    let res = execute_ai_question_with_options(
        &ctx,
        command.guild_id,
        command.channel_id,
        user.id,
        &user.name,
        &question,
        AiQuestionOptions {
            images,
            text_files,
            search_mode: search.map(Into::into).unwrap_or(SearchMode::Auto),
            stream_tx: Some(stream_tx),
            ..Default::default()
        },
    )
    .await;

    let _ = updater_handle.await;

    let output = match res {
        Ok(output) => output,
        Err(err) => {
            tracing::error!(
                ?err,
                user_id = %user.id,
                "AI request failed"
            );

            let builder = MessageBuilder::new().content(describe_ai_error(&err));

            command.update(&ctx, &builder).await?;

            return Err(err.wrap_err("AI request failed"));
        }
    };

    let attachment_pairs: Vec<(String, Vec<u8>)> = output
        .attachments
        .into_iter()
        .map(|a| (a.filename, a.bytes))
        .collect();
    let mut iter = output.chunks.into_iter();
    if let Some(first) = iter.next() {
        let mut builder = MessageBuilder::new().content(first);
        if !attachment_pairs.is_empty() {
            builder = builder.attachments(attachment_pairs);
        }
        let sent_msg = command.update(&ctx, &builder).await?;
        if !is_ephemeral {
            let prompt_text = truncate_history_message(question.trim());
            let author_name = sanitize_llm_text(&user.name);
            let clean_q = sanitize_llm_text(&prompt_text);
            let mut summary = String::new();
            if !attached_file_names.is_empty() {
                summary.push_str(&format!("[Attached file(s): {}] ", attached_file_names.join(", ")));
            }
            summary.push_str(&clean_q);
            if !summary.trim().is_empty() {
                ctx.ai()
                    .record_slash_turn(
                        sent_msg.id,
                        format!("{author_name}: {}", summary.trim()),
                    )
                    .await;
            }
        }
    }
    let interaction = ctx.interaction();
    for chunk in iter {
        let mut req = interaction.create_followup(&command.token).content(&chunk);
        if is_ephemeral {
            req = req.flags(MessageFlags::EPHEMERAL);
        }
        let _ = req.await;
    }

    Ok(())
}

/// Handle prefix `!ask` (or `!ai`, `!a`) messages.
pub async fn handle_prefix_ask(
    ctx: Arc<Context>,
    msg: &Message,
    args_str: &str,
    prefix: &str,
) {
    let mut question = resolve_discord_mentions(args_str.trim(), &msg.mentions);

    let bot_id = ctx.cache.current_user(|u| u.id).ok();
    if let Some(id) = bot_id {
        if !bot_has_channel_perms(
            &ctx,
            msg.channel_id,
            id,
            Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES,
        ) {
            tracing::debug!(
                channel_id = %msg.channel_id,
                "skipping prefix AI reply because bot lacks cached channel view/send permissions"
            );
            return;
        }
    }

    // Collect image and text/code attachments from both this message and any replied message
    let mut attachments: Vec<Attachment> = msg.attachments.clone();
    if let Some(replied) = msg.referenced_message.as_deref() {
        attachments.extend(replied.attachments.iter().cloned());
    }
    let images = collect_images_from_attachments(&ctx, &attachments).await;
    let text_files = collect_text_files_from_attachments(
        &ctx,
        &attachments,
        max_attached_text_chars(ctx.ai().num_ctx()),
    )
    .await;

    let is_reply_to_ai = msg
        .referenced_message
        .as_deref()
        .map(|r| Some(r.author.id) == bot_id && is_conversational_ai_message(r))
        .unwrap_or(false);

    // If the message is a reply to another message/embed (not the AI), include that as context
    if let Some(ref replied) = msg.referenced_message {
        if !is_reply_to_ai {
            let ref_author = sanitize_llm_text(&replied.author.name);
            let ref_content = format_message_text_and_embeds(replied);
            if !ref_content.is_empty() {
                if question.is_empty() {
                    question = format!("What do you think about what {ref_author} posted: \"{ref_content}\"?");
                } else {
                    question = format!("(In reply to {ref_author}: \"{ref_content}\")\n{question}");
                }
            } else if !images.is_empty() && question.is_empty() {
                question = format!("What do you think about this image posted by {ref_author}?");
            }
        }
    }

    if question.is_empty() {
        if !images.is_empty() {
            question = "What do you see in this image?".to_string();
        } else if !text_files.is_empty() {
            question = "Please inspect the attached file(s).".to_string();
        } else if is_reply_to_ai {
            question = "What do you think about what you said?".to_string();
        } else {
            let _ = msg
                .error(
                    &ctx,
                    format!("Please provide a question! Example: `{prefix}ask Why is the sky blue?`"),
                )
                .await;
            return;
        }
    }

    // Enter the fair FIFO AI queue to avoid overloading the backend
    let ticket = match ctx.ai().queue().enter().await {
        Ok(ticket) => ticket,
        Err(len) => {
            let content = format!(
                "⚠️ The AI queue is currently full ({len} requests in line). Please try again in a few moments."
            );
            let _ = msg.error(&ctx, content).await;
            return;
        }
    };

    let position = ticket.position();
    let initial_content = if position > 0 {
        format!("⏳ Waiting in AI queue (position #{position})...")
    } else {
        "🧠 Starting AI request...".to_string()
    };

    let placeholder = msg
        .channel_id
        .create_message(
            &ctx,
            &MessageBuilder::new()
                .content(initial_content.clone())
                .reply(msg.id),
        )
        .await
        .ok();

    // Wait until it is this request's turn
    let _permit = ticket.wait().await;

    tracing::info!(
        user = %msg.author.name,
        user_id = %msg.author.id,
        channel_id = %msg.channel_id,
        "Handling prefix AI request"
    );

    let _ = ctx.http.create_typing_trigger(msg.channel_id).await;

    let (stream_tx, updater_handle) = placeholder
        .as_ref()
        .map(|ph| {
            spawn_stream_updater(
                StreamTarget::Message {
                    ctx: Arc::clone(&ctx),
                    msg_id: ph.id,
                    channel_id: ph.channel_id,
                },
                if position > 0 {
                    String::new()
                } else {
                    initial_content.clone()
                },
            )
        })
        .map_or((None, None), |(tx, h)| (Some(tx), Some(h)));

    let result = execute_ai_question_with_options(
        &ctx,
        msg.guild_id,
        msg.channel_id,
        msg.author.id,
        &msg.author.name,
        &question,
        AiQuestionOptions {
            exclude_message_id: Some(msg.id),
            referenced_msg: msg.referenced_message.as_deref(),
            images,
            text_files,
            search_mode: SearchMode::Auto,
            stream_tx,
        },
    )
    .await;

    if let Some(handle) = updater_handle {
        let _ = handle.await;
    }

    match result {
        Ok(output) => {
            let attachment_pairs: Vec<(String, Vec<u8>)> = output
                .attachments
                .into_iter()
                .map(|a| (a.filename, a.bytes))
                .collect();
            let mut iter = output.chunks.into_iter();
            if let Some(first) = iter.next() {
                let mut builder = MessageBuilder::new().content(first);
                if !attachment_pairs.is_empty() {
                    builder = builder.attachments(attachment_pairs);
                }
                let updated = if let Some(ref ph) = placeholder {
                    match ph.update(&ctx, &builder).await {
                        Ok(_) => true,
                        Err(err) => {
                            tracing::debug!(?err, "placeholder update failed; sending fresh reply");
                            false
                        }
                    }
                } else {
                    false
                };
                if !updated {
                    builder = builder.reply(msg.id);
                    let _ = msg.channel_id.create_message(&ctx, &builder).await;
                }
            }
            for chunk in iter {
                let _ = msg
                    .channel_id
                    .create_message(&ctx, &MessageBuilder::new().content(chunk))
                    .await;
            }
        }
        Err(err) => {
            tracing::error!(?err, user_id = %msg.author.id, "prefix AI request failed");
            let error_content = describe_ai_error(&err);
            if let Some(ref ph) = placeholder {
                let _ = ph
                    .update(&ctx, &MessageBuilder::new().content(error_content))
                    .await;
            } else {
                let _ = msg.error(&ctx, error_content).await;
            }
        }
    }
}

/// Check cached bot permissions when guild & member metadata are available.
/// Returns `true` when metadata is missing so uncached channels never fail closed.
pub(crate) fn bot_has_channel_perms(
    ctx: &Context,
    channel_id: Id<ChannelMarker>,
    bot_id: Id<UserMarker>,
    required: Permissions,
) -> bool {
    let Ok(Some(guild_id)) = ctx.cache.channel(channel_id, |c| c.guild_id) else {
        return true;
    };
    if ctx.cache.member(guild_id, bot_id, |_| ()).is_err()
        || ctx.cache.role(guild_id.cast(), |_| ()).is_err()
    {
        return true;
    }
    ctx.cache
        .get_channel_permissions(bot_id, channel_id, Some(guild_id))
        .contains(required)
}

pub(crate) fn configured_history_limit() -> u16 {
    std::env::var("AI_HISTORY_MESSAGES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .clamp(1, MAX_HISTORY_LIMIT)
}

fn is_supported_image_attachment(att: &Attachment) -> bool {
    if att.size > MAX_IMAGE_BYTES {
        return false;
    }
    if let Some(ref ct) = att.content_type {
        let lower = ct.to_ascii_lowercase();
        if lower == "image/png" || lower == "image/jpeg" || lower == "image/jpg" || lower == "image/webp" {
            return true;
        }
    }
    matches!(
        att.filename.rsplit('.').next().map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("png" | "jpg" | "jpeg" | "webp")
    )
}

pub(crate) async fn collect_images_from_attachments(
    ctx: &Context,
    attachments: &[Attachment],
) -> Vec<String> {
    let mut images = Vec::new();
    for att in attachments {
        if images.len() >= MAX_IMAGES_PER_REQUEST {
            break;
        }
        if !is_supported_image_attachment(att) {
            continue;
        }
        match ctx.client().get_discord_attachment(att).await {
            Ok(bytes) if (bytes.len() as u64) <= MAX_IMAGE_BYTES => {
                images.push(base64_encode(&bytes));
            }
            Ok(_) => {}
            Err(err) => {
                tracing::debug!(?err, filename = %att.filename, "failed to download image attachment for vision");
            }
        }
    }
    images
}

fn max_attached_text_chars(num_ctx: u32) -> usize {
    // Leave ample room for system prompt, chat history, tool outputs, and model generation.
    ((num_ctx as usize).saturating_sub(6_500) * 2).clamp(4_000, 32_000)
}

fn is_supported_text_attachment(att: &Attachment) -> bool {
    const TEXT_EXTS: &[&str] = &[
        "txt", "md", "markdown", "log", "csv", "tsv", "json", "jsonl", "yaml", "yml", "toml",
        "ini", "cfg", "conf", "xml", "html", "htm", "css", "scss", "js", "jsx", "mjs", "cjs",
        "ts", "tsx", "py", "pyi", "rs", "go", "c", "h", "cpp", "hpp", "cc", "java", "kt", "cs",
        "php", "rb", "sh", "bash", "zsh", "fish", "ps1", "sql", "diff", "patch", "env",
        "gitignore", "dockerfile", "makefile", "lua", "zig", "nix", "svelte", "vue", "astro",
        "prisma", "graphql", "tf", "hcl", "cmake", "hs", "ex", "exs", "erl", "clj", "scala",
        "swift", "dart", "r", "jl", "tex", "bib", "svg", "osu",
    ];

    if att.size == 0 || att.size > MAX_TEXT_FILE_BYTES {
        return false;
    }

    if let Some(ref ct) = att.content_type {
        let mime = ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if mime.starts_with("image/") || mime.starts_with("video/") || mime.starts_with("audio/") {
            return false;
        }
        if mime.starts_with("text/")
            || matches!(
                mime.as_str(),
                "application/json"
                    | "application/xml"
                    | "application/yaml"
                    | "application/x-yaml"
                    | "application/toml"
                    | "application/javascript"
                    | "application/x-javascript"
                    | "application/typescript"
                    | "application/x-sh"
                    | "application/sql"
                    | "application/xhtml+xml"
            )
        {
            return true;
        }
        // Only fall back to filename extensions for generic binary/unspecified MIME types.
        if mime != "application/octet-stream" && !mime.is_empty() {
            return false;
        }
    }

    let name = att.filename.trim();
    if let Some((_, ext)) = name.rsplit_once('.') {
        let lower = ext.to_ascii_lowercase();
        TEXT_EXTS.contains(&lower.as_str())
    } else {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "makefile" | "dockerfile" | "readme" | "license" | "changelog"
        )
    }
}

fn is_likely_plain_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.contains(&0) {
        return false;
    }
    let text = String::from_utf8_lossy(bytes);
    let total = text.chars().count();
    if total == 0 {
        return false;
    }
    let bad_controls = text
        .chars()
        .filter(|&c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        .count();
    bad_controls * 100 <= total
}

pub(crate) async fn collect_text_files_from_attachments(
    ctx: &Context,
    attachments: &[Attachment],
    max_total_chars: usize,
) -> Vec<AttachedTextFile> {
    let mut files = Vec::new();
    let mut used_chars = 0usize;

    for att in attachments {
        if files.len() >= MAX_TEXT_FILES_PER_REQUEST || used_chars >= max_total_chars {
            break;
        }
        if !is_supported_text_attachment(att) {
            continue;
        }
        let Ok(bytes) = ctx.client().get_discord_attachment(att).await else {
            tracing::debug!(filename = %att.filename, "failed to download text attachment for AI");
            continue;
        };
        if (bytes.len() as u64) > MAX_TEXT_FILE_BYTES || !is_likely_plain_text(&bytes) {
            continue;
        }

        let decoded = sanitize_llm_text(&String::from_utf8_lossy(&bytes));
        let remaining_budget = max_total_chars.saturating_sub(used_chars);
        let total_chars = decoded.chars().count();
        let (content, truncated) = if total_chars > remaining_budget {
            (truncate_chars(&decoded, remaining_budget), true)
        } else {
            (decoded, false)
        };
        used_chars += content.chars().count();

        let clean_name = sanitize_llm_text(att.filename.trim());
        files.push(AttachedTextFile {
            filename: if clean_name.is_empty() {
                "attachment.txt".to_owned()
            } else {
                clean_name
            },
            content,
            truncated,
        });
    }

    files
}

fn format_attached_files_section(files: &[AttachedTextFile]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for file in files {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let fence = if file.content.contains("```") {
            "~~~~"
        } else {
            "```"
        };
        out.push_str(&format!(
            "--- Attached File: `{}` ---\n{fence}\n{}\n{fence}",
            file.filename, file.content
        ));
        if file.truncated {
            out.push_str("\n[Note: file content was truncated to fit the context window]");
        }
    }
    out
}

/// Format a Discord embed concisely for LLM consumption (score cards, rich links, etc.).
pub fn format_embed_summary(embed: &Embed) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(ref author) = embed.author {
        let a = sanitize_llm_text(author.name.trim());
        let author_url = author
            .url
            .as_deref()
            .map(|u| sanitize_llm_text(u.trim()))
            .filter(|u| !u.is_empty());
        match (a.is_empty(), author_url) {
            (false, Some(u)) => parts.push(format!("Author: {a} ({u})")),
            (false, None) => parts.push(format!("Author: {a}")),
            (true, Some(u)) => parts.push(format!("Author URL: {u}")),
            (true, None) => {}
        }
    }
    if let Some(ref url) = embed.url {
        let u = sanitize_llm_text(url.trim());
        let dup_author_url = embed
            .author
            .as_ref()
            .and_then(|a| a.url.as_deref())
            .is_some_and(|au| au.trim() == url.trim());
        if !u.is_empty() && !dup_author_url {
            parts.push(format!("URL: {u}"));
        }
    }
    if let Some(ref title) = embed.title {
        let t = sanitize_llm_text(title.trim());
        if !t.is_empty() {
            parts.push(format!("Title: {t}"));
        }
    }
    if let Some(ref desc) = embed.description {
        let d = sanitize_llm_text(desc.trim());
        if !d.is_empty() {
            parts.push(truncate_chars(&d, 600));
        }
    }
    for field in embed.fields.iter().take(8) {
        let k = sanitize_llm_text(field.name.trim());
        let v = sanitize_llm_text(field.value.trim());
        if !k.is_empty() || !v.is_empty() {
            parts.push(format!("{k}: {v}"));
        }
    }
    if let Some(ref footer) = embed.footer {
        let f = sanitize_llm_text(footer.text.trim());
        if !f.is_empty() {
            parts.push(format!("Footer: {f}"));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(truncate_chars(&format!("[Embed: {}]", parts.join(" | ")), HISTORY_MESSAGE_CHAR_LIMIT))
    }
}

/// Render a Discord message's text plus any embed summaries into sanitized prompt text.
pub fn format_message_text_and_embeds(msg: &Message) -> String {
    let resolved_content = resolve_discord_mentions(&msg.content, &msg.mentions);
    let mut out = sanitize_llm_text(&truncate_history_message(&resolved_content));
    for embed in msg.embeds.iter().take(2) {
        if let Some(summary) = format_embed_summary(embed) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&summary);
        }
    }
    truncate_history_message(&out)
}

fn format_history_as_background_log(history: &[AiMessage]) -> String {
    let mut lines = Vec::new();
    for msg in history {
        let trimmed = msg.content.trim();
        if trimmed.is_empty() {
            continue;
        }
        if msg.role == "assistant" {
            let single_line = trimmed
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if !single_line.is_empty() {
                lines.push(format!("- Render Slave (you): {single_line}"));
            }
        } else {
            for line in trimmed.lines() {
                let l = line.trim();
                if !l.is_empty() {
                    lines.push(format!("- {l}"));
                }
            }
        }
    }
    lines.join("\n")
}

/// Core entry point accepting image attachments and search-mode overrides.
pub async fn execute_ai_question_with_options(
    ctx: &Context,
    guild_id: Option<Id<GuildMarker>>,
    channel_id: Id<ChannelMarker>,
    user_id: Id<UserMarker>,
    username: &str,
    question: &str,
    opts: AiQuestionOptions<'_>,
) -> Result<AiQuestionOutput> {
    let bot_id = ctx
        .cache
        .current_user(|user| user.id)
        .expect("missing CurrentUser in cache");

    let resolved_guild =
        guild_id.or_else(|| ctx.cache.channel(channel_id, |c| c.guild_id).ok().flatten());
    let guild_prefix = resolved_guild
        .and_then(|gid| ctx.guild_settings(gid, |s| s.effective_prefix().to_owned()));

    let system_prompt_template = ctx.ai().get_prompt().await;

    let clean_username = sanitize_llm_text(username);
    let clean_question = sanitize_llm_text(question);

    let mut system_prompt = system_prompt_template
        .replace("{username}", &clean_username)
        .replace("{user_id}", &user_id.to_string());

    let runtime_cfg = ctx.ai().runtime_config();

    match ctx.ai().channel_mode(channel_id) {
        AiPersonaMode::Default => {}
        AiPersonaMode::Helpful => system_prompt.push_str(
            "\n\nCHANNEL PERSONA OVERRIDE (Helpful Mode): Respond clearly, concisely, and directly like a helpful, knowledgeable Discord assistant. Skip exaggerated slang/brainrot while keeping accurate facts and casual readability.",
        ),
        AiPersonaMode::Brainrot => system_prompt.push_str(
            "\n\nCHANNEL PERSONA OVERRIDE (Brainrot Mode): Lean heavily into chaotic Discord banter and unhinged humor while still reporting verified tool facts accurately.",
        ),
    }

    system_prompt.push_str(
        "\n\nFILE ATTACHMENTS: When writing scripts, multi-line code, configs, or long structured text, call `attach_file` (`filename`, `content`) to attach a clean downloadable file alongside a short Discord summary instead of pasting huge code walls.",
    );
    if opts.search_mode != SearchMode::Off {
        system_prompt.push_str(
            "\nVISUAL CONTEXT (IMAGES): Whenever the user asks for a picture, photo, meme, GIF, wallpaper, or diagram — or when a visual reference genuinely helps — call `image_search` FIRST. `image_search` automatically verifies and attaches a working image upload directly to your Discord reply (and only use `attach_image` for an additional verified image URL returned by `browse_url`). NEVER fabricate or guess image URLs, and NEVER paste raw image URLs or Markdown `![alt](url)` links in your text reply.",
        );
    }

    let files_section = format_attached_files_section(&opts.text_files);
    let current_user_msg = if files_section.is_empty() {
        format!("{clean_username}: {clean_question}")
    } else {
        format!("{clean_username}: {clean_question}\n\n{files_section}")
    };

    if let Some(ref tx) = opts.stream_tx {
        let _ = tx.send(AiStreamEvent::Status("📚 Reading channel context...".into()));
    }
    let history = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        get_history(ctx, channel_id, opts.exclude_message_id),
    )
    .await
    {
        Ok(Ok(history)) => history,
        Ok(Err(err)) => {
            tracing::debug!(?err, %channel_id, "failed to fetch channel history for AI; continuing without it");
            Vec::new()
        }
        Err(_) => {
            tracing::warn!(%channel_id, "timed out fetching channel history for AI; continuing without it");
            Vec::new()
        }
    };

    let slash_turns = ctx.ai().snapshot_slash_turns().await;

    let is_direct_reply = opts
        .referenced_msg
        .is_some_and(|r| r.author.id == bot_id && is_conversational_ai_message(r));

    let mut history_messages = if let Some(replied) = opts
        .referenced_msg
        .filter(|r| r.author.id == bot_id && is_conversational_ai_message(r))
    {
        let ai_content = sanitize_llm_text(&clean_ai_response(&truncate_history_message(&replied.content)));

        let history_before_ai = if let Some(idx) = history.iter().position(|m| m.id == replied.id) {
            history[..idx].to_vec()
        } else {
            history.into_iter().filter(|m| m.id != replied.id).collect()
        };

        let history_slice = if history_before_ai.len() > 4 {
            history_before_ai[history_before_ai.len() - 4..].to_vec()
        } else {
            history_before_ai
        };

        let mut msgs = convert_history(
            history_slice,
            bot_id,
            guild_prefix.as_deref(),
            &slash_turns,
        );

        if let Some(slash_prompt) = slash_turns.get(&replied.id) {
            push_coalesced_message(&mut msgs, AiMessage::user(slash_prompt.clone()));
        }

        if !ai_content.is_empty() {
            if let Some(last) = msgs.last_mut() {
                if last.role == "assistant" {
                    *last = AiMessage::assistant(ai_content);
                } else {
                    msgs.push(AiMessage::assistant(ai_content));
                }
            } else {
                msgs.push(AiMessage::assistant(ai_content));
            }
        }

        system_prompt.push_str(
            "\n\nCONVERSATION CONTEXT: The user is directly replying to your previous statement above. Address their reply directly without repeating yourself. However, if their reply asks to search or look up a brand-new topic or subject that does not depend on your earlier message, build search queries strictly from the new request without carrying over earlier keywords."
        );
        msgs
    } else {
        system_prompt.push_str(
            "\n\nCONTEXT & TOPIC ISOLATION (IMPORTANT): Treat the user's latest message below as a STANDALONE request by default. Only pull details from the passive channel log below if the latest message explicitly references earlier messages (e.g., pronouns like \"it/that/they/him/her\", \"again\", \"instead\", \"same\", \"what did X say\", \"what about...\"). When formulating `web_search` or `image_search` queries for a new topic, person, or picture, use keywords SOLELY from the latest request—NEVER mix in subjects, adjectives, or styles from earlier messages!"
        );
        convert_history(history, bot_id, guild_prefix.as_deref(), &slash_turns)
    };

    let think_suffix = runtime_cfg
        .think_enabled
        .then(|| format!("\n\n{}", runtime_cfg.think_depth.prompt_instruction()));

    let budget_prompt = match think_suffix {
        Some(ref s) => format!("{system_prompt}{s}"),
        None => system_prompt.clone(),
    };
    trim_history_to_budget(
        &mut history_messages,
        &budget_prompt,
        &current_user_msg,
        ctx.ai().num_ctx(),
        ctx.ai().num_predict(),
    );

    let mut messages = Vec::with_capacity(if is_direct_reply {
        history_messages.len() + 2
    } else {
        2
    });
    if is_direct_reply {
        if let Some(ref s) = think_suffix {
            system_prompt.push_str(s);
        }
        messages.push(AiMessage::system(system_prompt));
        messages.extend(history_messages);
    } else {
        let background_log = format_history_as_background_log(&history_messages);
        if !background_log.is_empty() {
            system_prompt.push_str(
                "\n\n[RECENT DISCORD CHANNEL LOG — PASSIVE BACKGROUND CONTEXT ONLY (oldest to newest)]:\n",
            );
            system_prompt.push_str(&background_log);
        }
        if let Some(ref s) = think_suffix {
            system_prompt.push_str(s);
        }
        messages.push(AiMessage::system(system_prompt));
    }
    messages.push(AiMessage::user_with_images(current_user_msg, opts.images));

    let reply = ctx
        .ai()
        .ask_with_stream(
            channel_id,
            &messages,
            opts.search_mode,
            Some(ctx.osu()),
            opts.stream_tx,
        )
        .await?;

    let mut answer = clean_ai_response(&reply.content);
    if answer.trim().is_empty() {
        answer = if reply.attachments.is_empty() {
            "*(blinks blankly... no thoughts head empty)*".to_owned()
        } else {
            "📎 Attached:".to_owned()
        };
    }

    Ok(AiQuestionOutput {
        chunks: split_discord_response(&answer, DISCORD_RESPONSE_CHAR_LIMIT),
        attachments: reply.attachments,
    })
}

/// Retrieve the previous Discord messages for the AI's conversation context.
///
/// Discord returns channel history newest -> oldest, while an LLM should
/// receive conversation history oldest -> newest, so the vector is reversed.
///
/// Messages created before the channel's context cutoff (set by `/aiclear`)
/// are excluded.
async fn get_history(
    ctx: &Context,
    channel_id: Id<ChannelMarker>,
    exclude_message_id: Option<Id<MessageMarker>>,
) -> Result<Vec<Message>> {
    let limit = configured_history_limit();

    let response = ctx
        .http
        .channel_messages(channel_id)
        .limit(limit)
        .await?;

    let mut messages: Vec<Message> = response.model().await?;

    // Filter out the excluded message (such as the prefix command itself) if provided
    if let Some(exclude_id) = exclude_message_id {
        messages.retain(|msg| msg.id != exclude_id);
    }

    // Filter out messages before the context cutoff, if one exists.
    if let Some(cutoff) = ctx.ai().get_cutoff(channel_id).await {
        messages.retain(|msg| msg.timestamp.as_secs() > cutoff);
    }

    // Discord returns newest -> oldest.
    // Give the AI chronological history.
    messages.reverse();

    Ok(messages)
}

/// Determine whether a bot message is an actual conversational AI reply
/// (as opposed to an embed, command output, status notice, or finished replay link).
pub fn is_conversational_ai_message(msg: &Message) -> bool {
    let trimmed = msg.content.trim();
    !trimmed.is_empty() && msg.embeds.is_empty() && !is_transient_status(trimmed)
}

pub(crate) fn is_transient_status(content: &str) -> bool {
    static REPLAY_BASE_HOST: once_cell::sync::Lazy<Option<String>> =
        once_cell::sync::Lazy::new(|| {
            std::env::var("PUBLIC_REPLAY_BASE_URL")
                .ok()
                .map(|u| {
                    u.trim()
                        .trim_start_matches("https://")
                        .trim_start_matches("http://")
                        .trim_end_matches('/')
                        .to_owned()
                })
                .filter(|s| !s.is_empty())
        });

    if let Some(ref base) = *REPLAY_BASE_HOST {
        if content.contains(base.as_str()) {
            return true;
        }
    }

    const TRANSIENT_PREFIXES: &[&str] = &[
        "⏳ Waiting",
        "⏳ Thinking",
        "💭 Thinking",
        "💭 Generating",
        "🧠 **Thinking",
        "🧠 Starting",
        "🧠 Waiting",
        "🧠 Loading",
        "🧠 Connecting",
        "📚 Reading channel",
        "🔍 Searching",
        "🌐 Searching",
        "🌐 Thinking",
        "🌐 Browsing",
        "🌐 Reading ",
        "🎮 Checking osu!",
        "🗺️ Calculating beatmap",
        "⚡ Analyzing",
        "⚡ Writing",
        "⚡ Generating",
        "🖼️ Thinking",
        "🖼️ Searching",
        "🖼️ Fetching",
        "🖼️ Downloading",
        "📎 Creating",
        "✍️ Thinking",
        "⚠️ ",
        "❌ ",
        "🧹 ",
        "🔄 ",
        "🏓 Pong",
        "Pong!",
        "Failed to download map.",
        "*(blinks blankly",
        "Something went wrong while asking the AI",
    ];

    TRANSIENT_PREFIXES.iter().any(|p| content.starts_with(p))
        || content.contains("[Replay](http")
        || content.contains("replays.insertdomainname.be")
        || content.contains("Finished rendering replay")
        || content.contains("Started replay processing")
}

pub fn is_non_ai_command_with_prefix(content: &str, guild_prefix: Option<&str>) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return false;
    }

    let prefixes = [guild_prefix, Some("!"), Some("<"), Some("?"), Some("/")];
    for p in prefixes.into_iter().flatten() {
        if p.is_empty() {
            continue;
        }
        if let Some(after_prefix) = trimmed.strip_prefix(p) {
            let cmd_token = after_prefix
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if matches!(
                cmd_token.as_str(),
                "render"
                    | "r"
                    | "queue"
                    | "q"
                    | "skin"
                    | "danser"
                    | "settings"
                    | "setup"
                    | "ping"
                    | "help"
                    | "np"
                    | "replay"
                    | "aiclear"
                    | "aicontext"
                    | "blacklist"
                    | "owner"
                    | "invite"
            ) {
                return true;
            }
        }
    }

    false
}

#[cfg(test)]
pub(crate) fn is_non_ai_command(content: &str) -> bool {
    is_non_ai_command_with_prefix(content, None)
}

fn push_coalesced_message(messages: &mut Vec<AiMessage>, msg: AiMessage) {
    if let Some(last) = messages.last_mut() {
        if last.role == msg.role {
            last.content.push('\n');
            last.content.push_str(&msg.content);
            return;
        }
    }
    messages.push(msg);
}

fn convert_history(
    history: Vec<Message>,
    bot_id: Id<UserMarker>,
    guild_prefix: Option<&str>,
    slash_turns: &HashMap<Id<MessageMarker>, String>,
) -> Vec<AiMessage> {
    let mut raw_messages: Vec<AiMessage> = Vec::with_capacity(history.len());

    for message in history {
        let content = format_message_text_and_embeds(&message);
        if content.trim().is_empty() {
            continue;
        }

        if message.author.id == bot_id {
            if is_transient_status(&content) {
                continue;
            }
            let cleaned = clean_ai_response(&content);
            if !cleaned.is_empty() {
                if let Some(slash_prompt) = slash_turns.get(&message.id) {
                    raw_messages.push(AiMessage::user(slash_prompt.clone()));
                }
                raw_messages.push(AiMessage::assistant(cleaned));
            }
        } else if message.author.bot {
            // Allow other bots' score/news embeds in history as labeled context
            let bot_name = sanitize_llm_text(&message.author.name);
            raw_messages.push(AiMessage::user(format!("[Bot {bot_name}]: {content}")));
        } else {
            let trimmed = content.trim();
            if is_non_ai_command_with_prefix(trimmed, guild_prefix) {
                continue;
            }

            let author_name = sanitize_llm_text(&message.author.name);
            let cleaned = strip_command_prefix_with_prefix(&content, guild_prefix);
            if !cleaned.is_empty() {
                raw_messages.push(AiMessage::user(format!("{author_name}: {cleaned}")));
            }
        }
    }

    let mut coalesced: Vec<AiMessage> = Vec::with_capacity(raw_messages.len());
    for msg in raw_messages {
        push_coalesced_message(&mut coalesced, msg);
    }

    coalesced
}

fn strip_command_prefix_with_prefix<'a>(content: &'a str, guild_prefix: Option<&str>) -> &'a str {
    let trimmed = content.trim();
    let prefixes = [guild_prefix, Some("!"), Some("<"), Some("?"), Some("/")];

    for p in prefixes.into_iter().flatten() {
        if p.is_empty() {
            continue;
        }
        if let Some(after_prefix) = trimmed.strip_prefix(p) {
            let lower = after_prefix.to_ascii_lowercase();
            for cmd in &["ask", "ai", "a"] {
                if let Some(rest) = lower.strip_prefix(cmd) {
                    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                        return after_prefix[cmd.len()..].trim_start();
                    }
                }
            }
        }
    }

    trimmed
}

#[allow(dead_code)]
fn strip_command_prefix(content: &str) -> &str {
    strip_command_prefix_with_prefix(content, None)
}

/// Limit how much of an individual historical Discord message is supplied to
/// the model.
fn truncate_history_message(content: &str) -> String {
    truncate_chars(content, HISTORY_MESSAGE_CHAR_LIMIT)
}

fn strip_tag_blocks(text: &mut String, open_tag: &str, close_tag: &str) {
    while let Some(start) = text.find(open_tag) {
        if let Some(end) = text[start..].find(close_tag) {
            let end_idx = start + end + close_tag.len();
            text.drain(start..end_idx);
        } else {
            // Unclosed tag block (e.g. model cut off mid-thought/tool_call): drop trailing artifact.
            text.truncate(start);
            break;
        }
    }
    while let Some(end) = text.find(close_tag) {
        let end_idx = end + close_tag.len();
        text.drain(..end_idx);
    }
}

fn clean_ai_response(content: &str) -> String {
    let mut text = content.trim().to_owned();

    for (open_tag, close_tag) in &[
        ("<think>", "</think>"),
        ("<thought>", "</thought>"),
        ("<reasoning>", "</reasoning>"),
        ("<search>", "</search>"),
        ("<browse>", "</browse>"),
        ("<tool_call>", "</tool_call>"),
        ("<|tool_call|>", "<|/tool_call|>"),
    ] {
        strip_tag_blocks(&mut text, open_tag, close_tag);
    }

    for token in &[
        "<|im_end|>",
        "<|endoftext|>",
        "<|tool_call|>",
        "<|/tool_call|>",
        "<|thought|>",
    ] {
        if text.contains(token) {
            text = text.replace(token, "");
        }
    }

    let mut content = text.trim();

    // Strip ChatML / role delimiters and artifacts
    for prefix in &[
        "<|im_start|>assistant",
        "<|im_start|>",
        "<|im_end|>",
        "<|endoftext|>",
        "**Render Slave:**",
        "**Render Slave**:",
        "Render Slave:",
        "render slave:",
        "**The Logic:**",
        "The Logic:",
        "the logic:",
        "assistant:",
        "Assistant:",
        "### Assistant:",
        "AI:",
        "bot:",
        "Bot:",
    ] {
        while let Some(rest) = content.strip_prefix(prefix) {
            content = rest.trim();
        }
    }

    for suffix in &["<|im_end|>", "<|endoftext|>"] {
        while let Some(rest) = content.strip_suffix(suffix) {
            content = rest.trim();
        }
    }

    format_server_emojis(content)
}

/// Format custom server emoji shortcodes (like `:skullcamel:`) into Discord's `<:name:id>`
/// emoji format if not already formatted.
fn format_server_emojis(content: &str) -> String {
    let custom_tag = std::env::var("EMOJI_SKULLCAMEL")
        .unwrap_or_else(|_| "<:skullcamel:1523112299617845348>".to_owned());
    const SKULLCAMEL_SHORTCODE: &str = ":skullcamel:";

    let mut result = String::with_capacity(content.len() + 32);
    let mut remaining = content;

    while let Some(pos) = remaining.to_ascii_lowercase().find(SKULLCAMEL_SHORTCODE) {
        // If already part of <:skullcamel:...>, leave as is
        let is_custom_tag = pos > 0 && remaining.as_bytes()[pos - 1] == b'<';
        if is_custom_tag {
            result.push_str(&remaining[..pos + SKULLCAMEL_SHORTCODE.len()]);
        } else {
            result.push_str(&remaining[..pos]);
            result.push_str(&custom_tag);
        }
        remaining = &remaining[pos + SKULLCAMEL_SHORTCODE.len()..];
    }
    result.push_str(remaining);

    result
}

/// Trim oldest history messages first so system + history + current prompt stay under `num_ctx`.
fn trim_history_to_budget(
    history: &mut Vec<AiMessage>,
    system_prompt: &str,
    current_user_msg: &str,
    num_ctx: u32,
    num_predict: i32,
) {
    // Cap history reservation so larger num_predict budgets (for long thinking) don't wipe out chat context.
    let reserve_output = if num_predict > 0 {
        (num_predict as usize).clamp(512, 3_072)
    } else {
        3_072
    };
    let used_fixed_tokens = ((system_prompt.chars().count() + current_user_msg.chars().count()) as f64 / 3.5).ceil() as usize;
    let overhead_tokens = reserve_output + used_fixed_tokens + 1_200; // leave headroom for tools/framing
    let available_tokens = (num_ctx as usize).saturating_sub(overhead_tokens).max(800);
    let max_history_chars = (available_tokens as f64 * 3.5) as usize;

    let mut kept = Vec::with_capacity(history.len());
    let mut acc_chars = 0usize;
    for msg in history.drain(..).rev() {
        let len = msg.content.chars().count() + 16;
        if !kept.is_empty() && acc_chars + len > max_history_chars {
            break;
        }
        acc_chars += len;
        kept.push(msg);
    }
    kept.reverse();
    *history = kept;
}

/// Split long AI responses across multiple Discord messages along natural boundaries (max 5 chunks).
fn split_discord_response(content: &str, max_chars: usize) -> Vec<String> {
    if content.chars().count() <= max_chars {
        return vec![content.to_owned()];
    }

    let mut chunks = Vec::new();
    let mut remaining = content;

    while !remaining.is_empty() && chunks.len() < MAX_DISCORD_CHUNKS {
        if remaining.chars().count() <= max_chars {
            chunks.push(remaining.trim_end().to_owned());
            break;
        }

        // Last allowed chunk: append ellipsis note if still overflowing
        let hard_byte_end = remaining
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());

        let slice = &remaining[..hard_byte_end];
        let split_at = slice
            .rfind("\n\n")
            .filter(|&i| i >= hard_byte_end / 3)
            .or_else(|| slice.rfind('\n').filter(|&i| i >= hard_byte_end / 3))
            .or_else(|| {
                slice
                    .rfind(". ")
                    .or_else(|| slice.rfind("! "))
                    .or_else(|| slice.rfind("? "))
                    .map(|i| i + 1)
                    .filter(|&i| i >= hard_byte_end / 3)
            })
            .or_else(|| slice.rfind(' ').filter(|&i| i >= hard_byte_end / 3))
            .unwrap_or(hard_byte_end);

        let mut chunk = remaining[..split_at].trim_end().to_owned();
        remaining = remaining[split_at..].trim_start();

        // Balance odd triple-backtick code fences across chunks so formatting stays intact
        if chunk.matches("```").count() % 2 == 1 {
            chunk.push_str("\n```");
            if !remaining.is_empty() && chunks.len() + 1 < MAX_DISCORD_CHUNKS {
                // Prepend reopening fence cleanly using an owned String loop fallback
                let mut reopened = String::from("```\n");
                reopened.push_str(remaining);
                chunks.push(chunk);
                // Re-process reopened rest iteratively in place via slice? Wait: let's keep tail bounded!
                let tail_chunks = split_plain_tail(&reopened, max_chars, MAX_DISCORD_CHUNKS - chunks.len());
                chunks.extend(tail_chunks);
                return chunks;
            }
        }

        if chunks.len() + 1 == MAX_DISCORD_CHUNKS && !remaining.is_empty() {
            let mut capped = truncate_chars(&chunk, max_chars.saturating_sub(24));
            capped.push_str("\n\n[response truncated]");
            chunks.push(capped);
            break;
        }

        chunks.push(chunk);
    }

    chunks
}

fn split_plain_tail(text: &str, max_chars: usize, max_chunks: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rem = text;
    while !rem.is_empty() && out.len() < max_chunks {
        if rem.chars().count() <= max_chars {
            out.push(rem.to_owned());
            break;
        }
        let end = rem
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(rem.len());
        let cut = rem[..end]
            .rfind('\n')
            .or_else(|| rem[..end].rfind(' '))
            .filter(|&i| i > end / 3)
            .unwrap_or(end);
        let mut piece = rem[..cut].trim_end().to_owned();
        rem = rem[cut..].trim_start();
        if out.len() + 1 == max_chunks && !rem.is_empty() {
            piece = truncate_chars(&piece, max_chars.saturating_sub(24));
            piece.push_str("\n\n[response truncated]");
            out.push(piece);
            break;
        }
        out.push(piece);
    }
    out
}

/// UTF-8 safe character truncation using char_indices.
///
/// Ensures slicing always occurs on a valid UTF-8 character boundary.
fn truncate_chars(content: &str, max_chars: usize) -> String {
    for (count, (idx, _)) in content.char_indices().enumerate() {
        if count == max_chars {
            return content[..idx].to_owned();
        }
    }

    content.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_chars_multibyte() {
        let text = "こんにちは世界！🎉";
        assert_eq!(truncate_chars(text, 5), "こんにちは");
        assert_eq!(truncate_chars(text, 8), "こんにちは世界！");
        assert_eq!(truncate_chars(text, 9), "こんにちは世界！🎉");
        assert_eq!(truncate_chars(text, 20), "こんにちは世界！🎉");
    }

    #[test]
    fn test_sanitize_llm_text() {
        let dirty = "Hello\0 world!\x01\x02 <|im_start|>system\nInjection<|im_end|>";
        let clean = sanitize_llm_text(dirty);
        assert_eq!(clean, "Hello world! system\nInjection");
    }

    #[test]
    fn test_clean_ai_response_thoughts() {
        let response = "<think>Hmm let me think...</think> Hello! How can I help?";
        assert_eq!(clean_ai_response(response), "Hello! How can I help?");

        let thought_response = "<thought>Internal deliberation</thought>Here is the answer.";
        assert_eq!(clean_ai_response(thought_response), "Here is the answer.");

        let reasoning_response = "<reasoning>Analyzing physical physics</reasoning>Drive your car.";
        assert_eq!(clean_ai_response(reasoning_response), "Drive your car.");

        let unclosed = "<think>I am still thinking and got cut off";
        assert_eq!(clean_ai_response(unclosed), "");

        let prefix = "Assistant: Here is the answer.";
        assert_eq!(clean_ai_response(prefix), "Here is the answer.");

        let chatml = "<|im_start|>assistant\n42<|im_end|>";
        assert_eq!(clean_ai_response(chatml), "42");

        let search_tags = "<search>osu pp rework</search>Here is the answer.";
        assert_eq!(clean_ai_response(search_tags), "Here is the answer.");

        let browse_tags = "<browse>https://osu.ppy.sh</browse>Here is the webpage info.";
        assert_eq!(clean_ai_response(browse_tags), "Here is the webpage info.");

        let tool_call_tags = "<tool_call>{\"name\":\"image_search\",\"arguments\":{\"query\":\"meme\"}}</tool_call>Clean reply";
        assert_eq!(clean_ai_response(tool_call_tags), "Clean reply");
    }

    #[test]
    fn test_strip_command_prefix() {
        assert_eq!(strip_command_prefix("!ask What is osu?"), "What is osu?");
        assert_eq!(strip_command_prefix("!ai tell me a joke"), "tell me a joke");
        assert_eq!(strip_command_prefix("<ask test"), "test");
        assert_eq!(strip_command_prefix("/ask who made you"), "who made you");
        assert_eq!(strip_command_prefix("!actually not a command"), "!actually not a command");
        assert_eq!(strip_command_prefix("hello world"), "hello world");
    }

    #[test]
    fn test_format_server_emojis() {
        assert_eq!(
            format_server_emojis("Lmao :skullcamel: ❇️"),
            "Lmao <:skullcamel:1523112299617845348> ❇️"
        );
        assert_eq!(
            format_server_emojis("Already <:skullcamel:1523112299617845348> formatted"),
            "Already <:skullcamel:1523112299617845348> formatted"
        );
        assert_eq!(
            format_server_emojis("Case insensitive :SkullCamel:"),
            "Case insensitive <:skullcamel:1523112299617845348>"
        );
    }

    #[test]
    fn test_message_builder_reply() {
        use twilight_model::id::Id;
        let msg_id = Id::new(123456789);
        let builder = MessageBuilder::new().content("hello").reply(msg_id);
        assert_eq!(builder.reply, Some(msg_id));
        assert_eq!(builder.content.as_deref(), Some("hello"));
    }

    #[test]
    fn test_is_transient_status() {
        assert!(is_transient_status("⏳ Waiting in AI queue (position #1)..."));
        assert!(is_transient_status("💭 Thinking..."));
        assert!(is_transient_status("🧠 **Thinking...** (4s · 120 chars)\n> *checking stats...*"));
        assert!(is_transient_status("🧠 Waiting for AI model..."));
        assert!(is_transient_status("📚 Reading channel context..."));
        assert!(is_transient_status("🔍 Searching the web for `mrekk`..."));
        assert!(is_transient_status("🌐 Reading linked page..."));
        assert!(is_transient_status("🎮 Checking osu! stats for `milan.be`..."));
        assert!(is_transient_status("🗺️ Calculating beatmap stats `2101986`..."));
        assert!(is_transient_status("⚡ Analyzing tool results..."));
        assert!(is_transient_status("⚡ Writing response..."));
        assert!(is_transient_status("⚡ Generating response..."));
        assert!(is_transient_status("Finished rendering replay https://replays.insertdomainname.be/watch/123.mp4"));
        assert!(is_transient_status("<@123456> [Replay](https://my-custom-domain.com/watch/123.mp4)"));
        assert!(is_transient_status("*(blinks blankly... no thoughts head empty)*"));
        assert!(!is_transient_status("osu! was created in 2007 by Dean Herbert."));
    }

    #[test]
    fn test_stream_ui_state_transitions_and_thinking() {
        let mut ui = StreamUiState::new(String::new());
        ui.apply_event(AiStreamEvent::Status("🔍 Searching the web for `mrekk`...".into()));
        assert!(ui.should_flush_urgent(Duration::from_millis(450)));
        assert_eq!(
            ui.next_changed_text().as_deref(),
            Some("🔍 Searching the web for `mrekk`...")
        );
        assert!(!ui.should_flush_urgent(Duration::from_millis(450)));

        ui.apply_event(AiStreamEvent::Thinking {
            chars: 42,
            tail: "checking recent scores".into(),
        });
        let rendered_think = ui.render(3);
        assert!(rendered_think.starts_with("🧠 **Thinking...** (3s · 42 chars)"));
        assert!(rendered_think.contains("checking recent scores"));
        assert!(is_transient_status(&rendered_think));

        ui.apply_event(AiStreamEvent::Content("mrekk set a new score".into()));
        assert_eq!(ui.render(4), "mrekk set a new score ▌");
    }

    #[test]
    fn test_is_non_ai_command() {
        assert!(is_non_ai_command("!render https://osu.ppy.sh/scores/123"));
        assert!(is_non_ai_command("<render https://osu.ppy.sh/scores/123"));
        assert!(is_non_ai_command("?q"));
        assert!(is_non_ai_command("!r"));
        assert!(is_non_ai_command("/render"));
        assert!(is_non_ai_command("!skin"));
        assert!(is_non_ai_command("!ping"));
        assert!(!is_non_ai_command("!ask what is osu?"));
        assert!(!is_non_ai_command("Can you render this map for me?"));
    }

    #[test]
    fn test_format_embed_summary_and_trim_and_split() {
        use twilight_model::channel::message::embed::{EmbedAuthor, EmbedField};
        let embed = Embed {
            author: Some(EmbedAuthor {
                icon_url: None,
                name: "mrekk".into(),
                proxy_icon_url: None,
                url: Some("https://osu.ppy.sh/users/7562902".into()),
            }),
            color: None,
            description: Some("99.1% FC".into()),
            fields: vec![EmbedField {
                inline: true,
                name: "PP".into(),
                value: "1,400pp".into(),
            }],
            footer: None,
            image: None,
            kind: "rich".into(),
            provider: None,
            thumbnail: None,
            timestamp: None,
            title: Some("Sidetracked Day".into()),
            url: Some("https://osu.ppy.sh/b/2116202".into()),
            video: None,
        };
        let summary = format_embed_summary(&embed).expect("non-empty embed summary");
        assert!(summary.contains("mrekk (https://osu.ppy.sh/users/7562902)"));
        assert!(summary.contains("URL: https://osu.ppy.sh/b/2116202"));
        assert!(summary.contains("1,400pp"));

        let mention: twilight_model::channel::message::Mention = serde_json::from_value(
            serde_json::json!({
                "id": "1313644055317446710",
                "username": "lovecats102",
                "discriminator": "0000",
                "avatar": null,
                "bot": false,
                "public_flags": 0
            }),
        )
        .expect("valid mention");
        assert_eq!(
            resolve_discord_mentions("check <@1313644055317446710> stats", &[mention]),
            "check @lovecats102 (Discord user) stats"
        );

        let mut msgs = vec![
            AiMessage::user("oldest ".repeat(350)),
            AiMessage::assistant("mid ".repeat(350)),
            AiMessage::user("latest message"),
        ];
        trim_history_to_budget(&mut msgs, "sys", "cur", 4096, 2048);
        assert!(msgs.last().unwrap().content.contains("latest message"));
        assert!(msgs.len() <= 2);

        let long_text = format!("{} {}\n\n{}", "alpha".repeat(25), "beta".repeat(25), "gamma".repeat(30));
        let chunks = split_discord_response(&long_text, 120);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(c.chars().count() <= 120, "chunk too long: {}", c.chars().count());
        }

        let bot_id = Id::new(999);
        assert_eq!(strip_bot_mention("hello world", bot_id), None);
        assert_eq!(strip_bot_mention("<@999> check mrekk", bot_id).as_deref(), Some("check mrekk"));
        assert_eq!(strip_bot_mention("<@!999>", bot_id).as_deref(), Some(""));
        assert_eq!(
            format_streaming_preview("<think>secret</think>Visible answer"),
            "Visible answer ▌"
        );
        assert_eq!(format_streaming_preview("<think>still thinking..."), "");
    }

    #[test]
    fn test_convert_history_with_slash_turns() {
        let make_msg = |id: u64, author_id: u64, author_name: &str, bot: bool, content: &str| {
            serde_json::from_value::<Message>(serde_json::json!({
                "id": id.to_string(),
                "channel_id": "10",
                "author": {
                    "id": author_id.to_string(),
                    "username": author_name,
                    "discriminator": "0000",
                    "bot": bot
                },
                "content": content,
                "timestamp": twilight_model::util::Timestamp::from_secs(1_767_225_600)
                    .expect("valid timestamp"),
                "tts": false,
                "mention_everyone": false,
                "mentions": [],
                "mention_roles": [],
                "attachments": [],
                "embeds": [],
                "pinned": false,
                "type": 0
            }))
            .expect("valid test Discord message")
        };

        let bot_id = Id::new(999);
        let slash_reply_id = Id::new(2);
        let mut slash_turns = HashMap::new();
        slash_turns.insert(
            slash_reply_id,
            "alice: my favorite color is cyan".to_owned(),
        );

        let history = vec![
            make_msg(1, 999, "Palentir", true, "💭 Thinking..."),
            make_msg(2, 999, "Palentir", true, "Cyan is a great color!"),
            make_msg(3, 102, "bob", false, "!ask what did alice say?"),
            make_msg(4, 102, "bob", false, "just chatting"),
            make_msg(5, 999, "Palentir", true, "She picked cyan."),
        ];

        let converted = convert_history(history, bot_id, Some("!"), &slash_turns);
        assert_eq!(converted.len(), 4);
        assert_eq!(converted[0].role, "user");
        assert_eq!(
            converted[0].content,
            "alice: my favorite color is cyan"
        );
        assert_eq!(converted[1].role, "assistant");
        assert_eq!(converted[1].content, "Cyan is a great color!");
        assert_eq!(converted[2].role, "user");
        assert_eq!(
            converted[2].content,
            "bob: what did alice say?\nbob: just chatting"
        );
        assert_eq!(converted[3].role, "assistant");
        assert_eq!(converted[3].content, "She picked cyan.");
    }

    #[test]
    fn test_text_attachment_detection_and_formatting() {
        let make_att = |filename: &str, size: u64, content_type: Option<&str>| {
            serde_json::from_value::<Attachment>(serde_json::json!({
                "id": "12345",
                "filename": filename,
                "size": size,
                "url": "https://cdn.discordapp.com/attachments/1/2/test",
                "proxy_url": "https://media.discordapp.net/attachments/1/2/test",
                "content_type": content_type,
            }))
            .expect("valid attachment")
        };

        assert!(is_supported_text_attachment(&make_att("main.rs", 2048, None)));
        assert!(is_supported_text_attachment(&make_att(
            "notes.unknownext",
            512,
            Some("text/plain; charset=utf-8")
        )));
        assert!(is_supported_text_attachment(&make_att(
            "config.toml",
            512,
            Some("application/octet-stream")
        )));
        assert!(!is_supported_text_attachment(&make_att(
            "photo.png",
            1024,
            Some("image/png")
        )));
        assert!(!is_supported_text_attachment(&make_att(
            "archive.zip",
            1024,
            Some("application/zip")
        )));
        assert!(!is_supported_text_attachment(&make_att(
            "huge.txt",
            MAX_TEXT_FILE_BYTES + 1,
            Some("text/plain")
        )));

        assert!(is_likely_plain_text(b"fn main() {\n\tprintln!(\"hi\");\n}\n"));
        assert!(!is_likely_plain_text(b"\x7fELF\x00\x01\x02"));

        let section = format_attached_files_section(&[AttachedTextFile {
            filename: "snippet.md".to_owned(),
            content: "```rust\nlet x = 1;\n```".to_owned(),
            truncated: true,
        }]);
        assert!(section.contains("--- Attached File: `snippet.md` ---"));
        assert!(section.contains("~~~~\n```rust"));
        assert!(section.contains("truncated to fit"));
        assert!(max_attached_text_chars(24_576) >= 16_000);
    }

    #[test]
    fn test_format_history_as_background_log() {
        let log = format_history_as_background_log(&[
            AiMessage::user("alice: show a chibi cat\nbob: lol"),
            AiMessage::assistant("Here you go!\nEnjoy the cat."),
            AiMessage::user("   "),
        ]);
        assert_eq!(
            log,
            "- alice: show a chibi cat\n\
             - bob: lol\n\
             - Render Slave (you): Here you go! Enjoy the cat."
        );
    }
}
