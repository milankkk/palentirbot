use std::sync::Arc;

use eyre::Result;
use twilight_model::guild::Permissions;

use crate::{
    core::Context,
    util::{builder::MessageBuilder, interaction::InteractionCommand, InteractionCommandExt},
};

use super::SetupAiChannel;

/// Configure a dedicated channel where messages trigger the AI without needing a prefix.
pub async fn aichannel(
    ctx: Arc<Context>,
    command: InteractionCommand,
    args: SetupAiChannel,
) -> Result<()> {
    let (Some(member), Some(guild_id)) = (command.member.as_ref(), command.guild_id) else {
        command
            .error_callback(&ctx, "This command can only be used in a server.", true)
            .await?;
        return Ok(());
    };
    let permissions = member.permissions.unwrap_or_else(Permissions::empty);

    if !permissions.contains(Permissions::ADMINISTRATOR) {
        command
            .error_callback(
                &ctx,
                "You need **Administrator** permission to configure the AI channel.",
                true,
            )
            .await?;
        return Ok(());
    }

    let SetupAiChannel { channel } = args;
    ctx.upsert_guild_settings(guild_id, |s| s.ai_channel = channel)
        .inspect_err(|err| tracing::warn!(?err, "failed to save ai_channel setting"))?;

    let content = match channel {
        Some(ch) => format!(
            "Successfully set <#{ch}> as the AI chat channel.\nMessages sent there (and `@mention`s anywhere) will talk to the AI without needing a prefix."
        ),
        None => "Cleared the dedicated AI chat channel. Users can still use `/ask`, prefix `ask`, replies, DMs, or `@mention`s.".to_owned(),
    };
    command
        .callback(&ctx, MessageBuilder::new().embed(content), false)
        .await?;

    Ok(())
}
