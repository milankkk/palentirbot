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
    _username: &str,
    question: &str,
    _exclude_message_id: Option<Id<MessageMarker>>,
    referenced_msg: Option<&Message>,
    mode: AiMode,
    status_sender: mpsc::Sender<AiStatusUpdate>,
) -> Result<(String, Option<(String, Vec<u8>)>)> {
    let ai = ctx.ai().ok_or_else(|| eyre::eyre!("AI is not initialized"))?;

    let bot_id = ctx
        .cache
        .current_user(|user| user.id)
        .expect("missing CurrentUser in cache");

    let raw_prompt = ai.get_prompt().await;
    let system_prompt = match mode {
        AiMode::Fast => format!(
            "{raw_prompt}\n\n[Mode: Fast Mode]\nBe direct, concise, and to the point. Reflect the user's question directly. If external facts or news are needed, immediately emit <search>exact user keywords</search> without hesitation."
        ),
        AiMode::Thinking => format!(
            "{raw_prompt}\n\n[Mode: Thinking Mode]\nReason thoroughly and critically inside your thoughts. Doubt your memory, search with exact user keywords, verify facts, close with </think>, and formulate your grounded response."
        ),
    };

    let clean_question = sanitize_llm_text(question);

    let is_reply_to_ai = referenced_msg.map(|r| r.author.id == bot_id).unwrap_or(false);

    let messages = if is_reply_to_ai {
        let replied = referenced_msg.unwrap();
        let chain = build_ai_reply_chain(ctx, channel_id, guild_id, bot_id, replied).await;
        let mut msgs = Vec::with_capacity(chain.len() + 2);
        msgs.push(AiMessage::system(system_prompt));
        msgs.extend(chain);
        msgs.push(AiMessage::user(clean_question));
        msgs
    } else {
        vec![
            AiMessage::system(system_prompt),
            AiMessage::user(clean_question),
        ]
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

/// Recursively builds the conversation chain for a reply to an AI message.
/// Traces backwards up to 6 messages (3 round-trips) in the reply thread.
async fn build_ai_reply_chain(
    ctx: &Context,
    channel_id: Id<ChannelMarker>,
    guild_id: Option<Id<GuildMarker>>,
    bot_id: Id<UserMarker>,
    initial_replied: &Message,
) -> Vec<AiMessage> {
    let cutoff = if let Some(ai) = ctx.ai() {
        ai.get_cutoff(channel_id, guild_id).await
    } else {
        None
    };

    let mut chain: Vec<AiMessage> = Vec::new();
    let mut current_msg: Option<Message> = Some(initial_replied.clone());
    let mut depth = 0;
    const MAX_DEPTH: usize = 6;

    while let Some(msg) = current_msg {
        if depth >= MAX_DEPTH {
            break;
        }
        depth += 1;

        if let Some(c) = cutoff {
            if msg.timestamp.as_secs() <= c {
                break;
            }
        }

        if msg.author.id == bot_id {
            let mut raw_text = msg.content.clone();
            if raw_text.trim().is_empty() {
                for embed in &msg.embeds {
                    if let Some(ref desc) = embed.description {
                        raw_text.push_str(desc);
                        raw_text.push(' ');
                    }
                    if let Some(ref title) = embed.title {
                        raw_text.push_str(title);
                        raw_text.push(' ');
                    }
                }
            }
            let ai_content = sanitize_llm_text(&clean_ai_response(&truncate_history_message(&raw_text)));
            if !ai_content.is_empty() && !is_transient_status(&ai_content) {
                chain.push(AiMessage::assistant(ai_content));
            }
        } else {
            let user_content = sanitize_llm_text(&strip_command_prefix(&truncate_history_message(&msg.content)));
            if !user_content.is_empty() && !is_non_ai_command(&msg.content) {
                chain.push(AiMessage::user(user_content));
            }
        }

        let parent_msg = if let Some(ref parent) = msg.referenced_message {
            Some((**parent).clone())
        } else if let Some(parent_id) = msg.reference.as_ref().and_then(|r| r.message_id) {
            match ctx.http.message(channel_id, parent_id).await {
                Ok(resp) => resp.model().await.ok(),
                Err(_) => None,
            }
        } else {
            None
        };

        current_msg = parent_msg;
    }

    chain.reverse();
    chain
}

fn is_transient_status(content: &str) -> bool {
    content.starts_with("⏳ Waiting in AI queue")
        || content.starts_with("⏳ Thinking")
        || content.starts_with("💭 Thinking")
        || content.starts_with("🧠 **Thinking")
        || content.starts_with("🌐 Searching")
        || content.starts_with("🌐 Thinking")
        || content.starts_with("🌐 Browsing")
        || content.starts_with("🖼️ Thinking")
        || content.starts_with("🖼️ Downloading")
        || content.starts_with("✍️ Thinking")
        || content.starts_with("⚡ Generating")
        || content.starts_with("💭 Generating")
        || content.starts_with("⚠️ ")
        || content.starts_with("🧹 ")
        || content.starts_with("🏓 Pong")
        || content.starts_with("*(blinks blankly")
        || content.starts_with("Something went wrong while asking the AI")
        || content.contains("replays.insertdomainname.be")
        || content.contains("Finished rendering replay")
        || content.contains("Started replay processing")
}

fn is_non_ai_command(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    for prefix in &[
        "!render", "!r ", "/render", "!queue", "!q ", "!skin", "!danser",
        "!settings", "!setup", "!ping", "!help", "!np", "!replay",
    ] {
        if lower.starts_with(prefix) {
            return true;
        }
    }
    false
}

fn strip_command_prefix(content: &str) -> &str {
    let trimmed = content.trim();
    for prefix in &[
        "!ask ", "!ask", "!ai ", "!ai", "!a ", "!a",
        "!fast ", "!fast", "/ask ", "/ask", "/fast ", "/fast",
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
            let trimmed = thought.trim();
            if !trimmed.is_empty() {
                let partial = if trimmed.chars().count() > 1500 {
                    let skip = trimmed.chars().count() - 1500;
                    trimmed.chars().skip(skip).collect::<String>()
                } else {
                    trimmed.to_string()
                };
                return format_server_emojis(partial.trim());
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
            "I am still thinking and got cut off"
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
    fn test_is_non_ai_command() {
        assert!(is_non_ai_command("!render https://example.com/replay.osr"));
        assert!(is_non_ai_command("!queue"));
        assert!(is_non_ai_command("!skin"));
        assert!(!is_non_ai_command("!ask what is osu?"));
        assert!(!is_non_ai_command("!fast hello"));
        assert!(!is_non_ai_command("can you render this?"));
    }

    #[test]
    fn test_is_transient_status() {
        assert!(is_transient_status("⏳ Thinking (Loading model into memory...)"));
        assert!(is_transient_status("🧠 **Thinking...** (elapsed: 3s)\n> *reasoning*"));
        assert!(is_transient_status("🌐 Searching web (DuckDuckGo)..."));
        assert!(is_transient_status("⚡ Generating response..."));
        assert!(is_transient_status("*(blinks blankly... no thoughts head empty)*"));
        assert!(!is_transient_status("osu! was created in 2007 by peppy."));
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
