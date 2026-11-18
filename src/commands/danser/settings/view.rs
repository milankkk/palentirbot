use std::{io::ErrorKind, sync::Arc};

use eyre::{Context as _, Report, Result};
use tokio::fs;
use twilight_model::channel::message::component::{ActionRow, Button, ButtonStyle, Component};

use crate::{
    core::{settings::DanserSettings, BotConfig, Context},
    util::{
        builder::MessageBuilder, interaction::InteractionCommand, Authored, InteractionCommandExt,
    },
};

use super::{create_settings_embed, SettingsView};

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

    let danser_dir = BotConfig::get().paths.danser();
    let user_path = danser_dir.join(format!("settings/{}.json", target_user.id));

    let (settings, using_default): (DanserSettings, bool) = match fs::read(&user_path).await {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(settings) => (settings, false),
            Err(err) => {
                let content = "Failed to read settings file";
                let _ = command.error_callback(&ctx, content, false).await;

                let err = Report::from(err)
                    .wrap_err(format!("Failed to deserialize file at {user_path:?}"));

                return Err(err);
            }
        },
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if !is_self {
                let content = format!("User <@{}> has no specified settings yet", target_user.id);
                let builder = MessageBuilder::new().embed(content);
                command.callback(&ctx, builder, false).await?;

                return Ok(());
            }

            let default_path = danser_dir.join("settings/default.json");
            let bytes = fs::read(&default_path)
                .await
                .context("failed to read default.json")?;
            let settings: DanserSettings = serde_json::from_slice(&bytes)
                .context("failed to deserialize default.json")?;
            (settings, true)
        }
        Err(err) => {
            let content = "Failed to open settings file";
            let _ = command.error_callback(&ctx, content, false).await;
            let err = Report::from(err).wrap_err("failed to open settings file");

            return Err(err);
        }
    };

    let embed = create_settings_embed(&target_user, &settings);
    let mut builder = MessageBuilder::new().embed(embed);
    if using_default {
        builder = builder.content("Showing default settings (no custom changes yet).");
    }
    if is_self {
        let open_btn = Button {
            sku_id: None,
            custom_id: Some(format!("uset_open:{caller_id}")),
            disabled: false,
            emoji: None,
            label: Some("⚙️ Open Settings Menu".to_owned()),
            style: ButtonStyle::Primary,
            url: None,
        };
        builder = builder.components(vec![Component::ActionRow(ActionRow {
            components: vec![Component::Button(open_btn)],
        })]);
    }
    command.callback(&ctx, builder, false).await?;

    Ok(())
}
