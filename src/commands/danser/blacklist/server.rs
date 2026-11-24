use eyre::Result;

use crate::{
    commands::danser::{blacklist::user::parse_discord_id, BlacklistServerAdd, BlacklistServerRemove},
    core::Context,
    util::{builder::MessageBuilder, interaction::InteractionCommand, InteractionCommandExt},
};

pub async fn add(
    ctx: &Context,
    command: &InteractionCommand,
    args: BlacklistServerAdd,
) -> Result<()> {
    let Some(guild_id) = parse_discord_id(&args.guild_id) else {
        command.error(ctx, "Guild ID is invalid!").await?;
        return Ok(());
    };

    ctx.psql().blacklist_server(guild_id, args.reason).await?;
    let builder =
        MessageBuilder::new().embed(format!("Server `{guild_id}` successfully blacklisted"));
    command.update(ctx, &builder).await?;
    Ok(())
}

pub async fn remove(
    ctx: &Context,
    command: &InteractionCommand,
    args: BlacklistServerRemove,
) -> Result<()> {
    let Some(guild_id) = parse_discord_id(&args.guild_id) else {
        command.error(ctx, "Guild ID is invalid!").await?;
        return Ok(());
    };

    if ctx.psql().whitelist_server(guild_id).await? {
        let builder =
            MessageBuilder::new().embed(format!("Server `{guild_id}` successfully whitelisted"));
        command.update(ctx, &builder).await?;
    } else {
        command
            .error(ctx, format!("Server `{guild_id}` not found"))
            .await?;
    }

    Ok(())
}
