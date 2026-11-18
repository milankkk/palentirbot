use std::sync::Arc;

use command_macros::SlashCommand;
use eyre::Result;
use twilight_interactions::command::{CommandModel, CommandOption, CreateCommand, CreateOption};

use crate::{
    core::{BotConfig, Context},
    util::{
        builder::MessageBuilder, interaction::InteractionCommand, Authored, InteractionCommandExt,
    },
};

#[derive(Copy, Clone, CommandOption, CreateOption, Eq, PartialEq)]
pub enum AiClearAction {
    #[option(name = "Clear context and reload prompt", value = "both")]
    Both,
    #[option(name = "Clear context only", value = "context")]
    ContextOnly,
    #[option(name = "Reload prompt only", value = "prompt")]
    PromptOnly,
}

#[derive(CommandModel, CreateCommand, SlashCommand)]
#[command(name = "aiclear", dm_permission = false)]
#[flags(AUTHORITY, SKIP_DEFER)]
/// Clear the AI's conversation context or reload the prompt (admins & owner)
pub struct AiClear {
    /// What action to perform (defaults to clearing context and reloading prompt)
    action: Option<AiClearAction>,
}

async fn slash_aiclear(ctx: Arc<Context>, mut command: InteractionCommand) -> Result<()> {
    let AiClear { action } = AiClear::from_interaction(command.input_data())?;
    let user_id = command.user_id()?;
    let is_owner = BotConfig::get().owners.contains(&user_id);
    let channel_id = command.channel_id;

    if matches!(action, Some(AiClearAction::PromptOnly)) && !is_owner {
        command
            .error_callback(
                &ctx,
                "Only the bot owner can reload `prompt.txt`.",
                true,
            )
            .await?;
        return Ok(());
    }

    let do_context = match action {
        None | Some(AiClearAction::Both) | Some(AiClearAction::ContextOnly) => true,
        Some(AiClearAction::PromptOnly) => false,
    };

    let do_prompt = match action {
        None | Some(AiClearAction::Both) | Some(AiClearAction::PromptOnly) => is_owner,
        Some(AiClearAction::ContextOnly) => false,
    };

    if do_context {
        // Set context cutoff for this channel so older messages are ignored
        ctx.ai().clear_context(channel_id).await;
    }

    let mut response = String::new();

    if do_context {
        response.push_str("🧹 AI conversation context cleared for this channel.\n");
    }

    if do_prompt {
        match ctx.ai().reload_prompt().await {
            Ok(()) => {
                response.push_str("🔄 System prompt successfully reloaded from file.");
            }
            Err(err) => {
                tracing::error!(?err, "failed to reload AI prompt");
                response.push_str("⚠️ Failed to reload system prompt from file.");
            }
        }
    } else if matches!(action, None | Some(AiClearAction::Both)) {
        response.push_str("ℹ️ System prompt reload is restricted to the bot owner.");
    }

    let queued = ctx.ai().queue_len().await;
    if queued > 0 {
        response.push_str(&format!("\nℹ️ Note: {queued} request(s) currently in the AI queue."));
    }

    let builder = MessageBuilder::new().content(response.trim_end());
    command.callback(&ctx, builder, true).await?;

    Ok(())
}
