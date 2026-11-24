use std::sync::Arc;

use eyre::Result;

use crate::{
    core::Context,
    util::{builder::MessageBuilder, interaction::InteractionCommand, InteractionCommandExt},
};

use super::{require_guild_admin, InputAction, SetupInput};

pub async fn input(ctx: Arc<Context>, command: InteractionCommand, args: SetupInput) -> Result<()> {
    let Some(guild_id) = require_guild_admin(
        &ctx,
        &command,
        "You do not have the required permissions to perform this action!",
    )
    .await?
    else {
        return Ok(());
    };

    let SetupInput { action, channel } = args;

    match action {
        InputAction::Add => {
            if let Err(err) =
                ctx.upsert_guild_settings(guild_id, |s| s.input_channels.insert(channel))
            {
                let _ = command
                    .error_callback(&ctx, "Failed to update server settings", false)
                    .await;
                return Err(err);
            }

            let content = format!("Successfully added channel <#{channel}>");
            command
                .callback(&ctx, MessageBuilder::new().embed(content), false)
                .await?;
        }
        InputAction::Remove => {
            match ctx.upsert_guild_settings(guild_id, |s| s.input_channels.remove(&channel)) {
                Ok(true) => {
                    let content = format!("Successfully removed channel <#{channel}>");
                    command
                        .callback(&ctx, MessageBuilder::new().embed(content), false)
                        .await?;
                }
                Ok(false) => {
                    command
                        .error_callback(&ctx, "That channel is not whitelisted", false)
                        .await?;
                }
                Err(err) => {
                    let _ = command
                        .error_callback(&ctx, "Failed to update server settings", false)
                        .await;
                    return Err(err);
                }
            }
        }
    }

    Ok(())
}
