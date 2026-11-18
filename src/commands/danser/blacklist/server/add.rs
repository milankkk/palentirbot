use crate::{
    commands::danser::{blacklist::user::parse_discord_id, BlacklistServerAdd},
    core::Context,
    util::{builder::MessageBuilder, interaction::InteractionCommand, InteractionCommandExt},
};
use eyre::Result;

pub async fn add(
    ctx: &Context,
    command: &InteractionCommand,
    args: BlacklistServerAdd,
) -> Result<()> {
    let BlacklistServerAdd { guild_id, reason } = args;

    let Some(guild_id_parsed) = parse_discord_id(&guild_id) else {
        command.error(ctx, "Guild ID is invalid!").await?;
        return Ok(());
    };

    ctx.psql().blacklist_server(guild_id_parsed, reason).await?;
    let builder = MessageBuilder::new()
        .embed(format!("Server `{guild_id_parsed}` successfully blacklisted"));
    command.update(ctx, &builder).await?;
    Ok(())
}
