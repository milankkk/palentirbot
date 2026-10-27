use std::sync::Arc;
use std::time::{Duration, Instant};

use command_macros::SlashCommand;
use eyre::Result;
use tokio::sync::mpsc;
use twilight_interactions::command::{CommandModel, CreateCommand};
use twilight_model::{
    channel::Message,
    id::{
        marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker},
        Id,
    },
};

use crate::{
    ai::{
        ollama::AiStatusUpdate, search::strip_tool_tags, AiMessage, AiMode,
    },
    core::Context,
    util::{
        builder::MessageBuilder, interaction::InteractionCommand, Authored, ChannelExt,
        InteractionCommandExt, MessageExt,
    },
};

/// Maximum number of previous Discord messages sent to the AI.
const HISTORY_LIMIT: u16 = 100;
/// Maximum number of characters taken from a single historical message.
const HISTORY_MESSAGE_CHAR_LIMIT: usize = 1_500;
/// Keep some room below Discord's 2,000-character message limit.
const DISCORD_RESPONSE_CHAR_LIMIT: usize = 1_900;

#[derive(CommandModel, CreateCommand, SlashCommand)]
#[command(name = "ask")]
#[flags(SKIP_DEFER)]
/// Ask the AI a question (Thinking or Fast mode)
pub struct Ask {
    /// What you want to ask
    question: String,
    /// Response mode (Thinking: deep reasoning, Fast: quick direct response)
    mode: Option<AiMode>,
}

#[derive(CommandModel, CreateCommand, SlashCommand)]
#[command(name = "fast")]
#[flags(SKIP_DEFER)]
/// Ask the AI a question in Fast mode (quick direct response without reasoning)
pub struct Fast {
    /// What you want to ask
    question: String,
}

pub struct MessageStatusUpdater {
    ctx: Arc<Context>,
    target: StatusTarget,
    last_update: Instant,
    last_rendered: String,
}

enum StatusTarget {
    Interaction(Box<InteractionCommand>),
    Message(Id<MessageMarker>, Id<ChannelMarker>),
}

impl MessageStatusUpdater {
    pub fn new_interaction(ctx: Arc<Context>, cmd: InteractionCommand) -> Self {
        Self {
            ctx,
            target: StatusTarget::Interaction(Box::new(cmd)),
            last_update: Instant::now() - Duration::from_secs(10),
            last_rendered: String::new(),
        }
    }

    pub fn new_message(ctx: Arc<Context>, msg_id: Id<MessageMarker>, channel_id: Id<ChannelMarker>) -> Self {
        Self {
            ctx,
            target: StatusTarget::Message(msg_id, channel_id),
            last_update: Instant::now() - Duration::from_secs(10),
            last_rendered: String::new(),
        }
    }

    pub async fn update_status(&mut self, status: AiStatusUpdate, force: bool) {
        let text = match status {
            AiStatusUpdate::LoadingModel => {
                "⏳ Thinking (Loading model into memory & reading context...)".to_string()
            }
            AiStatusUpdate::GeneratingFast => "⚡ Generating response...".to_string(),
            AiStatusUpdate::Reasoning {
                elapsed_secs,
                thought_snippet,
            } => {
                format!("🧠 **Thinking...** (elapsed: {elapsed_secs}s)\n> *{thought_snippet}*")
            }
            AiStatusUpdate::WritingResponse => "✍️ Thinking (Writing response...)".to_string(),
            AiStatusUpdate::Searching {
                query,
                category,
                safesearch_on,
            } => {
                let safe_str = if safesearch_on { "ON" } else { "OFF" };
                format!("🌐 Searching {category} for \"{query}\" • SafeSearch: {safe_str}...")
            }
            AiStatusUpdate::DownloadingMedia => {
                "🖼️ Downloading media attachment...".to_string()
            }
        };

        if !force {
            if self.last_update.elapsed() < Duration::from_millis(2000) {
                return;
            }
            if self.last_rendered == text {
                return;
            }
        }

        self.last_update = Instant::now();
        self.last_rendered = text.clone();

        let builder = MessageBuilder::new().content(text);
        match &self.target {
            StatusTarget::Interaction(cmd) => {
                let _ = cmd.update(&self.ctx, &builder).await;
            }
            StatusTarget::Message(msg_id, ch_id) => {
                let _ = (*msg_id, *ch_id).update(&self.ctx, &builder).await;
            }
        }
    }

    pub async fn finish(
        &self,
        content: String,
        attachment: Option<(String, Vec<u8>)>,
    ) -> Result<()> {
        let mut builder = MessageBuilder::new().content(content);
        if let Some((name, bytes)) = attachment {
            builder = builder.attachment(name, bytes);
        }

        match &self.target {
            StatusTarget::Interaction(cmd) => {
                cmd.update(&self.ctx, &builder).await?;
            }
            StatusTarget::Message(msg_id, ch_id) => {
                (*msg_id, *ch_id).update(&self.ctx, &builder).await?;
            }
        }
        Ok(())
    }

    pub async fn error(&self, err_msg: &str) -> Result<()> {
        let builder = MessageBuilder::new().content(err_msg);
        match &self.target {
            StatusTarget::Interaction(cmd) => {
                cmd.update(&self.ctx, &builder).await?;
            }
            StatusTarget::Message(msg_id, ch_id) => {
                (*msg_id, *ch_id).update(&self.ctx, &builder).await?;
            }
        }
        Ok(())
    }
}

async fn slash_ask(ctx: Arc<Context>, command: InteractionCommand) -> Result<()> {
    slash_ask_impl(ctx, command, None).await
}

async fn slash_fast(ctx: Arc<Context>, command: InteractionCommand) -> Result<()> {
    slash_ask_impl(ctx, command, Some(AiMode::Fast)).await
}

async fn slash_ask_impl(
    ctx: Arc<Context>,
    mut command: InteractionCommand,
    mode_override: Option<AiMode>,
) -> Result<()> {
    if !crate::ai::is_enabled_for_guild(&ctx, command.guild_id) {
        command
            .error_callback(&ctx, crate::ai::AI_DISABLED_REPLY, true)
            .await?;
        return Ok(());
    }

    let ai = match ctx.ai() {
        Some(ai) => ai,
        None => {
            command
                .error_callback(&ctx, "AI module is disabled.", true)
                .await?;
            return Ok(());
        }
    };

    let (question, mode) = if let Some(m) = mode_override {
        let Fast { question } = Fast::from_interaction(command.input_data())?;
        (question, m)
    } else {
        let Ask { question, mode } = Ask::from_interaction(command.input_data())?;
        (question, mode.unwrap_or(AiMode::Thinking))
    };

    command.defer(&ctx, false).await?;

    // Enter the fair FIFO AI queue to avoid overloading the backend
    let ticket = match ai.queue().enter().await {
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
    } else {
        let initial_text = match mode {
            AiMode::Thinking => "⏳ Thinking (Loading model into memory & reading context...)".to_string(),
            AiMode::Fast => "⚡ Generating response...".to_string(),
        };
        let builder = MessageBuilder::new().content(initial_text);
        let _ = command.update(&ctx, &builder).await;
    }

    let _permit = ticket.wait().await;

    let (user_id, user_name) = {
        let user = command.user()?;
        (user.id, user.name.clone())
    };
    let channel_id = command.channel_id;
    let guild_id = command.guild_id;

    let updater = MessageStatusUpdater::new_interaction(Arc::clone(&ctx), command);
    let (tx, mut rx) = mpsc::channel::<AiStatusUpdate>(64);

    let updater_handle = tokio::spawn(async move {
        let mut u = updater;
        while let Some(status) = rx.recv().await {
            let force = matches!(
                status,
                AiStatusUpdate::LoadingModel
                    | AiStatusUpdate::GeneratingFast
                    | AiStatusUpdate::Searching { .. }
                    | AiStatusUpdate::DownloadingMedia
            );
            u.update_status(status, force).await;
        }
        u
    });

    let exec_res = execute_ai_question(
        &ctx,
        channel_id,
        guild_id,
        user_id,
        &user_name,
        &question,
        None,
        None,
        mode,
        tx,
    )
    .await;

    let updater = updater_handle.await.unwrap_or_else(|_| {
        MessageStatusUpdater::new_message(Arc::clone(&ctx), Id::new(1), channel_id)
    });

    match exec_res {
        Ok((answer, attachment)) => {
            updater.finish(answer, attachment).await?;
        }
        Err(err) => {
            tracing::error!(?err, %user_id, "AI slash request failed");
            let _ = updater
                .error("Something went wrong while asking the AI. Please try again later.")
                .await;
            return Err(err.wrap_err("AI request failed"));
        }
    }

    Ok(())
}

/// Handle prefix `!ask` (or `!ai`, `!a`, `!fast`) messages.
pub async fn handle_prefix_ask(
    ctx: Arc<Context>,
    msg: &Message,
    args_str: &str,
    prefix: &str,
    mode: AiMode,
) {
    if !crate::ai::is_enabled_for_guild(&ctx, msg.guild_id) {
        let _ = msg.error(&ctx, crate::ai::AI_DISABLED_REPLY).await;
        return;
    }

    let ai = match ctx.ai() {
        Some(ai) => ai,
        None => return,
    };

    let mut question = args_str.trim().to_string();

    let bot_id = ctx.cache.current_user(|u| u.id).ok();
    let is_reply_to_ai = msg
        .referenced_message
        .as_deref()
        .map(|r| Some(r.author.id) == bot_id)
        .unwrap_or(false);

    if let Some(ref replied) = msg.referenced_message {
        if !is_reply_to_ai {
            let ref_author = sanitize_llm_text(&replied.author.name);
            let ref_content = sanitize_llm_text(&truncate_history_message(&replied.content));
            if !ref_content.is_empty() {
                if question.is_empty() {
                    question = format!("What do you think about what {ref_author} said: \"{ref_content}\"?");
                } else {
                    question = format!("(In reply to {ref_author}: \"{ref_content}\")\n{question}");
                }
            }
        }
    }

    if question.is_empty() {
        if is_reply_to_ai {
            question = "What do you think about what you said?".to_string();
        } else {
            let example_cmd = match mode {
                AiMode::Fast => format!("{prefix}a What is osu?"),
                AiMode::Thinking => format!("{prefix}ask What is osu?"),
            };
            let _ = msg
                .error(
                    &ctx,
                    format!("Please provide a question! Example: `{example_cmd}`"),
                )
                .await;
            return;
        }
    }

    // Enter fair FIFO AI queue
    let ticket = match ai.queue().enter().await {
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
        match mode {
            AiMode::Thinking => "⏳ Thinking (Loading model into memory & reading context...)".to_string(),
            AiMode::Fast => "⚡ Generating response...".to_string(),
        }
    };

    let placeholder = msg
        .channel_id
        .create_message(
            &ctx,
            &MessageBuilder::new()
                .content(initial_content)
                .reply(msg.id),
        )
        .await
        .ok();

    let _permit = ticket.wait().await;

    let placeholder_msg = match placeholder {
        Some(p) => p,
        None => {
            let _ = msg.error(&ctx, "Failed to send initial thinking placeholder.").await;
            return;
        }
    };

    let _ = ctx.http.create_typing_trigger(msg.channel_id).await;

    let updater = MessageStatusUpdater::new_message(Arc::clone(&ctx), placeholder_msg.id, placeholder_msg.channel_id);
    let (tx, mut rx) = mpsc::channel::<AiStatusUpdate>(64);

    let updater_handle = tokio::spawn(async move {
        let mut u = updater;
        while let Some(status) = rx.recv().await {
            let force = matches!(
                status,
                AiStatusUpdate::LoadingModel
                    | AiStatusUpdate::GeneratingFast
                    | AiStatusUpdate::Searching { .. }
                    | AiStatusUpdate::DownloadingMedia
            );
            u.update_status(status, force).await;
        }
        u
    });

    let exec_res = execute_ai_question(
        &ctx,
        msg.channel_id,
        msg.guild_id,
        msg.author.id,
        &msg.author.name,
        &question,
        Some(msg.id),
        msg.referenced_message.as_deref(),
        mode,
        tx,
    )
    .await;

    let updater = updater_handle.await.unwrap_or_else(|_| {
        MessageStatusUpdater::new_message(Arc::clone(&ctx), placeholder_msg.id, placeholder_msg.channel_id)
    });

    match exec_res {
        Ok((answer, attachment)) => {
            if let Err(e) = updater.finish(answer.clone(), attachment.clone()).await {
                tracing::warn!(?e, "failed to update placeholder, sending fresh message");
                let mut b = MessageBuilder::new().content(answer).reply(msg.id);
                if let Some((name, bytes)) = attachment {
                    b = b.attachment(name, bytes);
                }
                let _ = msg.channel_id.create_message(&ctx, &b).await;
            }
        }
        Err(err) => {
            tracing::error!(?err, user_id = %msg.author.id, "prefix AI request failed");
            let error_content = "Something went wrong while asking the AI. Please try again later.";
            let _ = updater.error(error_content).await;
        }
    }
}

/// Core function to formulate messages and query the AI backend.
#[allow(clippy::too_many_arguments)]
pub async fn execute_ai_question(
    ctx: &Context,
    channel_id: Id<ChannelMarker>,
    guild_id: Option<Id<GuildMarker>>,
    _user_id: Id<UserMarker>,
    username: &str,
    question: &str,
    exclude_message_id: Option<Id<MessageMarker>>,
    referenced_msg: Option<&Message>,
    mode: AiMode,
    status_sender: mpsc::Sender<AiStatusUpdate>,
) -> Result<(String, Option<(String, Vec<u8>)>)> {
    let ai = ctx.ai().ok_or_else(|| eyre::eyre!("AI is not initialized"))?;

    let bot_id = ctx
        .cache
        .current_user(|user| user.id)
        .expect("missing CurrentUser in cache");

    let history = get_history(ctx, channel_id, exclude_message_id).await?;

    let system_prompt = ai.get_prompt().await;

    let clean_username = sanitize_llm_text(username);
    let clean_question = sanitize_llm_text(question);

    let current_user_msg = format!("{clean_username}: {clean_question}");

    let messages = if let Some(replied) = referenced_msg {
        if replied.author.id == bot_id {
            let mut raw_ai_text = replied.content.clone();
            if raw_ai_text.trim().is_empty() {
                for embed in &replied.embeds {
                    if let Some(ref desc) = embed.description {
                        raw_ai_text.push_str(desc);
                        raw_ai_text.push(' ');
                    }
                    if let Some(ref title) = embed.title {
                        raw_ai_text.push_str(title);
                        raw_ai_text.push(' ');
                    }
                }
            }
            let ai_content = sanitize_llm_text(&clean_ai_response(&truncate_history_message(&raw_ai_text)));

            let history_before_ai = if let Some(idx) = history.iter().position(|m| m.id == replied.id) {
                history[..idx].to_vec()
            } else {
                history.into_iter().filter(|m| m.id != replied.id).collect()
            };

            let mut history_messages = convert_history(history_before_ai, bot_id);

            if !ai_content.is_empty() {
                history_messages.push(AiMessage::assistant(ai_content));
            }

            let mut msgs = Vec::with_capacity(history_messages.len() + 2);
            msgs.push(AiMessage::system(system_prompt));
            msgs.extend(history_messages);
            msgs.push(AiMessage::user(format!(
                "{clean_username} (in reply to your statement above): {clean_question}"
            )));
            msgs
        } else {
            let history_messages = convert_history(history, bot_id);
            let mut msgs = Vec::with_capacity(history_messages.len() + 2);
            msgs.push(AiMessage::system(system_prompt));
            msgs.extend(history_messages);
            msgs.push(AiMessage::user(current_user_msg));
            msgs
        }
    } else {
        let history_messages = convert_history(history, bot_id);
        let mut msgs = Vec::with_capacity(history_messages.len() + 2);
        msgs.push(AiMessage::system(system_prompt));
        msgs.extend(history_messages);
        msgs.push(AiMessage::user(current_user_msg));
        msgs
    };

    let safesearch_level = crate::ai::safesearch_level_for_guild(ctx, guild_id);

    let gen_res = ai
        .ollama()
        .ask_orchestrated(
            ai.search(),
            channel_id,
            &messages,
            safesearch_level,
            mode,
            move |status| {
                let _ = status_sender.try_send(status);
            },
        )
        .await?;

    let cleaned = clean_ai_response(&gen_res.content);
    let mut answer = truncate_discord_response(&cleaned);

    if answer.is_empty() {
        answer = "*(blinks blankly... no thoughts head empty)*".to_owned();
    }

    Ok((answer, gen_res.attachment))
}

async fn get_history(
    ctx: &Context,
    channel_id: Id<ChannelMarker>,
    exclude_message_id: Option<Id<MessageMarker>>,
) -> Result<Vec<Message>> {
    let limit = std::env::var("AI_HISTORY_MESSAGES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(HISTORY_LIMIT);

    let response = ctx
        .http
        .channel_messages(channel_id)
        .limit(limit)
        .await?;

    let mut messages: Vec<Message> = response.model().await?;

    if let Some(exclude_id) = exclude_message_id {
        messages.retain(|msg| msg.id != exclude_id);
    }

    if let Some(ai) = ctx.ai() {
        if let Some(cutoff) = ai.get_cutoff(channel_id).await {
            messages.retain(|msg| msg.timestamp.as_secs() > cutoff);
        }
    }

    messages.reverse();
    Ok(messages)
}

fn convert_history(history: Vec<Message>, bot_id: Id<UserMarker>) -> Vec<AiMessage> {
    let mut raw_messages: Vec<AiMessage> = Vec::with_capacity(history.len());

    for message in history {
        if message.content.trim().is_empty() {
            continue;
        }

        if message.author.bot && message.author.id != bot_id {
            continue;
        }

        let content = sanitize_llm_text(&truncate_history_message(&message.content));

        if message.author.id == bot_id {
            if content.starts_with("⏳ Waiting in AI queue")
                || content.starts_with("⏳ Thinking")
                || content.starts_with("💭 Thinking")
                || content.starts_with("🧠 **Thinking")
                || content.starts_with("🌐 Thinking")
                || content.starts_with("🖼️ Thinking")
                || content.starts_with("✍️ Thinking")
                || content.starts_with("💭 Generating")
                || content.starts_with("⚠️ ")
                || content.starts_with("🧹 ")
                || content.starts_with("🏓 Pong")
                || content.starts_with("*(blinks blankly")
                || content.starts_with("Something went wrong while asking the AI")
            {
                continue;
            }
            raw_messages.push(AiMessage::assistant(content));
        } else {
            let author_name = sanitize_llm_text(&message.author.name);
            let cleaned = strip_command_prefix(&content);
            if !cleaned.is_empty() {
                raw_messages.push(AiMessage::user(format!("{author_name}: {cleaned}")));
            }
        }
    }

    let mut coalesced: Vec<AiMessage> = Vec::with_capacity(raw_messages.len());
    for msg in raw_messages {
        if let Some(last) = coalesced.last_mut() {
            if last.role == msg.role {
                last.content.push('\n');
                last.content.push_str(&msg.content);
                continue;
            }
        }
        coalesced.push(msg);
    }

    coalesced
}

fn strip_command_prefix(content: &str) -> &str {
    let trimmed = content.trim();
    for prefix in &[
        "!ask ", "!ask", "!ai ", "!ai", "!a ", "!a",
        "/ask ", "/ask", "!render ", "!render", "!queue", "!ping"
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.trim_start();
        }
    }
    trimmed
}

fn truncate_history_message(content: &str) -> String {
    truncate_chars(content, HISTORY_MESSAGE_CHAR_LIMIT)
}

fn sanitize_llm_text(text: &str) -> String {
    text.chars()
        .filter(|&c| c == '\n' || c == '\r' || c == '\t' || !c.is_control())
        .collect::<String>()
        .replace("<|im_start|>", "")
        .replace("<|im_end|>", "")
        .replace("<|endoftext|>", "")
}

pub fn clean_ai_response(content: &str) -> String {
    let mut text = content.trim().to_owned();
    let mut unclosed_thought: Option<String> = None;

    // Repeatedly strip all complete <think>...</think> blocks
    while let Some(start) = text.find("<think>") {
        if let Some(end) = text[start..].find("</think>") {
            let end_idx = start + end + "</think>".len();
            text.drain(start..end_idx);
        } else {
            // Unclosed <think> block (e.g. model hit max tokens mid-thought):
            let thought_content = text[start + "<think>".len()..].trim().to_string();
            unclosed_thought = Some(thought_content);
            text.truncate(start);
            break;
        }
    }

    // Also strip any standalone </think> tags
    while let Some(end) = text.find("</think>") {
        let end_idx = end + "</think>".len();
        text.drain(..end_idx);
    }

    let mut cleaned = text.trim();

    // Strip ChatML / role delimiters and artifacts
    for prefix in &[
        "<|im_start|>assistant",
        "<|im_start|>",
        "<|im_end|>",
        "<|endoftext|>",
        "assistant:",
        "Assistant:",
        "### Assistant:",
        "AI:",
        "bot:",
        "Bot:",
    ] {
        while let Some(rest) = cleaned.strip_prefix(prefix) {
            cleaned = rest.trim();
        }
    }

    for suffix in &["<|im_end|>", "<|endoftext|>"] {
        while let Some(rest) = cleaned.strip_suffix(suffix) {
            cleaned = rest.trim();
        }
    }

    // Strip search and attach tool tags
    let no_tools = strip_tool_tags(cleaned);
    let mut final_text = no_tools.trim();

    // Strip any leaked prompt meta-commentary lines
    let filtered_lines: Vec<&str> = final_text
        .lines()
        .filter(|line| {
            let lower = line.trim().to_ascii_lowercase();
            !lower.starts_with("focus rule")
                && !lower.starts_with("system prompt")
                && !lower.starts_with("rule activated")
                && !lower.starts_with("critical focus")
        })
        .collect();
    let cleaned_lines = filtered_lines.join("\n");
    final_text = cleaned_lines.trim();

    if final_text.is_empty() {
        if let Some(thought) = unclosed_thought {
            if !thought.is_empty() {
                let partial = if thought.chars().count() > 1500 {
                    let skip = thought.chars().count() - 1500;
                    thought.chars().skip(skip).collect::<String>()
                } else {
                    thought
                };
                return format!(
                    "⚠️ *(Reached max token limit while reasoning—partial thought)*:\n{}",
                    format_server_emojis(partial.trim())
                );
            }
        }
    }

    format_server_emojis(final_text)
}

fn format_server_emojis(content: &str) -> String {
    let custom_tag = std::env::var("EMOJI_SKULLCAMEL")
        .unwrap_or_else(|_| "<:skullcamel:1523112299617845348>".to_owned());
    const SKULLCAMEL_SHORTCODE: &str = ":skullcamel:";

    let mut result = String::with_capacity(content.len() + 32);
    let mut remaining = content;

    while let Some(pos) = remaining.to_ascii_lowercase().find(SKULLCAMEL_SHORTCODE) {
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

fn truncate_discord_response(content: &str) -> String {
    if content.chars().count() <= DISCORD_RESPONSE_CHAR_LIMIT {
        return content.to_owned();
    }

    let mut truncated = truncate_chars(content, DISCORD_RESPONSE_CHAR_LIMIT);
    truncated.push_str("\n\n[response truncated]");
    truncated
}

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

        let unclosed = "<think>I am still thinking and got cut off";
        assert_eq!(
            clean_ai_response(unclosed),
            "⚠️ *(Reached max token limit while reasoning—partial thought)*:\nI am still thinking and got cut off"
        );

        let prefix = "Assistant: Here is the answer.";
        assert_eq!(clean_ai_response(prefix), "Here is the answer.");

        let chatml = "<|im_start|>assistant\n42<|im_end|>";
        assert_eq!(clean_ai_response(chatml), "42");
    }

    #[test]
    fn test_clean_ai_response_with_tools() {
        let with_tools = "<think>Searching...</think>Here is the info: <search>osu news</search> Peppy said hi! <attach>https://example.com/a.png</attach>";
        assert_eq!(clean_ai_response(with_tools), "Here is the info:  Peppy said hi!");
    }

    #[test]
    fn test_strip_command_prefix() {
        assert_eq!(strip_command_prefix("!ask What is osu?"), "What is osu?");
        assert_eq!(strip_command_prefix("!ai tell me a joke"), "tell me a joke");
        assert_eq!(strip_command_prefix("/ask who made you"), "who made you");
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
}
