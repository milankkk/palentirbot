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
        drop(_log_worker_guard);
        std::process::exit(1);
    }
}

async fn async_main() -> Result<()> {
    BotConfig::init().context("failed to initialize config")?;

    let (ctx, shards) = tokio::time::timeout(std::time::Duration::from_secs(90), Context::new())
        .await
        .context("timed out initializing Discord/DB/osu! clients after 90s")?
        .context("failed to create ctx")?;

    let ctx = Arc::new(ctx);

    let slash_commands = Commands::get().collect(Command::create);
    info!("Setting {} slash commands...", slash_commands.len());

    let set_cmds_fut = async {
        if cfg!(debug_assertions) {
            ctx.interaction()
                .set_guild_commands(BotConfig::get().dev_guild, &slash_commands)
                .await
                .context("failed to set guild commands")
        } else {
            ctx.interaction()
                .set_global_commands(&slash_commands)
                .await
                .context("failed to set global commands")
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(45), set_cmds_fut)
        .await
        .context("timed out registering slash commands after 45s")??;

    ReplayQueue::process(Arc::clone(&ctx));
    crate::core::web_render::spawn(Arc::clone(&ctx));

    let shard_handles = event_loop(Arc::clone(&ctx), shards);
    let mut all_shards = futures::future::join_all(shard_handles);

    let unexpected_exit = tokio::select! {
        _ = &mut all_shards => {
            error!("Event loop ended unexpectedly");
            true
        }
        res = wait_for_shutdown_signal() => {
            match res {
                Ok(sig_name) => info!("Received {sig_name}"),
                Err(err) => error!("error while awaiting shutdown signal: {err:?}"),
            }
            false
        }
    };

    if unexpected_exit {
        // Allow any in-flight GPU render to finish (up to 3 minutes) before exiting for restart.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(180), ctx.gpu_lock().lock()).await;
    }

    info!("Shutting down shards");
    for sender in &ctx.shard_senders {
        let _ = sender.close(CloseFrame::NORMAL);
    }
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), all_shards).await;

    if unexpected_exit {
        eyre::bail!("Discord shard event loop terminated unexpectedly");
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
