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

use super::{user_settings_path, SettingsCopy};

pub async fn copy(
    ctx: Arc<Context>,
    command: InteractionCommand,
    args: SettingsCopy,
) -> Result<()> {
    let author = command.user_id()?;
    let SettingsCopy { user } = args;

    if author == user {
        command.error_callback(&ctx, ":clown:", false).await?;

        return Ok(());
    }

    let from = user_settings_path(user);
    if !fs::try_exists(&from).await.unwrap_or(false) {
        command
            .error_callback(&ctx, "That user has not configured their settings yet", false)
            .await?;

        return Ok(());
    }

    let to = user_settings_path(author);
    if let Err(err) = copy_atomic(&from, &to).await {
        let _ = command
            .error_callback(&ctx, "Failed to copy the settings file", false)
            .await;

        return Err(err.wrap_err(format!("failed to copy settings from {user} for {author}")));
    }

    let builder = MessageBuilder::new().embed("Successfully copied settings!");
    command.callback(&ctx, builder, false).await?;

    Ok(())
}
