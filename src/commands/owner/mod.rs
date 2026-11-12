use std::sync::Arc;

use command_macros::SlashCommand;
use eyre::Result;
use twilight_interactions::command::{AutocompleteValue, CommandModel, CreateCommand};

use crate::{
    util::{interaction::InteractionCommand, InteractionCommandExt},
    Context,
};

use self::ai::*;
use self::cache::*;
use self::settings::*;

mod ai;
mod cache;
pub mod settings;

#[derive(CommandModel, CreateCommand, SlashCommand)]
#[command(name = "owner")]
#[flags(ONLY_OWNER, SKIP_DEFER)]
/// You won't be able to use this :^)
pub enum Owner {
    #[command(name = "cache")]
    Cache(OwnerCache),
    #[command(name = "settings")]
    Settings(OwnerSettings),
    #[command(name = "ai")]
    Ai(OwnerAi),
}

#[derive(CommandModel)]
pub enum OwnerParsable {
    #[command(name = "cache")]
    Cache(OwnerCache),
    #[command(name = "settings")]
    Settings(OwnerSettings),
    #[command(name = "ai")]
    Ai(OwnerAiParsable),
}

#[derive(CommandModel, CreateCommand)]
#[command(name = "cache")]
/// Display stats about the internal cache
pub struct OwnerCache;

#[derive(CommandModel, CreateCommand)]
#[command(name = "settings")]
/// Edit default danser settings
pub struct OwnerSettings;

// * EXAMPLE:
// #[derive(CommandModel, CreateCommand)]
// #[command(name = "interval")]
// /// Adjust the tracking interval
// pub struct OwnerTrackingInterval {
//     /// Specify the interval in seconds, defaults to 9000
//     number: Option<i64>,
// }

async fn slash_owner(ctx: Arc<Context>, mut command: InteractionCommand) -> Result<()> {
    match Owner::from_interaction(command.input_data())? {
        Owner::Cache(_) => cache(ctx, command).await,
        Owner::Settings(_) => settings(ctx, command).await,
        Owner::Ai(action) => handle_owner_ai(ctx, command, action).await,
    }
}

pub async fn slash_owner_autocomplete(
    ctx: Arc<Context>,
    mut command: InteractionCommand,
) -> Result<()> {
    if let OwnerParsable::Ai(OwnerAiParsable::Model(args)) =
        OwnerParsable::from_interaction(command.input_data())?
    {
        if let AutocompleteValue::Focused(partial) = args.name {
            handle_owner_model_autocomplete(&ctx, &command, &partial).await?;
        }
    }
    Ok(())
}
