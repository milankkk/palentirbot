use std::sync::Arc;

use command_macros::SlashCommand;
use eyre::Result;
use twilight_interactions::command::CreateCommand;

use crate::{
    util::{
        builder::{EmbedBuilder, MessageBuilder},
        interaction::InteractionCommand,
        InteractionCommandExt,
    },
    Context,
};

#[derive(CreateCommand, SlashCommand)]
#[command(name = "invite")]
#[flags(SKIP_DEFER)]
/// Invite me to your server
pub struct Invite;

pub async fn slash_invite(ctx: Arc<Context>, command: InteractionCommand) -> Result<()> {
    let invite_url = format!(
        "https://discord.com/oauth2/authorize?client_id={}&permissions=274878286848&integration_type=0&scope=bot+applications.commands",
        ctx.bot_user_id()
    );

    let embed = EmbedBuilder::new()
        .description(invite_url)
        .title("Invite me to your server!");

    let builder = MessageBuilder::new().embed(embed);
    command.callback(&ctx, builder, false).await?;

    Ok(())
}
