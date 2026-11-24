use std::sync::Arc;

use command_macros::SlashCommand;
use eyre::Result;
use twilight_interactions::command::{CommandModel, CommandOption, CreateCommand, CreateOption};
use twilight_model::{
    guild::Permissions,
    id::{
        marker::{ChannelMarker, GuildMarker},
        Id,
    },
};

use crate::{
    util::{interaction::InteractionCommand, InteractionCommandExt},
    Context,
};

use self::{aichannel::*, input::*, view::*};

mod aichannel;
mod input;
mod setprefix;
mod view;

pub(super) async fn require_guild_admin(
    ctx: &Context,
    command: &InteractionCommand,
    deny_msg: &str,
) -> Result<Option<Id<GuildMarker>>> {
    let (Some(member), Some(guild_id)) = (command.member.as_ref(), command.guild_id) else {
        command
            .error_callback(ctx, "This command can only be used in a server.", true)
            .await?;
        return Ok(None);
    };

    let permissions = member.permissions.unwrap_or_else(Permissions::empty);
    if !permissions.contains(Permissions::ADMINISTRATOR) {
        command.error_callback(ctx, deny_msg, true).await?;
        return Ok(None);
    }

    Ok(Some(guild_id))
}

#[derive(CommandModel, CreateCommand, SlashCommand)]
#[command(name = "setup", dm_permission = false)]
#[flags(SKIP_DEFER)]
/// Channel setup for the bot
pub enum Setup {
    #[command(name = "view")]
    View(SetupView),
    #[command(name = "input")]
    Input(SetupInput),
    #[command(name = "setprefix")]
    SetPrefix(SetupSetPrefix),
    #[command(name = "aichannel")]
    AiChannel(SetupAiChannel),
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "view")]
/// Shows the current configuration of the channels
pub struct SetupView;

#[derive(CommandModel, CreateCommand)]
#[command(name = "input")]
/// Configure the channels in which replays can be rendered
pub struct SetupInput {
    /// Add or remove a channel
    action: InputAction,
    #[command(channel_types = "guild_text guild_announcement")]
    /// The channel you want to add/remove
    channel: Id<ChannelMarker>,
}

#[derive(CommandOption, CreateOption)]
pub enum InputAction {
    #[option(name = "add", value = "add")]
    Add,
    #[option(name = "remove", value = "remove")]
    Remove,
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "setprefix")]
/// Set a custom prefix for text commands in this server
pub struct SetupSetPrefix {
    /// The new prefix (e.g. `!`, `?`, `>>`) — max 16 characters
    pub prefix: String,
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "aichannel")]
/// Set a dedicated AI chat channel where no prefix is required (omit to clear)
pub struct SetupAiChannel {
    #[command(channel_types = "guild_text guild_announcement")]
    /// Channel for prefixless AI chat (leave empty to clear)
    pub channel: Option<Id<ChannelMarker>>,
}

async fn slash_setup(ctx: Arc<Context>, mut command: InteractionCommand) -> Result<()> {
    match Setup::from_interaction(command.input_data())? {
        Setup::Input(args) => input(ctx, command, args).await,
        Setup::View(_) => view(ctx, command).await,
        Setup::SetPrefix(args) => setprefix::set_prefix(ctx, command, args).await,
        Setup::AiChannel(args) => aichannel(ctx, command, args).await,
    }
}
