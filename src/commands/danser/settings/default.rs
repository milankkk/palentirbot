use std::sync::Arc;

use eyre::Result;
use tokio::fs;

use crate::{
    core::Context,
    util::{
        builder::MessageBuilder, fs::copy_atomic, interaction::InteractionCommand, Authored,
        InteractionCommandExt,
    },
};

use super::{default_settings_path, user_settings_path, SettingsConfirm, SettingsDefault};

pub async fn default(
    ctx: Arc<Context>,
    command: InteractionCommand,
    args: SettingsDefault,
) -> Result<()> {
    if matches!(args.confirm, SettingsConfirm::Cancel) {
        let content = "Restoring default settings was cancelled.\n\
            Be sure to confirm if you want to overwrite your current settings with default values.";

        let builder = MessageBuilder::new().embed(content);
        command.callback(&ctx, builder, true).await?;

        return Ok(());
    }

    let author = command.user_id()?;
    let from = default_settings_path();

    if !fs::try_exists(&from).await.unwrap_or(false) {
        let _ = command
            .error_callback(&ctx, "Could not find default settings", false)
            .await;

        bail!("No default settings found at {from:?}");
    }

    let to = user_settings_path(author);
    if let Err(err) = copy_atomic(&from, &to).await {
        let _ = command
            .error_callback(&ctx, "Failed to set default values", false)
            .await;

        return Err(err.wrap_err(format!("failed to copy default settings for {author}")));
    }

    let builder = MessageBuilder::new().embed("Successfully restored default settings!");
    command.callback(&ctx, builder, false).await?;

    Ok(())
}
