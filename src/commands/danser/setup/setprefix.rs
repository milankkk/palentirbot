use std::sync::Arc;

use eyre::Result;
use twilight_model::guild::Permissions;

use crate::{
    core::Context,
    util::{
        builder::MessageBuilder,
        interaction::InteractionCommand,
    },
};
use crate::util::InteractionCommandExt;
use super::SetupSetPrefix;

pub async fn set_prefix(
    ctx: Arc<Context>,
    command: InteractionCommand,
    args: SetupSetPrefix,
) -> Result<()> {
    // Require Administrator.
    let permissions = command
        .member
        .as_ref()
        .and_then(|m| m.permissions)
        .unwrap_or_else(Permissions::empty);

    if !permissions.contains(Permissions::ADMINISTRATOR) {
        command
            .error_callback(
                &ctx,
                "You need **Administrator** permission to change the prefix.",
                true,
            )
            .await?;
        return Ok(());
    }

    let guild_id = command.guild_id.unwrap();
    let new_prefix = args.prefix.trim().to_owned();

    if new_prefix.is_empty() {
        command.error_callback(&ctx, "Prefix cannot be empty.", true).await?;
        return Ok(());
    }
    if new_prefix.len() > 16 {
        command.error_callback(&ctx, "Prefix must be 16 characters or fewer.", true).await?;
        return Ok(());
    }

    ctx.upsert_guild_settings(guild_id, |s| {
        s.prefix = Some(new_prefix.clone());
    })?;

    let content = format!(
        "✅ Prefix updated to `{new_prefix}`.\n\
         Use `{new_prefix}ping`, `{new_prefix}render`, `{new_prefix}queue`, etc."
    );
    command.callback(&ctx, MessageBuilder::new().embed(content), false).await?;
    Ok(())
}
