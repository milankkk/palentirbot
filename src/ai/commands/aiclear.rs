use std::sync::Arc;

use command_macros::SlashCommand;
use eyre::Result;
use twilight_interactions::command::{CommandModel, CommandOption, CreateCommand, CreateOption};

use crate::{
    core::Context,
    util::{
        builder::MessageBuilder, interaction::InteractionCommand, InteractionCommandExt,
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
#[command(name = "aiclear")]
#[flags(AUTHORITY, SKIP_DEFER)]
/// Clear the AI's conversation context or reload the prompt (admins & owner)
pub struct AiClear {
    /// What action to perform (defaults to clearing context and reloading prompt)
    action: Option<AiClearAction>,
}

async fn slash_aiclear(ctx: Arc<Context>, mut command: InteractionCommand) -> Result<()> {
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

    let AiClear { action } = AiClear::from_interaction(command.input_data())?;
    let channel_id = command.channel_id;

    let do_context = match action {
        None | Some(AiClearAction::Both) | Some(AiClearAction::ContextOnly) => true,
        Some(AiClearAction::PromptOnly) => false,
    };

    let do_prompt = match action {
        None | Some(AiClearAction::Both) | Some(AiClearAction::PromptOnly) => true,
        Some(AiClearAction::ContextOnly) => false,
    };

    if do_context {
        // Set context cutoff for this channel so older messages are ignored
        ai.clear_context(channel_id).await;
    }

    let mut response = String::new();

    if do_context {
        response.push_str("🧹 AI conversation context cleared for this channel.\n");
    }

    if do_prompt {
        match ai.reload_prompt().await {
            Ok(()) => {
                response.push_str("🔄 System prompt successfully reloaded from file.");
            }
            Err(err) => {
                tracing::error!(?err, "failed to reload AI prompt");
                response.push_str("⚠️ Failed to reload system prompt from file.");
            }
        }
    }

    let queued = ai.queue_len().await;
    if queued > 0 {
        response.push_str(&format!("\nℹ️ Note: {queued} request(s) currently in the AI queue."));
    }

    let builder = MessageBuilder::new().content(response.trim_end());
    command.callback(&ctx, builder, true).await?;

    Ok(())
}
