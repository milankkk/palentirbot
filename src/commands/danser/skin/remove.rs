use std::sync::Arc;

use eyre::{Context as _, Result};
use tokio::fs;

use crate::{
    core::{BotConfig, Context},
    util::{
        builder::MessageBuilder,
        constants::{GENERAL_ISSUE, NOT_OWNER},
        interaction::InteractionCommand,
        Authored, InteractionCommandExt,
    },
};

use super::SkinRemove;

pub async fn remove(
    ctx: Arc<Context>,
    command: InteractionCommand,
    args: SkinRemove,
) -> Result<()> {
    let config = BotConfig::get();

    let user = match command.user() {
        Ok(user) => user,
        Err(err) => {
            command.error_callback(&ctx, GENERAL_ISSUE, false).await?;
            return Err(err.wrap_err("failed to get user from command"));
        }
    };

    if !config.owners.contains(&user.id) {
        command.error_callback(&ctx, NOT_OWNER, true).await?;
        return Ok(());
    }

    command.defer(&ctx, false).await?;

    let SkinRemove { index } = args;

    let skin_path = BotConfig::get().paths.skins();
    let mut entries = fs::read_dir(&skin_path)
        .await
        .context("failed to read skins folder")?;

    let mut skin_list = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .context("failed to read entry in skins folder")?
    {
        skin_list.push(entry);
    }

    skin_list.sort_unstable_by_key(|entry| entry.file_name().to_ascii_lowercase());

    let skin_to_remove_opt = index.checked_sub(1).and_then(|idx| skin_list.get(idx));
    if let Some(skin_to_remove) = skin_to_remove_opt {
        fs::remove_dir_all(skin_to_remove.path()).await?;

        // Reset the skin list cache
        ctx.skin_list().clear();

        let skin = skin_to_remove.file_name();
        let content = format!("Successfully deleted skin `{}`", skin.to_string_lossy());
        let builder = MessageBuilder::new().embed(content);

        command.update(&ctx, &builder).await?;
    } else {
        let len = skin_list.len();
        let content = format!("Invalid skin index, must be between 1 and {len}");
        command.error(&ctx, content).await?;
    }

    Ok(())
}
