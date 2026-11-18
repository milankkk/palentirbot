use crate::core::ai::AiClient;
use eyre::{Result, WrapErr};
use flexmap::tokio::TokioMutexMap;
use rosu_v2::{error::OsuError, Osu};
use std::{
    sync::{Arc, Mutex, MutexGuard, RwLock},
    time::{Duration, Instant},
};
use tokio::{fs, io::AsyncWriteExt};
use twilight_gateway::{MessageSender, Shard};
use twilight_http::{client::InteractionClient, Client};
use twilight_model::{
    channel::message::AllowedMentions,
    id::{
        marker::{ApplicationMarker, MessageMarker, UserMarker},
        Id,
    },
};

use crate::{
    core::BotConfig, custom_client::CustomClient, database::Database, pagination::Pagination,
    util::hasher::IntBuildHasher,
};

use super::{cluster::build_shards, settings::RootSettings, stats::BotStats, Cache, ReplayQueue};

use self::skin_list::SkinList;

mod configs;
mod skin_list;

pub struct Context {
    pub cache: Cache,
    pub shard_senders: Vec<MessageSender>,
    pub http: Arc<Client>,
    pub paginations: Arc<TokioMutexMap<Id<MessageMarker>, Pagination, IntBuildHasher>>,
    pub stats: Arc<BotStats>,
    pub replay_queue: ReplayQueue,
    root_settings: RootSettings,
    skin_list: Arc<Mutex<SkinList>>,
    application_id: Id<ApplicationMarker>,
    clients: Clients,
    osu_refresh_lock: tokio::sync::Mutex<Option<Instant>>,
    gpu_lock: tokio::sync::Mutex<()>,
    ai: AiClient,
}

impl Context {
    pub fn interaction(&self) -> InteractionClient<'_> {
        self.http.interaction(self.application_id)
    }

    pub fn bot_user_id(&self) -> Id<UserMarker> {
        self.application_id.cast()
    }

    pub fn osu(&self) -> Arc<Osu> {
        Arc::clone(
            &self
                .clients
                .osu
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    pub fn is_osu_auth_error(err: &OsuError) -> bool {
        matches!(err, OsuError::NoToken)
            || matches!(err, OsuError::Response { status, .. } if status.as_u16() == 401)
    }

    pub async fn refresh_osu_client(&self) -> Result<Arc<Osu>> {
        let mut guard = self.osu_refresh_lock.lock().await;
        if let Some(last) = *guard {
            if last.elapsed() < Duration::from_secs(10) {
                return Ok(self.osu());
            }
        }

        let config = BotConfig::get();
        let fresh = Arc::new(
            Osu::new(
                config.tokens.osu_client_id,
                &config.tokens.osu_client_secret,
            )
            .await
            .wrap_err("failed to re-authenticate osu! API client")?,
        );
        {
            let mut slot = self
                .clients
                .osu
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = Arc::clone(&fresh);
        }
        *guard = Some(Instant::now());
        info!("Re-authenticated osu! API client");

        Ok(fresh)
    }

    pub fn psql(&self) -> &Database {
        &self.clients.psql
    }

    pub fn client(&self) -> &CustomClient {
        &self.clients.custom
    }

    pub fn skin_list(&self) -> MutexGuard<'_, SkinList> {
        self.skin_list
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn ai(&self) -> &AiClient {
        &self.ai
    }

    pub fn gpu_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.gpu_lock
    }

    pub async fn new() -> Result<(Self, Vec<Shard>)> {
        let config = BotConfig::get();

        create_missing_folders_and_files(config).await?;

        let discord_token = &config.tokens.discord;

        let bytes = fs::read(config.paths.server_settings())
            .await
            .context("failed to read server settings file")?;

        let root_settings =
            serde_json::from_slice(&bytes).context("failed to deserialize server settings file")?;

        let mentions = AllowedMentions {
            replied_user: true,
            ..Default::default()
        };

        let http = Client::builder()
            .token(discord_token.to_owned())
            .remember_invalid_token(false)
            .default_allowed_mentions(mentions)
            .build();

        let http = Arc::new(http);

        let current_user = http.current_user().await?.model().await?;
        let application_id = current_user.id.cast();

        info!(
            "Connecting to Discord as {}#{}...",
            current_user.name, current_user.discriminator
        );

        let psql =
            Database::new(&config.database_url).wrap_err("failed to create database client")?;
        psql.run_migrations().await;

        let osu_client_id = config.tokens.osu_client_id;
        let osu_client_secret = &config.tokens.osu_client_secret;
        let osu = Osu::new(osu_client_id, osu_client_secret).await?;

        let custom = CustomClient::new();

        let cache = Cache::new();
        let stats = Arc::new(BotStats::new());

        let clients = Clients::new(osu, custom, psql);
        let (shards, shard_senders) = build_shards(discord_token, Arc::clone(&http)).await?;

        let paginations = TokioMutexMap::with_shard_amount_and_hasher(16, IntBuildHasher);

        let ctx = Self {
            cache,
            http,
            clients,
            osu_refresh_lock: tokio::sync::Mutex::new(None),
            gpu_lock: tokio::sync::Mutex::new(()),
            shard_senders,
            application_id,
            root_settings,
            paginations: Arc::new(paginations),
            stats,
            replay_queue: ReplayQueue::load().await,
            skin_list: Arc::new(Mutex::default()),
            ai: AiClient::new().await,
        };

        Ok((ctx, shards))
    }
}

struct Clients {
    custom: CustomClient,
    osu: RwLock<Arc<Osu>>,
    psql: Database,
}

impl Clients {
    fn new(osu: Osu, custom: CustomClient, psql: Database) -> Self {
        Self {
            osu: RwLock::new(Arc::new(osu)),
            custom,
            psql,
        }
    }
}

async fn create_missing_folders_and_files(config: &BotConfig) -> Result<()> {
    fs::create_dir_all(config.paths.downloads())
        .await
        .context("failed to create Downloads folder")?;

    fs::create_dir_all(config.paths.replays())
        .await
        .context("failed to create Replays folder")?;

    fs::create_dir_all(config.paths.skins())
        .await
        .context("failed to create Skins folder")?;

    fs::create_dir_all(config.paths.songs())
        .await
        .context("failed to create Songs folder")?;

    let danser_entry = config
        .paths
        .danser()
        .read_dir()
        .ok()
        .and_then(|mut dir| dir.next());

    ensure!(
        danser_entry.is_some(),
        "danser not found! please download from https://github.com/Wieku/danser-go/releases/"
    );

    let danser_bin = config.paths.danser().join("danser");
    if tokio::fs::metadata(&danser_bin)
        .await
        .is_ok_and(|m| m.is_dir())
    {
        warn!(
            path = ?danser_bin,
            "`DANSER_PATH/danser` is a directory instead of the danser executable binary; replay rendering will fail until danser files are placed directly inside DANSER_PATH"
        );
    }

    let server_settings = config.paths.server_settings();

    if !tokio::fs::try_exists(&server_settings).await.unwrap_or(false) {
        let tmp_settings = server_settings.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp_settings)
            .await
            .context("failed to create temp server settings file")?;

        file.write_all(b"{\"Servers\":[]}")
            .await
            .context("failed writing to temp server settings file")?;
        file.sync_all()
            .await
            .context("failed syncing temp server settings file")?;
        drop(file);
        fs::rename(&tmp_settings, &server_settings)
            .await
            .context("failed moving initial server settings file into place")?;
    }

    Ok(())
}
