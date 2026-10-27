use std::sync::Arc;

use command_macros::SlashCommand;
use eyre::Result;
use twilight_interactions::command::{CommandModel, CreateCommand};
use twilight_model::{
    channel::Message,
    id::{
        marker::{ChannelMarker, MessageMarker, UserMarker},
        Id,
    },
};

use crate::{
    core::{ai::AiMessage, Context},
    util::{
        builder::MessageBuilder, interaction::InteractionCommand, Authored, ChannelExt,
        InteractionCommandExt, MessageExt,
    },
};

/// Maximum number of previous Discord messages sent to the AI.
const HISTORY_LIMIT: u16 = 100;
/// Maximum number of characters taken from a single historical message.
///
/// This prevents a handful of enormous Discord messages from consuming the
/// model's entire context window.
const HISTORY_MESSAGE_CHAR_LIMIT: usize = 1_500;

/// Keep some room below Discord's 2,000-character message limit.
const DISCORD_RESPONSE_CHAR_LIMIT: usize = 1_900;

#[derive(CommandModel, CreateCommand, SlashCommand)]
#[command(name = "ask")]
#[flags(SKIP_DEFER)]
/// Ask the AI a question
pub struct Ask {
    /// What you want to ask
    question: String,
}

async fn slash_ask(ctx: Arc<Context>, mut command: InteractionCommand) -> Result<()> {
    // Parse the slash command.
    let Ask { question } = Ask::from_interaction(command.input_data())?;

    // AI generation can take a while, especially when running locally.
    //
    // Defer immediately so Discord doesn't consider the interaction timed out.
    command.defer(&ctx, false).await?;

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

    if position > 0 {
        let builder = MessageBuilder::new().content("💭 Generating response...");
        let _ = command.update(&ctx, &builder).await;
    }

    // Get the person currently talking to the AI.
    let user = command.user()?;

    let answer = match execute_ai_question(
        &ctx,
        command.channel_id,
        user.id,
        &user.name,
        &question,
        None,
        None,
    )
    .await
    {
        Ok(answer) => answer,
        Err(err) => {
            tracing::error!(
                ?err,
                user_id = %user.id,
                "AI request failed"
            );

            let builder = MessageBuilder::new()
                .content("Something went wrong while asking the AI. Please try again later.");

            command.update(&ctx, &builder).await?;

            return Err(err.wrap_err("AI request failed"));
        }
    };

    let builder = MessageBuilder::new().content(answer);
    command.update(&ctx, &builder).await?;

    Ok(())
}

/// Handle prefix `!ask` (or `!ai`, `!a`) messages.
pub async fn handle_prefix_ask(
    ctx: Arc<Context>,
    msg: &Message,
    args_str: &str,
    prefix: &str,
) {
    let mut question = args_str.trim().to_string();

    let bot_id = ctx.cache.current_user(|u| u.id).ok();
    let is_reply_to_ai = msg
        .referenced_message
        .as_deref()
        .map(|r| Some(r.author.id) == bot_id)
        .unwrap_or(false);

    // If the message is a reply to another human message (not the AI), include that as context
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
            let _ = msg
                .error(
                    &ctx,
                    format!("Please provide a question! Example: `{prefix}ask What is osu?`"),
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
        "💭 Thinking...".to_string()
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

    // Wait until it is this request's turn
    let _permit = ticket.wait().await;

    if position > 0 {
        if let Some(ref ph) = placeholder {
            let _ = ph
                .update(&ctx, &MessageBuilder::new().content("💭 Thinking..."))
                .await;
        }
    }

    let _ = ctx.http.create_typing_trigger(msg.channel_id).await;

    let result = execute_ai_question(
        &ctx,
        msg.channel_id,
        msg.author.id,
        &msg.author.name,
        &question,
        Some(msg.id),
        msg.referenced_message.as_deref(),
    )
    .await;

    match result {
        Ok(answer) => {
            if let Some(ref ph) = placeholder {
                if let Err(err) = ph
                    .update(&ctx, &MessageBuilder::new().content(answer.clone()))
                    .await
                {
                    tracing::warn!(?err, "failed to update placeholder message, sending new message");
                    let _ = msg
                        .channel_id
                        .create_message(
                            &ctx,
                            &MessageBuilder::new().content(answer).reply(msg.id),
                        )
                        .await;
                }
            } else {
                let _ = msg
                    .channel_id
                    .create_message(
                        &ctx,
                        &MessageBuilder::new().content(answer).reply(msg.id),
                    )
                    .await;
            }
        }
        Err(err) => {
            tracing::error!(?err, user_id = %msg.author.id, "prefix AI request failed");
            let error_content = "Something went wrong while asking the AI. Please try again later.";
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

/// Core function to formulate messages and query the AI backend.
pub async fn execute_ai_question(
    ctx: &Context,
    channel_id: Id<ChannelMarker>,
    user_id: Id<UserMarker>,
    username: &str,
    question: &str,
    exclude_message_id: Option<Id<MessageMarker>>,
    referenced_msg: Option<&Message>,
) -> Result<String> {
    // Get our own Discord user ID.
    //
    // We need this so previous responses from this bot can be sent to the
    // model with role="assistant" instead of pretending another user wrote
    // them.
    let bot_id = ctx
        .cache
        .current_user(|user| user.id)
        .expect("missing CurrentUser in cache");

    // Get the last N messages from this channel, filtered by context cutoff.
    let history = get_history(ctx, channel_id, exclude_message_id).await?;

    // Get the cached system prompt template from AiClient.
    let system_prompt_template = ctx.ai().get_prompt().await;

    let clean_username = sanitize_llm_text(username);
    let clean_question = sanitize_llm_text(question);

    // Explicitly tell the model who is currently speaking.
    let mut system_prompt = system_prompt_template
        .replace("{username}", &clean_username)
        .replace("{user_id}", &user_id.to_string());

    let current_user_msg = format!(
        "{clean_username} (Discord ID {user_id}): {clean_question}"
    );

    let messages = if let Some(replied) = referenced_msg {
        if replied.author.id == bot_id {
            // User is directly replying to a message sent by the AI!
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

            // Focus context: Take messages up to the replied AI message, excluding intervening chatter
            let history_before_ai = if let Some(idx) = history.iter().position(|m| m.id == replied.id) {
                history[..idx].to_vec()
            } else {
                history.into_iter().filter(|m| m.id != replied.id).collect()
            };

            let mut history_messages = convert_history(history_before_ai, bot_id);

            // Put the exact AI message being replied to as the immediate prior assistant turn
            if !ai_content.is_empty() {
                history_messages.push(AiMessage::assistant(ai_content));
            }

            system_prompt.push_str(
                "\n\nCONVERSATION CONTEXT: The user is directly replying to your previous statement above. Focus your response specifically on addressing what they said in direct relation to your statement."
            );

            let mut msgs = Vec::with_capacity(history_messages.len() + 2);
            msgs.push(AiMessage::system(system_prompt));
            msgs.extend(history_messages);
            msgs.push(AiMessage::user(current_user_msg));
            msgs
        } else {
            // Replying to another user
            let history_messages = convert_history(history, bot_id);
            let mut msgs = Vec::with_capacity(history_messages.len() + 2);
            msgs.push(AiMessage::system(system_prompt));
            msgs.extend(history_messages);
            msgs.push(AiMessage::user(current_user_msg));
            msgs
        }
    } else {
        // Standalone question
        let history_messages = convert_history(history, bot_id);
        let mut msgs = Vec::with_capacity(history_messages.len() + 2);
        msgs.push(AiMessage::system(system_prompt));
        msgs.extend(history_messages);
        msgs.push(AiMessage::user(current_user_msg));
        msgs
    };

    // Ask whichever AI backend is configured in AiClient.
    let answer = ctx.ai().ask(channel_id, &messages).await?;

    // Remove role prefixes that some local models generate.
    let answer = clean_ai_response(&answer);

    // Truncate if exceeding Discord limit.
    let mut answer = truncate_discord_response(&answer);

    if answer.is_empty() {
        answer = "*(blinks blankly... no thoughts head empty)*".to_owned();
    }

    Ok(answer)
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

/// Convert normal Discord messages into messages suitable for the AI.
///
/// Human messages become:
///
///     role: user
///     Alice (Discord ID 123): hello
///
/// Previous messages written by this bot become:
///
///     role: assistant
///     Hello! How can I help?
///
/// Messages from unrelated bots are ignored.
/// Consecutive messages with the same role are coalesced to enforce
/// strict alternating user <-> assistant dialogue expected by LLM chat templates.
fn convert_history(history: Vec<Message>, bot_id: Id<UserMarker>) -> Vec<AiMessage> {
    let mut raw_messages: Vec<AiMessage> = Vec::with_capacity(history.len());

    for message in history {
        // Ignore messages without textual content.
        if message.content.trim().is_empty() {
            continue;
        }

        // Ignore messages from unrelated bots.
        if message.author.bot && message.author.id != bot_id {
            continue;
        }

        let content = sanitize_llm_text(&truncate_history_message(&message.content));

        if message.author.id == bot_id {
            // Filter out non-conversational bot messages (status, queue notices, clear confirmations)
            if content.starts_with("⏳ Waiting in AI queue")
                || content.starts_with("💭 Thinking")
                || content.starts_with("💭 Generating")
                || content.starts_with("⚠️ ")
                || content.starts_with("🧹 ")
                || content.starts_with("🏓 Pong")
                || content.starts_with("*(blinks blankly")
                || content.starts_with("Something went wrong while asking the AI")
            {
                continue;
            }
            // Previous AI response.
            raw_messages.push(AiMessage::assistant(content));
        } else {
            // Human Discord message.
            let author_name = sanitize_llm_text(&message.author.name);
            let cleaned = strip_command_prefix(&content);
            if !cleaned.is_empty() {
                raw_messages.push(AiMessage::user(format!(
                    "{} (Discord ID {}): {}",
                    author_name, message.author.id, cleaned,
                )));
            }
        }
    }

    // Coalesce consecutive messages of the same role so the model receives
    // a clean, strictly alternating dialogue without repeated role headers.
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

/// Strip leading command prefix (e.g. `!ask `, `!ai `, `/ask `) from historical user messages
/// so historical conversation text reads naturally for the model.
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

/// Limit how much of an individual historical Discord message is supplied to
/// the model.
fn truncate_history_message(content: &str) -> String {
    truncate_chars(content, HISTORY_MESSAGE_CHAR_LIMIT)
}

/// Sanitize text for LLM prompts by stripping null bytes, invalid control characters,
/// and prompt template delimiters that could confuse ChatML / LLM tokenizer.
fn sanitize_llm_text(text: &str) -> String {
    text.chars()
        .filter(|&c| c == '\n' || c == '\r' || c == '\t' || !c.is_control())
        .collect::<String>()
        .replace("<|im_start|>", "")
        .replace("<|im_end|>", "")
        .replace("<|endoftext|>", "")
}

fn clean_ai_response(content: &str) -> String {
    let mut text = content.trim().to_owned();

    // Repeatedly strip all complete <think>...</think> blocks
    while let Some(start) = text.find("<think>") {
        if let Some(end) = text[start..].find("</think>") {
            let end_idx = start + end + "</think>".len();
            text.drain(start..end_idx);
        } else {
            // Unclosed <think> block (e.g. model was cut off mid-thought):
            // Strip from <think> to the end
            text.truncate(start);
            break;
        }
    }

    // Also strip any standalone </think> tags
    while let Some(end) = text.find("</think>") {
        let end_idx = end + "</think>".len();
        text.drain(..end_idx);
    }

    let mut content = text.trim();

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

/// Limit the initial AI response so it fits safely inside one Discord message.
///
/// Later you can replace this with response chunking and send multiple
/// follow-up messages instead.
fn truncate_discord_response(content: &str) -> String {
    if content.chars().count() <= DISCORD_RESPONSE_CHAR_LIMIT {
        return content.to_owned();
    }

    let mut truncated = truncate_chars(content, DISCORD_RESPONSE_CHAR_LIMIT);

    truncated.push_str("\n\n[response truncated]");

    truncated
}

/// UTF-8 safe character truncation using char_indices.
///
/// Ensures slicing always occurs on a valid UTF-8 character boundary.
fn truncate_chars(content: &str, max_chars: usize) -> String {
    let mut count = 0;
    for (idx, _) in content.char_indices() {
        if count == max_chars {
            return content[..idx].to_owned();
        }
        count += 1;
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
        assert_eq!(clean_ai_response(unclosed), "");

        let prefix = "Assistant: Here is the answer.";
        assert_eq!(clean_ai_response(prefix), "Here is the answer.");

        let chatml = "<|im_start|>assistant\n42<|im_end|>";
        assert_eq!(clean_ai_response(chatml), "42");
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

    #[test]
    fn test_message_builder_reply() {
        use twilight_model::id::Id;
        let msg_id = Id::new(123456789);
        let builder = MessageBuilder::new().content("hello").reply(msg_id);
        assert_eq!(builder.reply, Some(msg_id));
        assert_eq!(builder.content.as_deref(), Some("hello"));
    }
}
