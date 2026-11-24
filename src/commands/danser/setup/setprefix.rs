use std::sync::Arc;

use eyre::Result;

use super::{require_guild_admin, SetupSetPrefix};
use crate::util::InteractionCommandExt;
use crate::{
    core::Context,
    util::{builder::MessageBuilder, interaction::InteractionCommand},
};

pub async fn set_prefix(
    ctx: Arc<Context>,
    command: InteractionCommand,
    args: SetupSetPrefix,
) -> Result<()> {
    let Some(guild_id) = require_guild_admin(
        &ctx,
        &command,
        "You need **Administrator** permission to change the prefix.",
    )
    .await?
    else {
        return Ok(());
    };
    let new_prefix = args.prefix.trim().to_owned();

    if new_prefix.is_empty() {
        command
            .error_callback(&ctx, "Prefix cannot be empty.", true)
            .await?;
        return Ok(());
    }
    if new_prefix.chars().count() > 16 {
        command
            .error_callback(&ctx, "Prefix must be 16 characters or fewer.", true)
            .await?;
        return Ok(());
    }
    if new_prefix.chars().any(|c| c.is_whitespace() || c == '`') {
        command
            .error_callback(
                &ctx,
                "Prefix cannot contain whitespace, newlines, or backticks.",
                true,
            )
            .await?;
        return Ok(());
    }

    ctx.upsert_guild_settings(guild_id, |s| {
        s.prefix = Some(new_prefix.clone());
    })?;

    let content = format!(
        "✅ Prefix updated to `{new_prefix}`.\n\
         Use `{new_prefix}ping`, `{new_prefix}render`, `{new_prefix}queue`, etc."
    );
    command
        .callback(&ctx, MessageBuilder::new().embed(content), false)
        .await?;
    Ok(())
}
