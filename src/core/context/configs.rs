use std::{borrow::Cow, sync::Mutex};

use eyre::{Context as _, Result};
use twilight_model::id::{marker::GuildMarker, Id};

use crate::{
    core::{
        settings::{serialize_root_settings_with_override, Server},
        BotConfig,
    },
    Context,
};

static SETTINGS_FILE_LOCK: Mutex<()> = Mutex::new(());

impl Context {
    /// Return the configured command prefix for `guild_id`, defaulting to `"!"`.
    pub fn guild_prefix(&self, guild_id: Option<Id<GuildMarker>>) -> Cow<'static, str> {
        guild_id
            .and_then(|gid| {
                self.guild_settings(gid, |s| s.prefix.as_ref().map(|p| Cow::Owned(p.clone())))
                    .flatten()
            })
            .unwrap_or(Cow::Borrowed("!"))
    }

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

        let (server, output) = {
            let mut server = self
                .root_settings
                .servers
                .pin()
                .get(&guild_id)
                .cloned()
                .unwrap_or_default();
            let output = f(&mut server);
            (server, output)
        };

        let bytes = serialize_root_settings_with_override(
            &self.root_settings.servers,
            guild_id,
            &server,
        )
        .context("failed to serialize root settings")?;

        let path = BotConfig::get().paths.server_settings();
        let write_fn = || crate::util::fs::write_bytes_atomic_sync(&path, &bytes);
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
}
