use std::sync::Arc;

use eyre::Result;
use twilight_interactions::command::{
    AutocompleteValue, CommandModel, CommandOption, CreateCommand, CreateOption,
};
use twilight_model::{
    application::command::{CommandOptionChoice, CommandOptionChoiceValue},
    id::{marker::ChannelMarker, Id},
};

use crate::{
    core::{
        ai::{AiPersonaMode, RuntimeAiConfig},
        BotConfig, Context,
    },
    util::{
        builder::{EmbedBuilder, FooterBuilder, MessageBuilder},
        interaction::InteractionCommand,
        Authored, InteractionCommandExt,
    },
};

#[derive(CommandModel, CreateCommand)]
#[command(name = "ai")]
/// Manage Ollama AI runtime configuration and per-channel persona
pub enum OwnerAi {
    #[command(name = "status")]
    Status(OwnerAiStatus),
    #[command(name = "model")]
    Model(OwnerAiModel),
    #[command(name = "think")]
    Think(OwnerAiThink),
    #[command(name = "temp")]
    Temp(OwnerAiTemp),
    #[command(name = "ctx")]
    Ctx(OwnerAiCtx),
    #[command(name = "reload_prompt")]
    ReloadPrompt(OwnerAiReloadPrompt),
    #[command(name = "channel_mode")]
    ChannelMode(OwnerAiChannelMode),
}

#[derive(CommandModel)]
#[allow(dead_code)]
pub enum OwnerAiParsable {
    #[command(name = "status")]
    Status(OwnerAiStatus),
    #[command(name = "model")]
    Model(OwnerAiModelAutocomplete),
    #[command(name = "think")]
    Think(OwnerAiThink),
    #[command(name = "temp")]
    Temp(OwnerAiTemp),
    #[command(name = "ctx")]
    Ctx(OwnerAiCtx),
    #[command(name = "reload_prompt")]
    ReloadPrompt(OwnerAiReloadPrompt),
    #[command(name = "channel_mode")]
    ChannelMode(OwnerAiChannelMode),
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "status")]
/// Inspect current AI runtime settings and channel overrides
pub struct OwnerAiStatus;

#[derive(CommandModel, CreateCommand)]
#[command(name = "model")]
/// Switch the active Ollama model at runtime
pub struct OwnerAiModel {
    /// Model name installed in Ollama
    #[command(autocomplete = true)]
    pub name: String,
}

#[derive(CommandModel)]
#[command(autocomplete = true)]
pub struct OwnerAiModelAutocomplete {
    pub name: AutocompleteValue<String>,
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "think")]
/// Toggle reasoning pass (think=true/false) on Ollama requests
pub struct OwnerAiThink {
    /// Whether extended thinking is enabled before responding
    pub enabled: bool,
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "temp")]
/// Set sampling temperature (0.0 - 2.0)
pub struct OwnerAiTemp {
    #[command(min_value = 0.0, max_value = 2.0)]
    /// Sampling temperature
    pub value: f64,
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "ctx")]
/// Set Ollama context window token limit (num_ctx)
pub struct OwnerAiCtx {
    #[command(min_value = 2048, max_value = 131072)]
    /// Context window size in tokens
    pub tokens: i64,
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "reload_prompt")]
/// Reload prompt.txt from disk without restarting the bot
pub struct OwnerAiReloadPrompt;

#[derive(Clone, Copy, CommandOption, CreateOption)]
pub enum SlashPersonaMode {
    #[option(name = "Default (prompt.txt)", value = "default")]
    Default,
    #[option(name = "Helpful (clear & direct)", value = "helpful")]
    Helpful,
    #[option(name = "Brainrot (extra chaotic)", value = "brainrot")]
    Brainrot,
}

impl From<SlashPersonaMode> for AiPersonaMode {
    fn from(mode: SlashPersonaMode) -> Self {
        match mode {
            SlashPersonaMode::Default => Self::Default,
            SlashPersonaMode::Helpful => Self::Helpful,
            SlashPersonaMode::Brainrot => Self::Brainrot,
        }
    }
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "channel_mode")]
/// Set AI personality override for a channel (defaults to current channel)
pub struct OwnerAiChannelMode {
    /// Persona mode to apply
    pub mode: SlashPersonaMode,
    /// Target channel (omit to apply to current channel)
    pub channel: Option<Id<ChannelMarker>>,
}

fn format_runtime_summary(cfg: &RuntimeAiConfig, channel_mode: AiPersonaMode, queue_len: usize, prompt_chars: usize) -> String {
    format!(
        "• **Model:** `{}`\n\
         • **Thinking:** `{}`\n\
         • **Temperature:** `{:.2}`\n\
         • **Context Window (`num_ctx`):** `{}` tokens\n\
         • **This Channel Mode:** `{channel_mode}`\n\
         • **Channel Overrides:** `{}` active\n\
         • **System Prompt Length:** `{prompt_chars}` chars\n\
         • **Queue:** `{queue_len}` waiting/active",
        cfg.model,
        if cfg.think_enabled { "Enabled" } else { "Disabled" },
        cfg.temperature,
        cfg.num_ctx,
        cfg.channel_modes.len(),
    )
}

async fn reply_embed(
    ctx: &Context,
    command: &InteractionCommand,
    title: impl Into<String>,
    description: impl Into<String>,
    footer: Option<&str>,
) -> Result<()> {
    let mut builder = EmbedBuilder::new().title(title).description(description);
    if let Some(f) = footer {
        builder = builder.footer(FooterBuilder::new(f));
    }
    command
        .callback(ctx, MessageBuilder::new().embed(builder.build()), false)
        .await?;
    Ok(())
}

pub async fn handle_owner_ai(
    ctx: Arc<Context>,
    command: InteractionCommand,
    action: OwnerAi,
) -> Result<()> {
    match action {
        OwnerAi::Status(_) => {
            let cfg = ctx.ai().runtime_config();
            let ch_mode = ctx.ai().channel_mode(command.channel_id);
            let queue_len = ctx.ai().queue_len().await;
            let prompt_chars = ctx.ai().get_prompt().await.chars().count();
            let desc = format_runtime_summary(&cfg, ch_mode, queue_len, prompt_chars);
            reply_embed(&ctx, &command, "AI Runtime Status", desc, Some("Persisted in data/ai_runtime.json")).await
        }
        OwnerAi::Model(OwnerAiModel { name }) => {
            let next_model = name.trim().to_owned();
            if next_model.is_empty() {
                command
                    .error_callback(&ctx, "Model name cannot be empty.", true)
                    .await?;
                return Ok(());
            }

            let installed = ctx.ai().list_models().await.unwrap_or_default();
            let cfg = ctx
                .ai()
                .update_runtime_config(|c| c.model = next_model.clone())
                .await?;

            let note = if !installed.is_empty() && !installed.iter().any(|m| m == &cfg.model) {
                format!(
                    "Switched active model to `{}`.\n⚠️ Note: Ollama `/api/tags` did not list this exact tag right now — ensure it is pulled if requests fail.",
                    cfg.model
                )
            } else {
                format!("Switched active model to `{}`.", cfg.model)
            };
            reply_embed(&ctx, &command, "AI Model Updated", note, None).await
        }
        OwnerAi::Think(OwnerAiThink { enabled }) => {
            let cfg = ctx
                .ai()
                .update_runtime_config(|c| c.think_enabled = enabled)
                .await?;
            reply_embed(
                &ctx,
                &command,
                "AI Thinking Updated",
                format!("Reasoning (`think`) is now **{}**.", if cfg.think_enabled { "Enabled" } else { "Disabled" }),
                None,
            )
            .await
        }
        OwnerAi::Temp(OwnerAiTemp { value }) => {
            let temp = (value as f32).clamp(0.0, 2.0);
            let cfg = ctx
                .ai()
                .update_runtime_config(|c| c.temperature = temp)
                .await?;
            reply_embed(
                &ctx,
                &command,
                "AI Temperature Updated",
                format!("Sampling temperature set to `{:.2}`.", cfg.temperature),
                None,
            )
            .await
        }
        OwnerAi::Ctx(OwnerAiCtx { tokens }) => {
            let ctx_size = u32::try_from(tokens).unwrap_or(12_288).clamp(2_048, 131_072);
            let cfg = ctx
                .ai()
                .update_runtime_config(|c| c.num_ctx = ctx_size)
                .await?;
            reply_embed(
                &ctx,
                &command,
                "AI Context Window Updated",
                format!("`num_ctx` set to `{}` tokens.", cfg.num_ctx),
                None,
            )
            .await
        }
        OwnerAi::ReloadPrompt(_) => {
            ctx.ai().reload_prompt().await?;
            let chars = ctx.ai().get_prompt().await.chars().count();
            reply_embed(
                &ctx,
                &command,
                "System Prompt Reloaded",
                format!("Reloaded `prompt.txt` into memory (`{chars}` chars)."),
                None,
            )
            .await
        }
        OwnerAi::ChannelMode(OwnerAiChannelMode { mode, channel }) => {
            let target_channel = channel.unwrap_or(command.channel_id);
            let next_mode: AiPersonaMode = mode.into();
            ctx.ai()
                .update_runtime_config(|c| {
                    let key = target_channel.get().to_string();
                    if next_mode == AiPersonaMode::Default {
                        c.channel_modes.remove(&key);
                    } else {
                        c.channel_modes.insert(key, next_mode);
                    }
                })
                .await?;
            reply_embed(
                &ctx,
                &command,
                "Channel AI Persona Updated",
                format!("AI mode for <#{target_channel}> set to **{next_mode}**."),
                None,
            )
            .await
        }
    }
}

pub async fn handle_owner_model_autocomplete(
    ctx: &Context,
    command: &InteractionCommand,
    focused_input: &str,
) -> Result<()> {
    let user_id = command.user_id()?;
    if !BotConfig::get().owners.contains(&user_id) {
        command.autocomplete(ctx, Vec::new()).await?;
        return Ok(());
    }

    let needle = focused_input.trim().to_ascii_lowercase();
    let models = ctx.ai().list_models().await.unwrap_or_default();
    let choices: Vec<CommandOptionChoice> = models
        .into_iter()
        .filter(|m| needle.is_empty() || m.to_ascii_lowercase().contains(&needle))
        .take(25)
        .map(|m| CommandOptionChoice {
            name: m.clone(),
            name_localizations: None,
            value: CommandOptionChoiceValue::String(m),
        })
        .collect();

    command.autocomplete(ctx, choices).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use twilight_interactions::command::CreateCommand as _;

    #[test]
    fn test_owner_command_schema_valid() {
        let cmd = super::super::Owner::create_command().into();
        let built: twilight_model::application::command::Command = cmd;
        assert_eq!(built.name, "owner");
        assert_eq!(built.options.len(), 3);
    }
}
