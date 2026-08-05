use std::sync::Arc;

use command_macros::SlashCommand;
use eyre::{Context as _, Result};
use twilight_interactions::command::{CommandModel, CreateCommand};
use twilight_model::{
    channel::Message,
    id::{
        marker::{ChannelMarker, UserMarker},
        Id,
    },
};

use crate::{
    core::{ai::AiMessage, Context},
    util::{
        builder::MessageBuilder, interaction::InteractionCommand, Authored, InteractionCommandExt,
    },
};

/// Maximum number of previous Discord messages sent to the AI.
const HISTORY_LIMIT: u16 = 20;
const SYSTEM_PROMPT_ENV: &str = "AI_SYSTEM_PROMPT";
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

    // Get the person currently talking to the AI.
    let user = command.user()?;

    let username = user.name.clone();
    let user_id = user.id;

    // Get our own Discord user ID.
    //
    // We need this so previous responses from this bot can be sent to the
    // model with role="assistant" instead of pretending another user wrote
    // them.
    let bot_id = ctx
        .cache
        .current_user(|user| user.id)
        .expect("missing CurrentUser in cache");

    // Get the last 20 messages from this channel.
    let history = get_history(&ctx, command.channel_id).await?;

    //for message in &history {
    //    tracing::warn!(
    //        author = %message.author.name,
    //        bot = message.author.bot,
    //        content = %message.content,
    //        "AI HISTORY"
    //    );
    //}
    let system_prompt_template = std::env::var(SYSTEM_PROMPT_ENV).with_context(|| {
        format!("missing `{SYSTEM_PROMPT_ENV}`; add it to .env before using /ask")
    })?;
    // Explicitly tell the model who is currently speaking.
    let system_prompt = system_prompt_template
        .replace("{username}", &username)
        .replace("{user_id}", &user_id.to_string());

    // system + 20 history messages + current question
    let mut messages = Vec::with_capacity(HISTORY_LIMIT as usize + 2);

    messages.push(AiMessage::system(system_prompt));

    // Add the previous Discord messages.
    messages.extend(convert_history(history, bot_id));

    // IMPORTANT:
    //
    // The current slash-command question is always added LAST.
    //
    // This tells the model that this is the person who is currently talking
    // directly to it.
    messages.push(AiMessage::user(format!(
        "{username} (Discord ID {user_id}): {question}"
    )));

    // Ask whichever AI backend is configured in AiClient.
    let answer = match ctx.ai().ask(&messages).await {
        Ok(answer) => answer,

        Err(err) => {
            tracing::error!(
                ?err,
                user_id = %user_id,
                "AI request failed"
            );

            let builder = MessageBuilder::new()
                .content("Something went wrong while asking the AI. Please try again later.");

            command.update(&ctx, &builder).await?;

            return Err(err.wrap_err("AI request failed"));
        }
    };
    // Remove role prefixes that some local models generate.
    let answer = clean_ai_response(&answer);

    // For now we truncate responses instead of trying to send multiple Discord
    // messages. You can replace this later with proper response chunking.
    let answer = truncate_discord_response(&answer);

    let builder = MessageBuilder::new().content(answer);

    // The interaction was already deferred, so UPDATE the deferred response
    // instead of calling callback().
    command.update(&ctx, &builder).await?;

    Ok(())
}

/// Retrieve the previous Discord messages for the AI's conversation context.
///
/// Discord returns channel history newest -> oldest, while an LLM should
/// receive conversation history oldest -> newest, so the vector is reversed.
async fn get_history(ctx: &Context, channel_id: Id<ChannelMarker>) -> Result<Vec<Message>> {
    let response = ctx
        .http
        .channel_messages(channel_id)
        .limit(HISTORY_LIMIT)
        .await?;

    let mut messages: Vec<Message> = response.model().await?;

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
        .map(|message| {
            let content = truncate_history_message(&message.content);

            if message.author.id == bot_id {
                // Previous AI response.
                AiMessage::assistant(content)
            } else {
                // Human Discord message.
                AiMessage::user(format!(
                    "{} (Discord ID {}): {}",
                    message.author.name, message.author.id, content,
                ))
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
    let content = content.trim();

    content
        .strip_prefix("assistant:")
        .or_else(|| content.strip_prefix("Assistant:"))
        .map(str::trim)
        .unwrap_or(content)
        .to_owned()
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
