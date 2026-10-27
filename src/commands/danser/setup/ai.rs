use std::sync::Arc;

use eyre::Result;
use twilight_model::guild::Permissions;

use super::SetupAi;
use crate::{
    core::{BotConfig, Context},
    util::{builder::MessageBuilder, interaction::InteractionCommand, Authored, InteractionCommandExt},
};

pub async fn ai(ctx: Arc<Context>, command: InteractionCommand, args: SetupAi) -> Result<()> {
    let user_id = command.user_id()?;
    let is_owner = BotConfig::get().owners.contains(&user_id);

    let permissions = command
        .member
        .as_ref()
        .and_then(|m| m.permissions)
        .unwrap_or_else(Permissions::empty);
    let is_admin = permissions.contains(Permissions::ADMINISTRATOR);

    // If changing safesearch, user must be a bot owner
    if args.safesearch.is_some() && !is_owner {
        command
            .error_callback(&ctx, "Only bot owners can toggle SafeSearch.", true)
            .await?;
        return Ok(());
    }

    // If changing enabled, user must be an administrator or bot owner
    if args.enabled.is_some() && !is_admin && !is_owner {
        command
            .error_callback(
                &ctx,
                "You need **Administrator** permission (or bot owner) to toggle AI.",
                true,
            )
            .await?;
        return Ok(());
    }

    let guild_id = match command.guild_id {
        Some(gid) => gid,
        None => {
            command
                .error_callback(&ctx, "This command can only be used in a server.", true)
                .await?;
            return Ok(());
        }
    };

    if args.enabled.is_none() && args.safesearch.is_none() {
        let (ai_enabled, safesearch) = ctx
            .guild_settings(guild_id, |s| (s.ai_enabled, s.ai_safesearch))
            .unwrap_or((true, true));

        let content = format!(
            "**AI Settings for this server:**\n• AI Module: **{}**\n• SafeSearch: **{}**",
            if ai_enabled { "Enabled" } else { "Disabled" },
            if safesearch { "Enabled (Moderate)" } else { "Disabled (Off)" }
        );
        let builder = MessageBuilder::new().embed(content);
        command.callback(&ctx, builder, false).await?;
        return Ok(());
    }

    let upsert_res = ctx.upsert_guild_settings(guild_id, |s| {
        if let Some(enabled) = args.enabled {
            s.ai_enabled = enabled;
        }
        if let Some(safesearch) = args.safesearch {
            s.ai_safesearch = safesearch;
        }
    });

    if let Err(err) = upsert_res {
        let content = "Failed to update server settings";
        let _ = command.error_callback(&ctx, content, false).await;
        return Err(err);
    }

    let (ai_enabled, safesearch) = ctx
        .guild_settings(guild_id, |s| (s.ai_enabled, s.ai_safesearch))
        .unwrap_or((true, true));

    let content = format!(
        "✅ **Updated AI settings for this server:**\n• AI Module: **{}**\n• SafeSearch: **{}**",
        if ai_enabled { "Enabled" } else { "Disabled" },
        if safesearch { "Enabled (Moderate)" } else { "Disabled (Off)" }
    );
    let builder = MessageBuilder::new().embed(content);
    command.callback(&ctx, builder, false).await?;

    Ok(())
}
