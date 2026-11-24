use std::{io::ErrorKind, sync::Arc};

use eyre::{Context as _, Report, Result};
use tokio::fs;
use twilight_model::channel::message::component::ButtonStyle;

use crate::{
    core::{settings::DanserSettings, Context},
    util::{
        builder::{action_row, button, MessageBuilder},
        interaction::InteractionCommand,
        Authored, InteractionCommandExt,
    },
};

use super::{create_settings_embed, default_settings_path, user_settings_path, SettingsView};

pub async fn view(
    ctx: Arc<Context>,
    command: InteractionCommand,
    args: SettingsView,
) -> Result<()> {
    let caller_id = command.user_id()?;
    let target_user = match args.user {
        Some(resolved) => resolved.resolved,
        None => command.user()?.clone(),
    };
    let is_self = target_user.id == caller_id;

    let user_path = user_settings_path(target_user.id);

    let (settings, using_default): (DanserSettings, bool) = match fs::read(&user_path).await {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(settings) => (settings, false),
            Err(err) => {
                let _ = command
                    .error_callback(&ctx, "Failed to read settings file", false)
                    .await;

                return Err(
                    Report::from(err).wrap_err(format!("Failed to deserialize file at {user_path:?}"))
                );
            }
        },
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if !is_self {
                let content = format!("User <@{}> has no specified settings yet", target_user.id);
                command
                    .callback(&ctx, MessageBuilder::new().embed(content), false)
                    .await?;

                return Ok(());
            }

            let default_path = default_settings_path();
            let bytes = fs::read(&default_path)
                .await
                .context("failed to read default.json")?;
            let settings: DanserSettings = serde_json::from_slice(&bytes)
                .context("failed to deserialize default.json")?;
            (settings, true)
        }
        Err(err) => {
            let _ = command
                .error_callback(&ctx, "Failed to open settings file", false)
                .await;

            return Err(Report::from(err).wrap_err("failed to open settings file"));
        }
    };

    let is_linked = ctx
        .psql()
        .get_linked_osu_user(target_user.id)
        .await
        .ok()
        .flatten()
        .is_some();
    let embed = create_settings_embed(&target_user, &settings, is_linked);
    let mut builder = MessageBuilder::new().embed(embed);
    if using_default {
        builder = builder.content("Showing default settings (no custom changes yet).");
    }
    if is_self {
        builder = builder.components(vec![action_row(vec![button(
            format!("uset_open:{caller_id}"),
            "⚙️ Open Settings Menu",
            ButtonStyle::Primary,
        )])]);
    }
    command.callback(&ctx, builder, false).await?;
    if is_self {
        crate::core::events::interaction::spawn_interaction_component_expiry(
            Arc::clone(&ctx),
            command.token,
        );
    }

    Ok(())
}
