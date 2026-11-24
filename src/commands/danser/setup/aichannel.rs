use std::sync::Arc;

use eyre::Result;

use crate::{
    core::Context,
    util::{builder::MessageBuilder, interaction::InteractionCommand, InteractionCommandExt},
};

use super::{require_guild_admin, SetupAiChannel};

/// Configure a dedicated channel where messages trigger the AI without needing a prefix.
pub async fn aichannel(
    ctx: Arc<Context>,
    command: InteractionCommand,
    args: SetupAiChannel,
) -> Result<()> {
    let Some(guild_id) = require_guild_admin(
        &ctx,
        &command,
        "You need **Administrator** permission to configure the AI channel.",
    )
    .await?
    else {
        return Ok(());
    };

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
