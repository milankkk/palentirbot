use std::time::Duration;

use eyre::Result;
use sqlx::{postgres::PgPoolOptions, PgPool};

mod impls;

pub struct Database {
    pool: PgPool,
}

impl Database {
    #[cold]
    pub fn new(uri: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .acquire_timeout(Duration::from_secs(5))
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
        ";
        if let Err(err) = sqlx::raw_sql(OSU_LINKS_SQL).execute(&self.pool).await {
            warn!(?err, "database osu_links migration check failed");
        }
    }
}
