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
const HISTORY_LIMIT: u16 = 20;
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

    // If the message is a reply to another message, include that as context
    if let Some(ref replied) = msg.referenced_message {
        let ref_author = &replied.author.name;
        let ref_content = truncate_history_message(&replied.content);
        if !ref_content.is_empty() {
            if question.is_empty() {
                question = format!("What do you think about what {ref_author} said: \"{ref_content}\"?");
            } else {
                question = format!("(In reply to {ref_author}: \"{ref_content}\")\n{question}");
            }
        }
    }

    if question.is_empty() {
        let _ = msg
            .error(
                &ctx,
                format!("Please provide a question! Example: `{prefix}ask What is osu?`"),
            )
            .await;
        return;
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
        .create_message(&ctx, &MessageBuilder::new().content(initial_content))
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
                        .create_message(&ctx, &MessageBuilder::new().content(answer))
                        .await;
                }
            } else {
                let _ = msg
                    .channel_id
                    .create_message(&ctx, &MessageBuilder::new().content(answer))
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

    // Get the last 20 messages from this channel, filtered by context cutoff.
    let history = get_history(ctx, channel_id, exclude_message_id).await?;

    // Get the cached system prompt template from AiClient.
    let system_prompt_template = ctx.ai().get_prompt().await;

    // Explicitly tell the model who is currently speaking.
    let system_prompt = system_prompt_template
        .replace("{username}", username)
        .replace("{user_id}", &user_id.to_string());

    // system + 20 history messages + current question
    let mut messages = Vec::with_capacity(HISTORY_LIMIT as usize + 2);

    messages.push(AiMessage::system(system_prompt));

    // Add the previous Discord messages.
    messages.extend(convert_history(history, bot_id));

    // The current question is always added LAST.
    messages.push(AiMessage::user(format!(
        "{username} (Discord ID {user_id}): {question}"
    )));

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
    let response = ctx
        .http
        .channel_messages(channel_id)
        .limit(HISTORY_LIMIT)
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
fn convert_history(history: Vec<Message>, bot_id: Id<UserMarker>) -> Vec<AiMessage> {
    history
        .into_iter()
        // Ignore messages without textual content.
        .filter(|message| !message.content.trim().is_empty())
        // Keep:
        //
        // 1. Human messages
        // 2. Our bot's messages
        //
        // Ignore messages from unrelated bots.
        .filter(|message| !message.author.bot || message.author.id == bot_id)
        .filter_map(|message| {
            let content = truncate_history_message(&message.content);

            if message.author.id == bot_id {
                // Filter out non-conversational bot messages (status, queue notices, clear confirmations)
                if content.starts_with("⏳ Waiting in AI queue")
                    || content.starts_with("💭 Thinking")
                    || content.starts_with("💭 Generating")
                    || content.starts_with("⚠️ ")
                    || content.starts_with("🧹 ")
                    || content.starts_with("🏓 Pong")
                {
                    return None;
                }
                // Previous AI response.
                Some(AiMessage::assistant(content))
            } else {
                // Human Discord message.
                Some(AiMessage::user(format!(
                    "{} (Discord ID {}): {}",
                    message.author.name, message.author.id, content,
                )))
            }
        })
        .collect()
}

/// Limit how much of an individual historical Discord message is supplied to
/// the model.
fn truncate_history_message(content: &str) -> String {
    truncate_chars(content, HISTORY_MESSAGE_CHAR_LIMIT)
}

fn clean_ai_response(content: &str) -> String {
    let mut content = content.trim();

    // Strip internal thought blocks if generated into content
    if let Some(end) = content.find("</think>") {
        content = content[end + 8..].trim();
    } else if let Some(start) = content.find("<think>") {
        if start == 0 {
            content = "";
        }
    }

    for prefix in &[
        "assistant:",
        "Assistant:",
        "### Assistant:",
        "AI:",
        "bot:",
        "Bot:",
    ] {
        if let Some(rest) = content.strip_prefix(prefix) {
            content = rest.trim();
        }
    }

    content.to_owned()
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

/// UTF-8 safe character truncation.
///
/// Do NOT use:
///
///     &content[..1500]
///
/// because Rust string indices are byte offsets and doing that can panic in
/// the middle of a multi-byte UTF-8 character.
fn truncate_chars(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        return content.to_owned();
    }

    content.chars().take(max_chars).collect()
}
