use std::sync::Arc;
use eyre::{Context as _, Result};
use twilight_model::channel::message::component::{ActionRow, Component, SelectMenu, SelectMenuOption, SelectMenuType, Button, ButtonStyle};

use crate::{
    core::{BotConfig, Context},
    util::{builder::MessageBuilder, interaction::{InteractionCommand, InteractionComponent, InteractionModal}, ComponentExt, InteractionCommandExt, builder::ModalBuilder, ModalExt},
};

pub async fn settings(ctx: Arc<Context>, command: InteractionCommand) -> Result<()> {
    let mut default_path = BotConfig::get().paths.danser().to_owned();
    default_path.push("settings/default.json");

    let file = std::fs::File::open(&default_path).context("failed to open default.json")?;
    let json: serde_json::Value = serde_json::from_reader(file).context("failed to parse default.json")?;

    let mut options = Vec::new();
    if let serde_json::Value::Object(map) = json {
        for (key, val) in map {
            if key != "General" && val.is_object() {
                options.push(SelectMenuOption {
                    default: false,
                    description: None,
                    emoji: None,
                    label: key.clone(),
                    value: key,
                });
            }
        }
    }

    if options.is_empty() {
        let builder = MessageBuilder::new().content("No sections found.");
        command.callback(&ctx, builder, true).await?;
        return Ok(());
    }

    let select_menu = SelectMenu {
        custom_id: "owner_settings_section".to_string(),
        disabled: false,
        max_values: Some(1),
        min_values: Some(1),
        options: Some(options),
        placeholder: Some("Select a settings block".to_string()),
        channel_types: None,
        default_values: None,
        kind: SelectMenuType::Text,
    };

    let components = vec![Component::ActionRow(ActionRow {
        components: vec![Component::SelectMenu(select_menu)],
    })];

    let builder = MessageBuilder::new()
        .content("Select a settings block to edit:")
        .components(components);

    command.callback(&ctx, builder, true).await?;
    Ok(())
}

pub async fn handle_owner_settings_section(ctx: Arc<Context>, component: InteractionComponent) -> Result<()> {
    let section = component.data.values.first().cloned().unwrap_or_default();
    if section.is_empty() {
        return Ok(());
    }

    let mut default_path = BotConfig::get().paths.danser().to_owned();
    default_path.push("settings/default.json");
    let file = std::fs::File::open(&default_path).context("failed to open default.json")?;
    let json: serde_json::Value = serde_json::from_reader(file).context("failed to parse default.json")?;

    let mut options = Vec::new();
    if let Some(section_val) = json.get(&section) {
        if let serde_json::Value::Object(map) = section_val {
            for (key, _) in map {
                options.push(SelectMenuOption {
                    default: false,
                    description: None,
                    emoji: None,
                    label: key.clone(),
                    value: key.clone(),
                });
            }
        }
    }

    options.truncate(25);

    if options.is_empty() {
        let builder = MessageBuilder::new().content("No properties found in this block.");
        component.callback(&ctx, builder).await?;
        return Ok(());
    }

    let select_menu = SelectMenu {
        custom_id: format!("owner_settings_prop_{}", section),
        disabled: false,
        max_values: Some(1),
        min_values: Some(1),
        options: Some(options),
        placeholder: Some(format!("Select a property in {}", section)),
        channel_types: None,
        default_values: None,
        kind: SelectMenuType::Text,
    };

    let components = vec![Component::ActionRow(ActionRow {
        components: vec![Component::SelectMenu(select_menu)],
    })];

    let builder = MessageBuilder::new()
        .content(format!("Editing block: **{}**", section))
        .components(components);

    component.callback(&ctx, builder).await?;
    Ok(())
}

pub async fn handle_owner_settings_prop(ctx: Arc<Context>, component: InteractionComponent) -> Result<()> {
    let custom_id = &component.data.custom_id;
    let section = custom_id.trim_start_matches("owner_settings_prop_");
    let prop = component.data.values.first().cloned().unwrap_or_default();
    
    tracing::info!("handle_owner_settings_prop: section='{}', prop='{}'", section, prop);

    if prop.is_empty() {
        return Ok(());
    }

    let mut default_path = BotConfig::get().paths.danser().to_owned();
    default_path.push("settings/default.json");
    let file = std::fs::File::open(&default_path).context("failed to open default.json")?;
    let json: serde_json::Value = serde_json::from_reader(file).context("failed to parse default.json")?;

    let current_val = json.get(section).and_then(|s| s.get(&prop)).map(|v| v.to_string()).unwrap_or_default();
    let current_val = current_val.trim_matches('"'); 
    
    tracing::info!("handle_owner_settings_prop: current_val='{}'", current_val);

    let modal = ModalBuilder::new("settings_value", format!("Value for {}", prop))
        .modal_id(format!("oset_modal_{}_{}", section, prop))
        .placeholder(current_val.to_string())
        .value(current_val.to_string())
        .title(format!("Edit {}", prop));

    component.modal(&ctx, modal).await?;
    Ok(())
}

pub async fn handle_owner_settings_modal(ctx: Arc<Context>, modal: InteractionModal) -> Result<()> {
    let custom_id = &modal.data.custom_id;
    let parts: Vec<&str> = custom_id.trim_start_matches("oset_modal_").splitn(2, '_').collect();
    if parts.len() != 2 {
        return Ok(());
    }
    let section = parts[0];
    let prop = parts[1];

    modal.defer_ephemeral(&ctx).await.context("failed to defer modal")?;

    let new_value_str = modal
        .data
        .components
        .first()
        .and_then(|component| component.components.first())
        .and_then(|input| input.value.clone())
        .unwrap_or_default();

    let mut default_path = BotConfig::get().paths.danser().to_owned();
    default_path.push("settings/default.json");
    let file = std::fs::File::open(&default_path).context("failed to open default.json")?;
    let mut json: serde_json::Value = serde_json::from_reader(file).context("failed to parse default.json")?;

    if let Some(section_val) = json.get_mut(section) {
        if let Some(prop_val) = section_val.get_mut(prop) {
            if prop_val.is_boolean() {
                if let Ok(b) = new_value_str.parse::<bool>() {
                    *prop_val = serde_json::Value::Bool(b);
                } else if new_value_str == "1" {
                    *prop_val = serde_json::Value::Bool(true);
                } else if new_value_str == "0" {
                    *prop_val = serde_json::Value::Bool(false);
                } else {
                    tracing::error!("Failed to parse '{}' as boolean for {}.{}", new_value_str, section, prop);
                }
            } else if prop_val.is_i64() {
                if let Ok(i) = new_value_str.parse::<i64>() {
                    *prop_val = serde_json::Value::Number(i.into());
                } else {
                    tracing::error!("Failed to parse '{}' as i64 for {}.{}", new_value_str, section, prop);
                }
            } else if prop_val.is_f64() {
                if let Ok(f) = new_value_str.parse::<f64>() {
                    if let Some(n) = serde_json::Number::from_f64(f) {
                        *prop_val = serde_json::Value::Number(n);
                    }
                } else {
                    tracing::error!("Failed to parse '{}' as f64 for {}.{}", new_value_str, section, prop);
                }
            } else {
                *prop_val = serde_json::Value::String(new_value_str.clone());
            }
        } else {
            tracing::error!("Property '{}' not found in section '{}'", prop, section);
        }
    } else {
        tracing::error!("Section '{}' not found in default.json", section);
    }

    let file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&default_path)
        .context("failed to open default.json for writing")?;
    serde_json::to_writer_pretty(file, &json).context("failed to write default.json")?;

    let apply_btn = Button {
        sku_id: None,
        custom_id: Some(format!("oset_applyall_{}_{}", section, prop)),
        disabled: false,
        emoji: None,
        label: Some("Apply to all existing configs".to_string()),
        style: ButtonStyle::Primary,
        url: None,
    };

    let skip_btn = Button {
        sku_id: None,
        custom_id: Some("oset_skipall".to_string()),
        disabled: false,
        emoji: None,
        label: Some("No, only default".to_string()),
        style: ButtonStyle::Secondary,
        url: None,
    };

    let components = vec![Component::ActionRow(ActionRow {
        components: vec![Component::Button(apply_btn), Component::Button(skip_btn)],
    })];

    let builder = MessageBuilder::new()
        .content(format!("Successfully updated `{}.{}` to `{}` in `default.json`.\n\nShould this be applied to all user configs that already exist?", section, prop, new_value_str))
        .components(components);
    modal.update_response(&ctx, &builder).await?;

    Ok(())
}

pub async fn handle_owner_settings_skipall(ctx: Arc<Context>, component: InteractionComponent) -> Result<()> {
    let builder = MessageBuilder::new().content("Settings change applied only to the default config.").components(vec![]);
    component.callback(&ctx, builder).await?;
    Ok(())
}

pub async fn handle_owner_settings_applyall(ctx: Arc<Context>, component: InteractionComponent) -> Result<()> {
    let custom_id = &component.data.custom_id;
    let parts: Vec<&str> = custom_id.trim_start_matches("oset_applyall_").splitn(2, '_').collect();
    if parts.len() != 2 {
        return Ok(());
    }
    let section = parts[0];
    let prop = parts[1];

    let mut default_path = BotConfig::get().paths.danser().to_owned();
    default_path.push("settings/default.json");
    let file = std::fs::File::open(&default_path).context("failed to open default.json")?;
    let default_json: serde_json::Value = serde_json::from_reader(file).context("failed to parse default.json")?;

    let default_val = default_json.get(section).and_then(|s| s.get(prop));
    
    if default_val.is_none() {
        let builder = MessageBuilder::new().content("Failed to find the setting in default.json!");
        component.callback(&ctx, builder).await?;
        return Ok(());
    }
    
    let default_val = default_val.unwrap().clone();

    let mut settings_dir = BotConfig::get().paths.danser().to_owned();
    settings_dir.push("settings");

    let mut applied_count = 0;

    let mut dir = tokio::fs::read_dir(&settings_dir).await?;
    while let Ok(Some(entry)) = dir.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        
        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if filename == "default.json" {
            continue;
        }

        if let Ok(file) = std::fs::File::open(&path) {
            if let Ok(mut json) = serde_json::from_reader::<_, serde_json::Value>(file) {
                if let Some(section_val) = json.get_mut(section) {
                    if let Some(prop_val) = section_val.get_mut(prop) {
                        *prop_val = default_val.clone();
                        if let Ok(file) = std::fs::OpenOptions::new().write(true).truncate(true).open(&path) {
                            if serde_json::to_writer_pretty(file, &json).is_ok() {
                                applied_count += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    let builder = MessageBuilder::new()
        .content(format!("Successfully applied `{}.{}` to {} user configs.", section, prop, applied_count))
        .components(vec![]);
    component.callback(&ctx, builder).await?;

    Ok(())
}
