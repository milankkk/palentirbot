use std::{sync::Arc, time::Instant};

use command_macros::SlashCommand;
use eyre::Result;
use twilight_interactions::command::CreateCommand;

use crate::{
    core::Context,
    util::{builder::MessageBuilder, interaction::InteractionCommand, InteractionCommandExt},
};

#[derive(CreateCommand, SlashCommand)]
#[command(
    name = "ping",
    help = "Most basic command, generally used to check if the bot is online.\n\
    The displayed latency is the time it takes for the bot \
    to receive a response from discord after sending a message."
)]
#[flags(SKIP_DEFER)]
/// Check if the bot is online
pub struct Ping;

async fn slash_ping(ctx: Arc<Context>, command: InteractionCommand) -> Result<()> {
    let start = Instant::now();
    command.defer(&ctx, false).await?;
    let elapsed = start.elapsed().as_millis();

    let content = format!(":ping_pong: Pong! ({elapsed}ms)");
    let builder = MessageBuilder::new().content(content);
    command.update(&ctx, &builder).await?;

    Ok(())
}
