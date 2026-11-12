#![deny(nonstandard_style, rust_2018_idioms)]
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
use twilight_gateway::CloseFrame;

use crate::core::{
    commands::slash::{Command, Commands},
    event_loop, logging, BotConfig, Context, ReplayQueue,
};

fn main() {
    let dotenv_res = dotenvy::dotenv();
    let _log_worker_guard = logging::init();

    if let Err(err) = dotenv_res {
        if !matches!(err, dotenvy::Error::Io(ref io_err) if io_err.kind() == std::io::ErrorKind::NotFound) {
            warn!("Failed to parse .env file: {err:?}");
        }
    }

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

    let ctx_bandwidth = Arc::clone(&ctx);
    tokio::spawn(async move {
        ctx_bandwidth.client().measure_bandwidth().await;
    });

    tokio::select! {
        _ = event_loop(Arc::clone(&ctx), shards) => error!("Event loop ended"),
        res = wait_for_shutdown_signal() => match res {
            Ok(sig_name) => info!("Received {sig_name}"),
            Err(err) => error!("error while awaiting shutdown signal: {err:?}"),
        },
    }

    info!("Shutting down shards");
    for sender in &ctx.shard_senders {
        let _ = sender.close(CloseFrame::NORMAL);
    }

    Ok(())
}

async fn wait_for_shutdown_signal() -> Result<&'static str> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())
            .context("failed to register SIGTERM handler")?;

        tokio::select! {
            res = signal::ctrl_c() => {
                res.context("error while awaiting ctrl+c")?;
                Ok("SIGINT")
            }
            _ = sigterm.recv() => Ok("SIGTERM"),
        }
    }

    #[cfg(not(unix))]
    {
        signal::ctrl_c()
            .await
            .context("error while awaiting ctrl+c")?;
        Ok("SIGINT")
    }
}
