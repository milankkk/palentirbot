use eyre::{Result, WrapErr};
use sqlx::Row;

use crate::database::Database;

pub const WEB_SESSION_TTL_SECS: i64 = 30 * 24 * 60 * 60;
const WEB_SESSION_RENEW_THRESHOLD_SECS: i64 = 24 * 60 * 60;
pub const DISCORD_LINK_TTL_SECS: i64 = 10 * 60;

#[derive(Clone, Debug)]
pub struct DiscordLinkPending {
    pub discord_id: u64,
    pub channel_id: u64,
    pub guild_id: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct WebUserProfile {
    pub osu_user_id: u32,
    pub username: String,
    pub avatar_url: String,
    pub country_code: String,
    pub theme: String,
    pub linked_discord_id: Option<u64>,
    pub session_expires_at: i64,
    pub session_renewed: bool,
}

impl Database {
    pub async fn upsert_web_user_profile(
        &self,
        osu_user_id: u32,
        username: &str,
        avatar_url: &str,
        country_code: &str,
        now: i64,
    ) -> Result<()> {
        sqlx::query(
            "
            INSERT INTO web_user_profiles (osu_user_id, username, avatar_url, country_code, updated_at)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (osu_user_id)
            DO UPDATE SET
                username = EXCLUDED.username,
                avatar_url = EXCLUDED.avatar_url,
                country_code = EXCLUDED.country_code,
                updated_at = EXCLUDED.updated_at
            ",
        )
        .bind(i64::from(osu_user_id))
        .bind(username)
        .bind(avatar_url)
        .bind(country_code)
        .bind(now)
        .execute(&self.pool)
        .await
        .wrap_err("failed to upsert web user profile")?;

        Ok(())
    }

    pub async fn create_web_session(
        &self,
        token_hash: &[u8; 32],
        osu_user_id: u32,
        now: i64,
        expires_at: i64,
    ) -> Result<()> {
        sqlx::query(
            "
            INSERT INTO web_sessions (token_hash, osu_user_id, expires_at, created_at, last_seen_at)
            VALUES ($1, $2, $3, $4, $4)
            ",
        )
        .bind(token_hash.as_slice())
        .bind(i64::from(osu_user_id))
        .bind(expires_at)
        .bind(now)
        .execute(&self.pool)
        .await
        .wrap_err("failed to insert web session")?;

        // Opportunistic cleanup of expired sessions so the table stays small.
        let _ = sqlx::query("DELETE FROM web_sessions WHERE expires_at <= $1")
            .bind(now)
            .execute(&self.pool)
            .await;

        Ok(())
    }

    pub async fn get_web_session_user(
        &self,
        token_hash: &[u8; 32],
        now: i64,
    ) -> Result<Option<WebUserProfile>> {
        let row = sqlx::query(
            "
            SELECT
                p.osu_user_id,
                p.username,
                p.avatar_url,
                p.country_code,
                p.theme,
                s.expires_at,
                s.last_seen_at,
                (SELECT l.discord_id FROM osu_links l WHERE l.osu_user_id = p.osu_user_id LIMIT 1) AS discord_id
            FROM web_sessions s
            JOIN web_user_profiles p ON p.osu_user_id = s.osu_user_id
            WHERE s.token_hash = $1 AND s.expires_at > $2
            ",
        )
        .bind(token_hash.as_slice())
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .wrap_err("failed to look up web session")?;

        let Some(row) = row else {
            return Ok(None);
        };

        let raw_osu_id: i64 = row.get("osu_user_id");
        let Ok(osu_user_id) = u32::try_from(raw_osu_id) else {
            return Ok(None);
        };
        let username: String = row.get("username");
        let avatar_url: String = row.get("avatar_url");
        let country_code: String = row.get("country_code");
        let theme: String = row.get("theme");
        let mut expires_at: i64 = row.get("expires_at");
        let last_seen_at: i64 = row.get("last_seen_at");
        let linked_discord_id: Option<u64> = row
            .get::<Option<i64>, _>("discord_id")
            .and_then(|v| u64::try_from(v).ok());

        // Rolling 30-day renewal once per day so active devices stay logged in seamlessly.
        let mut session_renewed = false;
        if now.saturating_sub(last_seen_at) >= WEB_SESSION_RENEW_THRESHOLD_SECS {
            let new_expires = now.saturating_add(WEB_SESSION_TTL_SECS);
            if sqlx::query(
                "UPDATE web_sessions SET expires_at = $1, last_seen_at = $2 WHERE token_hash = $3",
            )
            .bind(new_expires)
            .bind(now)
            .bind(token_hash.as_slice())
            .execute(&self.pool)
            .await
            .is_ok()
            {
                expires_at = new_expires;
                session_renewed = true;
            }
        }

        Ok(Some(WebUserProfile {
            osu_user_id,
            username,
            avatar_url,
            country_code,
            theme,
            linked_discord_id,
            session_expires_at: expires_at,
            session_renewed,
        }))
    }

    pub async fn set_web_user_theme(&self, osu_user_id: u32, theme: &str) -> Result<()> {
        sqlx::query("UPDATE web_user_profiles SET theme = $1 WHERE osu_user_id = $2")
            .bind(theme)
            .bind(i64::from(osu_user_id))
            .execute(&self.pool)
            .await
            .wrap_err("failed to save web user theme")?;

        Ok(())
    }

    pub async fn delete_web_session(&self, token_hash: &[u8; 32]) -> Result<()> {
        sqlx::query("DELETE FROM web_sessions WHERE token_hash = $1")
            .bind(token_hash.as_slice())
            .execute(&self.pool)
            .await
            .wrap_err("failed to delete web session")?;

        Ok(())
    }

    pub async fn create_discord_link_token(
        &self,
        token_hash: &[u8; 32],
        discord_id: u64,
        channel_id: u64,
        guild_id: Option<u64>,
        now: i64,
        expires_at: i64,
    ) -> Result<()> {
        // Keep at most one active pending verification link per Discord user and clean expired rows.
        let _ = sqlx::query(
            "DELETE FROM discord_link_tokens WHERE discord_id = $1 OR expires_at <= $2",
        )
        .bind(discord_id as i64)
        .bind(now)
        .execute(&self.pool)
        .await;

        sqlx::query(
            "
            INSERT INTO discord_link_tokens (token_hash, discord_id, channel_id, guild_id, expires_at, created_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            ",
        )
        .bind(token_hash.as_slice())
        .bind(discord_id as i64)
        .bind(channel_id as i64)
        .bind(guild_id.map(|g| g as i64))
        .bind(expires_at)
        .bind(now)
        .execute(&self.pool)
        .await
        .wrap_err("failed to create discord link token")?;

        Ok(())
    }

    pub async fn peek_discord_link_token(
        &self,
        token_hash: &[u8; 32],
        now: i64,
    ) -> Result<Option<DiscordLinkPending>> {
        let row = sqlx::query(
            "
            SELECT discord_id, channel_id, guild_id
            FROM discord_link_tokens
            WHERE token_hash = $1 AND expires_at > $2
            ",
        )
        .bind(token_hash.as_slice())
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .wrap_err("failed to check discord link token")?;

        Ok(row.and_then(|r| {
            let discord_id = u64::try_from(r.get::<i64, _>("discord_id")).ok()?;
            let channel_id = u64::try_from(r.get::<i64, _>("channel_id")).ok()?;
            let guild_id = r
                .get::<Option<i64>, _>("guild_id")
                .and_then(|v| u64::try_from(v).ok());
            Some(DiscordLinkPending {
                discord_id,
                channel_id,
                guild_id,
            })
        }))
    }

    pub async fn consume_discord_link_token(
        &self,
        token_hash: &[u8; 32],
        now: i64,
    ) -> Result<Option<DiscordLinkPending>> {
        let row = sqlx::query(
            "
            DELETE FROM discord_link_tokens
            WHERE token_hash = $1 AND expires_at > $2
            RETURNING discord_id, channel_id, guild_id
            ",
        )
        .bind(token_hash.as_slice())
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .wrap_err("failed to consume discord link token")?;

        Ok(row.and_then(|r| {
            let discord_id = u64::try_from(r.get::<i64, _>("discord_id")).ok()?;
            let channel_id = u64::try_from(r.get::<i64, _>("channel_id")).ok()?;
            let guild_id = r
                .get::<Option<i64>, _>("guild_id")
                .and_then(|v| u64::try_from(v).ok());
            Some(DiscordLinkPending {
                discord_id,
                channel_id,
                guild_id,
            })
        }))
    }

    pub async fn delete_discord_link_token(&self, token_hash: &[u8; 32]) -> Result<()> {
        sqlx::query("DELETE FROM discord_link_tokens WHERE token_hash = $1")
            .bind(token_hash.as_slice())
            .execute(&self.pool)
            .await
            .wrap_err("failed to delete discord link token")?;

        Ok(())
    }
}
