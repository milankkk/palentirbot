#![deny(clippy::all, nonstandard_style, rust_2018_idioms)]
#![warn(unused)]
#[macro_use]
extern crate eyre;

#[macro_use]
extern crate tracing;

mod commands;
mod core;
mod custom_client;
mod database;
mod pagination;
mod util;

use std::sync::Arc;

use eyre::{Context as _, Result};
use tokio::{runtime::Builder as RuntimeBuilder, signal};

use crate::core::{
    commands::slash::{Command, Commands},
    event_loop, logging, BotConfig, Context, ReplayQueue,
};

fn main() {
    let runtime = RuntimeBuilder::new_multi_thread()
        .enable_all()
        .thread_stack_size(4 * 1024 * 1024)
        .build()
        .expect("Could not build runtime");

    if let Err(err) = runtime.block_on(async_main()) {
        error!("critical error in main: {err:?}");
    }
}

async fn async_main() -> Result<()> {
    let _ = dotenv::dotenv().expect("failed to parse .env file");
    let _log_worker_guard = logging::init();

    BotConfig::init().context("failed to initialize config")?;

    let (ctx, shards) = Context::new().await.context("failed to create ctx")?;

    let ctx = Arc::new(ctx);

    let slash_commands = Commands::get().collect(Command::create);
    info!("Setting {} slash commands...", slash_commands.len());

    if cfg!(debug_assertions) {
        ctx.interaction()
            .set_global_commands(&[])
            .await
            .context("failed to set empty global commands")?;

        ctx.interaction()
            .set_guild_commands(BotConfig::get().dev_guild, &slash_commands)
            .await
            .context("failed to set guild commands")?;
    } else {
        ctx.interaction()
            .set_global_commands(&slash_commands)
            .await
            .context("failed to set global commands")?;
    }

    ReplayQueue::process(Arc::clone(&ctx));

    tokio::select! {
        _ = event_loop(Arc::clone(&ctx), shards) => error!("Event loop ended"),
        res = signal::ctrl_c() => if let Err(err) = res.context("error while awaiting ctrl+c") {
            error!("{err:?}");
        } else {
            info!("Received Ctrl+C");
        },
    }

    info!("Shutting down");

    Ok(())
}
