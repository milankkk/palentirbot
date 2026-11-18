use crate::{
    commands::danser::{blacklist::user::parse_discord_id, BlacklistServerRemove},
    core::Context,
    util::{builder::MessageBuilder, interaction::InteractionCommand, InteractionCommandExt},
};
use eyre::Result;

pub async fn remove(
    ctx: &Context,
    command: &InteractionCommand,
    args: BlacklistServerRemove,
) -> Result<()> {
    let BlacklistServerRemove { guild_id } = args;

    let Some(guild_id_parsed) = parse_discord_id(&guild_id) else {
        command.error(ctx, "Guild ID is invalid!").await?;
        return Ok(());
    };

    if ctx.psql().whitelist_server(guild_id_parsed).await? {
        let builder = MessageBuilder::new()
            .embed(format!("Server `{guild_id_parsed}` successfully whitelisted"));
        command.update(ctx, &builder).await?;
    } else {
        command
            .error(ctx, format!("Server `{guild_id_parsed}` not found"))
            .await?;
    }

    Ok(())
}
