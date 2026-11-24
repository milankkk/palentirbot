use std::time::Duration;

use eyre::Result;
use sqlx::{postgres::PgPoolOptions, PgPool};

pub use self::impls::{BlacklistHit, DISCORD_LINK_TTL_SECS, WEB_SESSION_TTL_SECS};

mod impls;

pub struct Database {
    pool: PgPool,
}

impl Database {
    #[cold]
    pub fn new(uri: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .acquire_timeout(Duration::from_secs(5))
            .idle_timeout(Duration::from_secs(300))
            .max_lifetime(Duration::from_secs(1800))
            .connect_lazy(uri)?;

        Ok(Self { pool })
    }

    /// Ensure required PostgreSQL tables exist idempotently without blocking startup if DB is momentarily unreachable.
    pub async fn run_migrations(&self) {
        let base_sql = include_str!("../../migrations/20221130212623_base_schemas.up.sql")
            .replace("CREATE TABLE blacklisted_", "CREATE TABLE IF NOT EXISTS blacklisted_");
        if let Err(err) = sqlx::raw_sql(&base_sql).execute(&self.pool).await {
            warn!(?err, "database base migration check failed");
        }

        const OSU_LINKS_SQL: &str = "
            CREATE TABLE IF NOT EXISTS osu_links (
                discord_id BIGINT PRIMARY KEY,
                osu_user_id BIGINT NOT NULL,
                osu_username TEXT NOT NULL
            );

            DELETE FROM osu_links a
            USING osu_links b
            WHERE a.osu_user_id = b.osu_user_id
              AND a.ctid < b.ctid;

            CREATE UNIQUE INDEX IF NOT EXISTS idx_osu_links_osu_user_id ON osu_links (osu_user_id);
        ";
        if let Err(err) = sqlx::raw_sql(OSU_LINKS_SQL).execute(&self.pool).await {
            warn!(?err, "database osu_links migration check failed");
        }

        const WEB_AUTH_SQL: &str = "
            CREATE TABLE IF NOT EXISTS web_user_profiles (
                osu_user_id  BIGINT PRIMARY KEY,
                username     TEXT NOT NULL,
                avatar_url   TEXT NOT NULL DEFAULT '',
                country_code TEXT NOT NULL DEFAULT '',
                theme        TEXT NOT NULL DEFAULT '',
                updated_at   BIGINT NOT NULL
            );

            ALTER TABLE web_user_profiles ADD COLUMN IF NOT EXISTS theme TEXT NOT NULL DEFAULT '';

            CREATE TABLE IF NOT EXISTS web_sessions (
                token_hash   BYTEA PRIMARY KEY,
                osu_user_id  BIGINT NOT NULL REFERENCES web_user_profiles(osu_user_id) ON DELETE CASCADE,
                expires_at   BIGINT NOT NULL,
                created_at   BIGINT NOT NULL,
                last_seen_at BIGINT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_web_sessions_expires_at ON web_sessions (expires_at);

            CREATE TABLE IF NOT EXISTS discord_link_tokens (
                token_hash  BYTEA PRIMARY KEY,
                discord_id  BIGINT NOT NULL,
                channel_id  BIGINT NOT NULL,
                guild_id    BIGINT,
                expires_at  BIGINT NOT NULL,
                created_at  BIGINT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_discord_link_tokens_expires_at ON discord_link_tokens (expires_at);
        ";
        if let Err(err) = sqlx::raw_sql(WEB_AUTH_SQL).execute(&self.pool).await {
            warn!(?err, "database web_auth migration check failed");
        }
    }
}
