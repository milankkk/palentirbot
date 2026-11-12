use std::{
    fs::OpenOptions,
    io::Write,
    sync::Mutex,
};

use eyre::{Context as _, Result};
use twilight_model::id::{marker::GuildMarker, Id};

use crate::{
    core::{settings::Server, BotConfig},
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

        let output = {
            let guard = self.root_settings.servers.guard();

            let mut server = self
                .root_settings
                .servers
                .get(&guild_id, &guard)
                .cloned()
                .unwrap_or_default();

            let output = f(&mut server);

            self.root_settings.servers.insert(guild_id, server, &guard);

            output
        };

        self.store_guild_settings()
            .context("failed to upsert server settings")?;

        Ok(output)
    }

    fn store_guild_settings(&self) -> Result<()> {
        let path = BotConfig::get().paths.server_settings();
        let tmp_path = path.with_extension("json.tmp");

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)
            .context("failed to open temp server settings file")?;

        serde_json::to_writer(&mut file, &self.root_settings)
            .context("failed to serialize root settings")?;
        file.flush()
            .context("failed to flush temp server settings file")?;
        file.sync_all()
            .context("failed to sync temp server settings file")?;
        drop(file);

        std::fs::rename(&tmp_path, &path).context("failed to atomically replace server settings file")?;

        Ok(())
    }
}
