use std::sync::Arc;

use eyre::{Context as _, Result};
use tokio::fs;

use crate::{
    core::{BotConfig, Context},
    util::{
        builder::MessageBuilder,
        constants::GENERAL_ISSUE,
        interaction::InteractionCommand,
        InteractionCommandExt,
    },
};

use super::SkinRemove;

pub async fn remove(
    ctx: Arc<Context>,
    command: InteractionCommand,
    args: SkinRemove,
) -> Result<()> {
    if !super::ensure_owner(&ctx, &command).await? {
        return Ok(());
    }

    command.defer(&ctx, false).await?;

    let skin_path = BotConfig::get().paths.skins();
    let (skin_to_remove, total_len) = {
        let mut skin_list = ctx.skin_list();
        let skins = skin_list.get()?;
        (
            args.index
                .checked_sub(1)
                .and_then(|idx| skins.get(idx))
                .cloned(),
            skins.len(),
        )
    };

    let Some(skin) = skin_to_remove else {
        command
            .error(
                &ctx,
                format!("Invalid skin index, must be between 1 and {total_len}"),
            )
            .await?;
        return Ok(());
    };

    let target = skin_path.join(&skin);
    if target.parent() != Some(skin_path.as_path()) {
        command.error(&ctx, "Invalid skin directory path.").await?;
        return Ok(());
    }
    if let Err(err) = fs::remove_dir_all(&target).await {
        let _ = command.error(&ctx, GENERAL_ISSUE).await;
        return Err(err).context("failed to delete skin directory");
    }

    ctx.skin_list().clear();

    let content = format!("Successfully deleted skin `{}`", skin.to_string_lossy());
    command
        .update(&ctx, &MessageBuilder::new().embed(content))
        .await?;

    Ok(())
}
