use eyre::{Result, WrapErr};
use sqlx::Row;
use twilight_model::id::{
    marker::{GuildMarker, UserMarker},
    Id,
};

use crate::database::Database;

/// Result of checking user and server blacklists for an incoming command.
pub enum BlacklistHit {
    User(Option<String>),
    Server(Option<String>),
}

#[derive(Copy, Clone)]
enum BlacklistKind {
    Server,
    User,
}

impl BlacklistKind {
    fn upsert_sql(self) -> &'static str {
        match self {
            Self::Server => {
                "INSERT INTO blacklisted_servers (guild_id, reason) \
                 VALUES ($1, $2) ON CONFLICT (guild_id) DO UPDATE SET reason = EXCLUDED.reason"
            }
            Self::User => {
                "INSERT INTO blacklisted_users (user_id, reason) \
                 VALUES ($1, $2) ON CONFLICT (user_id) DO UPDATE SET reason = EXCLUDED.reason"
            }
        }
    }

    fn delete_sql(self) -> &'static str {
        match self {
            Self::Server => "DELETE FROM blacklisted_servers WHERE guild_id = $1",
            Self::User => "DELETE FROM blacklisted_users WHERE user_id = $1",
        }
    }

    fn select_sql(self) -> &'static str {
        match self {
            Self::Server => "SELECT reason FROM blacklisted_servers WHERE guild_id = $1",
            Self::User => "SELECT reason FROM blacklisted_users WHERE user_id = $1",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::User => "user",
        }
    }
}

fn clamp_reason(reason: Option<String>) -> Option<String> {
    reason.map(|r| r.chars().take(256).collect())
}

impl Database {
    async fn set_blacklisted(
        &self,
        kind: BlacklistKind,
        id: u64,
        reason: Option<String>,
    ) -> Result<()> {
        sqlx::query(kind.upsert_sql())
            .bind(id as i64)
            .bind(clamp_reason(reason))
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to store blacklisted {}", kind.label()))?;

        Ok(())
    }

    async fn clear_blacklisted(&self, kind: BlacklistKind, id: u64) -> Result<bool> {
        let result = sqlx::query(kind.delete_sql())
            .bind(id as i64)
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to delete blacklisted {}", kind.label()))?;

        Ok(result.rows_affected() > 0)
    }

    async fn fetch_blacklist_entry(
        &self,
        kind: BlacklistKind,
        id: u64,
    ) -> Result<(bool, Option<String>)> {
        let row = sqlx::query(kind.select_sql())
            .bind(id as i64)
            .fetch_optional(&self.pool)
            .await
            .with_context(|| format!("failed to check if {} is blacklisted", kind.label()))?;

        Ok(match row {
            Some(row) => (true, row.get::<Option<String>, _>("reason")),
            None => (false, None),
        })
    }

    pub async fn blacklist_server(&self, guild_id: u64, reason: Option<String>) -> Result<()> {
        self.set_blacklisted(BlacklistKind::Server, guild_id, reason)
            .await
    }

    pub async fn whitelist_server(&self, guild_id: u64) -> Result<bool> {
        self.clear_blacklisted(BlacklistKind::Server, guild_id).await
    }

    pub async fn blacklist_user(&self, user_id: u64, reason: Option<String>) -> Result<()> {
        self.set_blacklisted(BlacklistKind::User, user_id, reason)
            .await
    }

    pub async fn whitelist_user(&self, user_id: u64) -> Result<bool> {
        self.clear_blacklisted(BlacklistKind::User, user_id).await
    }

    /// Check both user and optional server blacklists, returning any matching hit or propagating DB errors.
    pub async fn check_blacklisted(
        &self,
        user_id: Id<UserMarker>,
        guild_id: Option<Id<GuildMarker>>,
    ) -> Result<Option<BlacklistHit>> {
        if let (true, reason) = self
            .fetch_blacklist_entry(BlacklistKind::User, user_id.get())
            .await?
        {
            return Ok(Some(BlacklistHit::User(reason)));
        }

        if let Some(gid) = guild_id {
            if let (true, reason) = self
                .fetch_blacklist_entry(BlacklistKind::Server, gid.get())
                .await?
            {
                return Ok(Some(BlacklistHit::Server(reason)));
            }
        }

        Ok(None)
    }
}
