use std::{
    fs::OpenOptions,
    io::{BufWriter, Write},
    sync::Mutex,
};

use eyre::{Context as _, Result};
use flurry::HashMap as FlurryMap;
use twilight_model::id::{marker::GuildMarker, Id};

use crate::{
    core::{
        settings::{RootSettings, Server},
        BotConfig,
    },
    util::hasher::IntBuildHasher,
    Context,
};

static SETTINGS_FILE_LOCK: Mutex<()> = Mutex::new(());

impl Context {
    pub fn guild_settings<F, O>(&self, guild_id: Id<GuildMarker>, f: F) -> Option<O>
    where
        F: FnOnce(&Server) -> O,
    {
        self.root_settings.servers.pin().get(&guild_id).map(f)
    }

    pub fn upsert_guild_settings<F, O>(&self, guild_id: Id<GuildMarker>, f: F) -> Result<O>
    where
        F: FnOnce(&mut Server) -> O,
    {
        let _save_guard = SETTINGS_FILE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let snapshot_servers = FlurryMap::with_hasher(IntBuildHasher);
        let (server, output) = {
            let guard = self.root_settings.servers.guard();
            for (&k, v) in self.root_settings.servers.iter(&guard) {
                snapshot_servers.insert(k, v.clone(), &guard);
            }

            let mut server = self
                .root_settings
                .servers
                .get(&guild_id, &guard)
                .cloned()
                .unwrap_or_default();

            let output = f(&mut server);
            snapshot_servers.insert(guild_id, server.clone(), &guard);

            (server, output)
        };

        let snapshot = RootSettings {
            servers: snapshot_servers,
        };
        let write_fn = || Self::write_root_settings_file(&snapshot);
        if matches!(
            tokio::runtime::Handle::try_current(),
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
        ) {
            tokio::task::block_in_place(write_fn)?;
        } else {
            write_fn()?;
        }

        self.root_settings.servers.pin().insert(guild_id, server);

        Ok(output)
    }

    fn write_root_settings_file(root: &RootSettings) -> Result<()> {
        let path = BotConfig::get().paths.server_settings();
        let tmp_path = path.with_extension("json.tmp");

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)
            .context("failed to open temp server settings file")?;
        let mut writer = BufWriter::new(file);

        serde_json::to_writer(&mut writer, root).context("failed to serialize root settings")?;
        writer
            .flush()
            .context("failed to flush temp server settings file")?;
        let file = writer
            .into_inner()
            .map_err(|e| eyre::eyre!("failed to finish buffered write: {e}"))?;
        file.sync_all()
            .context("failed to sync temp server settings file")?;
        drop(file);

        std::fs::rename(&tmp_path, &path).context("failed to atomically replace server settings file")?;

        Ok(())
    }
}
