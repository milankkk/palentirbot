use eyre::{Result, WrapErr};
use sqlx::Row;
use twilight_model::id::{marker::UserMarker, Id};

use crate::database::Database;

impl Database {
    pub async fn link_osu_user(
        &self,
        discord_id: Id<UserMarker>,
        osu_user_id: u32,
        osu_username: &str,
    ) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .wrap_err("failed to begin osu_links transaction")?;

        // Enforce 1-to-1 mapping between osu_user_id and discord_id so two Discord accounts
        // cannot share the same verified osu! profile.
        sqlx::query("DELETE FROM osu_links WHERE osu_user_id = $1 AND discord_id <> $2")
            .bind(i64::from(osu_user_id))
            .bind(discord_id.get() as i64)
            .execute(&mut *tx)
            .await
            .wrap_err("failed to clear prior osu! profile link")?;

        sqlx::query(
            "
            INSERT INTO osu_links (discord_id, osu_user_id, osu_username)
            VALUES ($1, $2, $3)
            ON CONFLICT (discord_id)
            DO UPDATE SET
                osu_user_id = EXCLUDED.osu_user_id,
                osu_username = EXCLUDED.osu_username
            ",
        )
        .bind(discord_id.get() as i64)
        .bind(i64::from(osu_user_id))
        .bind(osu_username)
        .execute(&mut *tx)
        .await
        .wrap_err("failed to store osu! profile link")?;

        tx.commit()
            .await
            .wrap_err("failed to commit osu! profile link")?;

        Ok(())
    }

    pub async fn get_linked_osu_user(
        &self,
        discord_id: Id<UserMarker>,
    ) -> Result<Option<(u32, String)>> {
        let row = sqlx::query(
            "SELECT osu_user_id, osu_username FROM osu_links WHERE discord_id = $1",
        )
        .bind(discord_id.get() as i64)
        .fetch_optional(&self.pool)
        .await
        .wrap_err("failed to fetch linked osu! profile")?;

        let Some(row) = row else {
            return Ok(None);
        };

        let raw_id: i64 = row.get("osu_user_id");
        let Ok(osu_user_id) = u32::try_from(raw_id) else {
            return Ok(None);
        };
        let osu_username: String = row.get("osu_username");
        Ok(Some((osu_user_id, osu_username)))
    }
}
