use std::sync::Arc;

use command_macros::SlashCommand;
use eyre::Result;
use twilight_interactions::command::{CommandModel, CommandOption, CreateCommand, CreateOption};
use twilight_model::id::{marker::ChannelMarker, Id};

use crate::{
    commands::server_administrator,
    util::{interaction::InteractionCommand, InteractionCommandExt},
    Context,
};

use self::{input::*, output::*, view::*};

mod input;
mod output;
mod view;
mod setprefix;

#[derive(CommandModel, CreateCommand, SlashCommand)]
#[command(name = "setup", dm_permission = false)]
#[flags(SKIP_DEFER)]
/// Channel setup for the bot
pub enum Setup {
    #[command(name = "view")]
    View(SetupView),
    #[command(name = "input")]
    Input(SetupInput),
    #[command(name = "output")]
    Output(SetupOutput),
    #[command(name = "setprefix")] 
    SetPrefix(SetupSetPrefix), 
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "view")]
/// Shows the current configuration of the channels
pub struct SetupView;

#[derive(CommandModel, CreateCommand)]
#[command(name = "input")]
/// Configure the the channels in which replays can be rendered
pub struct SetupInput {
    /// Add or remove a channel
    action: InputAction,
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
#[command(name = "output")]
/// Configure the the channel in which the replay will be sent
pub struct SetupOutput {
    /// The channel you want as the output channel
    channel: Id<ChannelMarker>,
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "setprefix")]
/// Set a custom prefix for text commands in this server
pub struct SetupSetPrefix {
    /// The new prefix (e.g. `!`, `?`, `>>`) — max 16 characters
    pub prefix: String,
}


async fn slash_setup(ctx: Arc<Context>, mut command: InteractionCommand) -> Result<()> {
    match Setup::from_interaction(command.input_data())? {
        Setup::Input(args) => input(ctx, command, args).await,
        Setup::Output(args) => output(ctx, command, args).await,
        Setup::View(_) => view(ctx, command).await,
        Setup::SetPrefix(args) => setprefix::set_prefix(ctx, command, args).await,
    }
}
