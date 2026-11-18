use std::{
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use eyre::{Context as _, Result};
use tokio::{io::AsyncWriteExt as _, sync::Mutex};
use twilight_model::channel::message::component::{
    ActionRow, Button, ButtonStyle, Component, SelectMenu, SelectMenuOption, SelectMenuType,
};

use crate::{
    core::{BotConfig, Context},
    util::{
        builder::{MessageBuilder, ModalBuilder},
        constants::NOT_OWNER,
        interaction::{InteractionCommand, InteractionComponent, InteractionModal},
        Authored, ComponentExt, InteractionCommandExt, ModalExt,
    },
};

fn escape_pointer_segment(seg: &str) -> String {
    seg.replace('~', "~0").replace('/', "~1")
}

fn join_pointer(parent: &str, seg: &str) -> String {
    let escaped = escape_pointer_segment(seg);
    if parent.is_empty() || parent == "/" {
        format!("/{escaped}")
    } else {
        format!("{parent}/{escaped}")
    }
}

fn format_pointer_display(pointer: &str) -> String {
    pointer
        .trim_start_matches('/')
        .split('/')
        .map(|s| s.replace("~1", "/").replace("~0", "~"))
        .collect::<Vec<_>>()
        .join(".")
}

fn pointer_leaf_name(pointer: &str) -> String {
    pointer
        .rsplit('/')
        .next()
        .unwrap_or(pointer)
        .replace("~1", "/")
        .replace("~0", "~")
}

fn build_object_select_rows(
    map: &serde_json::Map<String, serde_json::Value>,
    parent_pointer: &str,
) -> Vec<Component> {
    let mut options: Vec<SelectMenuOption> = map
        .iter()
        .map(|(key, val)| {
            let description = if val.is_object() {
                Some("Nested settings block (open)".to_owned())
            } else {
                let preview = val.to_string();
                let short: String = preview.chars().take(80).collect();
                Some(format!("Current: {short}"))
            };

            let label: String = if val.is_object() {
                format!("{key} ▸")
            } else {
                key.clone()
            };

            SelectMenuOption {
                default: false,
                description,
                emoji: None,
                label: label.chars().take(100).collect(),
                value: key.chars().take(100).collect(),
            }
        })
        .collect();

    if options.is_empty() {
        return Vec::new();
    }

    // Discord allows up to 5 ActionRows per message and 25 options per SelectMenu.
    let total_chunks = options.len().div_ceil(25).min(5);
    let mut rows = Vec::with_capacity(total_chunks);

    for chunk_idx in 0..total_chunks {
        let take = options.len().min(25);
        let chunk_opts: Vec<SelectMenuOption> = options.drain(..take).collect();
        let label = format_pointer_display(parent_pointer);
        let placeholder = if total_chunks > 1 {
            format!(
                "Select a property in {} ({}/{})",
                label,
                chunk_idx + 1,
                total_chunks
            )
        } else {
            format!("Select a property in {label}")
        };

        let custom_id = format!("owner_settings_prop_{chunk_idx}:{parent_pointer}");
        if custom_id.len() > 100 {
            return Vec::new();
        }

        let menu = SelectMenu {
            custom_id,
            disabled: false,
            max_values: Some(1),
            min_values: Some(1),
            options: Some(chunk_opts),
            placeholder: Some(placeholder.chars().take(150).collect()),
            channel_types: None,
            default_values: None,
            kind: SelectMenuType::Text,
        };

        rows.push(Component::ActionRow(ActionRow {
            components: vec![Component::SelectMenu(menu)],
        }));
    }

    rows
}

static WRITE_SETTINGS_LOCK: Mutex<()> = Mutex::const_new(());
static WRITE_TMP_SEQ: AtomicU64 = AtomicU64::new(1);

async fn write_json_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let _guard = WRITE_SETTINGS_LOCK.lock().await;
    let seq = WRITE_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("settings.json");
    let tmp = path.with_file_name(format!(".{file_name}.{pid}_{seq}.tmp"));

    let write_res: Result<()> = async {
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("failed to create temp settings file {tmp:?}"))?;
        file.write_all(bytes)
            .await
            .with_context(|| format!("failed to write temp settings file {tmp:?}"))?;
        file.sync_all()
            .await
            .with_context(|| format!("failed to sync temp settings file {tmp:?}"))?;
        drop(file);
        tokio::fs::rename(&tmp, path)
            .await
            .with_context(|| format!("failed to replace settings file {path:?}"))?;
        Ok(())
    }
    .await;

    if write_res.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    write_res
}

pub async fn settings(ctx: Arc<Context>, command: InteractionCommand) -> Result<()> {
    let mut default_path = BotConfig::get().paths.danser().to_owned();
    default_path.push("settings/default.json");

    let bytes = tokio::fs::read(&default_path)
        .await
        .context("failed to read default.json")?;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).context("failed to parse default.json")?;

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

    let total_chunks = options.len().div_ceil(25).min(5);
    let mut components = Vec::with_capacity(total_chunks);
    for idx in 0..total_chunks {
        let take = options.len().min(25);
        let chunk_opts: Vec<_> = options.drain(..take).collect();
        let select_menu = SelectMenu {
            custom_id: if idx == 0 {
                "owner_settings_section".to_string()
            } else {
                format!("owner_settings_section_{idx}")
            },
            disabled: false,
            max_values: Some(1),
            min_values: Some(1),
            options: Some(chunk_opts),
            placeholder: Some("Select a settings block".to_string()),
            channel_types: None,
            default_values: None,
            kind: SelectMenuType::Text,
        };
        components.push(Component::ActionRow(ActionRow {
            components: vec![Component::SelectMenu(select_menu)],
        }));
    }

    let builder = MessageBuilder::new()
        .content("Select a settings block to edit:")
        .components(components);

    command.callback(&ctx, builder, true).await?;
    Ok(())
}

pub async fn handle_owner_settings_section(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let user_id = component.user_id()?;
    if !BotConfig::get().owners.contains(&user_id) {
        component
            .callback(&ctx, MessageBuilder::new().content(NOT_OWNER))
            .await?;
        return Ok(());
    }

    let section = component.data.values.first().cloned().unwrap_or_default();
    if section.is_empty() {
        return Ok(());
    }

    let mut default_path = BotConfig::get().paths.danser().to_owned();
    default_path.push("settings/default.json");
    let bytes = tokio::fs::read(&default_path)
        .await
        .context("failed to read default.json")?;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).context("failed to parse default.json")?;

    let pointer = join_pointer("", &section);
    let components = match json.pointer(&pointer) {
        Some(serde_json::Value::Object(map)) => build_object_select_rows(map, &pointer),
        _ => Vec::new(),
    };

    if components.is_empty() {
        let builder = MessageBuilder::new()
            .content("No properties found in this block.")
            .components(vec![]);
        component.callback(&ctx, builder).await?;
        return Ok(());
    }

    let builder = MessageBuilder::new()
        .content(format!("Editing block: **{section}**"))
        .components(components);

    component.callback(&ctx, builder).await?;
    Ok(())
}

pub async fn handle_owner_settings_prop(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let user_id = component.user_id()?;
    if !BotConfig::get().owners.contains(&user_id) {
        component
            .callback(&ctx, MessageBuilder::new().content(NOT_OWNER))
            .await?;
        return Ok(());
    }

    let custom_id = &component.data.custom_id;
    let Some(rest) = custom_id.strip_prefix("owner_settings_prop_") else {
        return Ok(());
    };
    let Some((_, parent_pointer)) = rest.split_once(':') else {
        return Ok(());
    };

    let prop = component.data.values.first().cloned().unwrap_or_default();
    if prop.is_empty() {
        return Ok(());
    }

    let target_pointer = join_pointer(parent_pointer, &prop);
    let display_path = format_pointer_display(&target_pointer);
    tracing::info!("handle_owner_settings_prop: path='{display_path}'");

    let mut default_path = BotConfig::get().paths.danser().to_owned();
    default_path.push("settings/default.json");
    let bytes = tokio::fs::read(&default_path)
        .await
        .context("failed to read default.json")?;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).context("failed to parse default.json")?;

    let Some(val) = json.pointer(&target_pointer) else {
        let builder = MessageBuilder::new()
            .content(format!("Setting `{display_path}` was not found in `default.json`."))
            .components(vec![]);
        component.callback(&ctx, builder).await?;
        return Ok(());
    };

    if let serde_json::Value::Object(map) = val {
        let components = build_object_select_rows(map, &target_pointer);
        if components.is_empty() {
            let too_long = format!("owner_settings_prop_0:{target_pointer}").len() > 100;
            let msg = if too_long {
                format!("Setting path `{display_path}` is too long for Discord component custom IDs.")
            } else {
                format!("Block `{display_path}` has no properties.")
            };
            let builder = MessageBuilder::new().content(msg).components(vec![]);
            component.callback(&ctx, builder).await?;
            return Ok(());
        }

        let builder = MessageBuilder::new()
            .content(format!("Editing nested block: **{display_path}**"))
            .components(components);
        component.callback(&ctx, builder).await?;
        return Ok(());
    }

    let modal_id = format!("oset_modal_{target_pointer}");
    if modal_id.len() > 100 || format!("oset_applyall_{target_pointer}").len() > 100 {
        let builder = MessageBuilder::new()
            .content(format!(
                "Setting path `{display_path}` is too long for Discord component custom IDs."
            ))
            .components(vec![]);
        component.callback(&ctx, builder).await?;
        return Ok(());
    }

    let raw_val = match val {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };

    let leaf = pointer_leaf_name(&target_pointer);
    let label: String = format!("Value for {leaf}").chars().take(45).collect();
    let title: String = format!("Edit {leaf}").chars().take(45).collect();
    let placeholder: String = raw_val.chars().take(100).collect();

    let mut modal = ModalBuilder::new("settings_value", label)
        .modal_id(modal_id)
        .placeholder(placeholder)
        .title(title);

    if raw_val.len() <= 4000 {
        modal = modal.value(raw_val);
    }

    component.modal(&ctx, modal).await?;
    Ok(())
}

pub async fn handle_owner_settings_modal(
    ctx: Arc<Context>,
    modal: InteractionModal,
) -> Result<()> {
    let user_id = modal.user_id()?;
    if !BotConfig::get().owners.contains(&user_id) {
        modal
            .callback(&ctx, MessageBuilder::new().content(NOT_OWNER))
            .await?;
        return Ok(());
    }

    let custom_id = &modal.data.custom_id;
    let Some(pointer) = custom_id.strip_prefix("oset_modal_") else {
        return Ok(());
    };
    if !pointer.starts_with('/') {
        return Ok(());
    }

    modal
        .defer_ephemeral(&ctx)
        .await
        .context("failed to defer modal")?;

    let new_value_str = modal
        .data
        .components
        .first()
        .and_then(|component| component.components.first())
        .and_then(|input| input.value.clone())
        .unwrap_or_default();

    let mut default_path = BotConfig::get().paths.danser().to_owned();
    default_path.push("settings/default.json");
    let bytes = tokio::fs::read(&default_path)
        .await
        .context("failed to read default.json")?;
    let mut json: serde_json::Value =
        serde_json::from_slice(&bytes).context("failed to parse default.json")?;

    let display_path = format_pointer_display(pointer);

    let Some(prop_val) = json.pointer_mut(pointer) else {
        let builder = MessageBuilder::new()
            .content(format!("Setting `{display_path}` was not found in `default.json`."))
            .components(vec![]);
        modal.update_response(&ctx, &builder).await?;
        return Ok(());
    };

    let parsed_value = match prop_val {
        serde_json::Value::Bool(_) => {
            let trimmed = new_value_str.trim();
            if trimmed.eq_ignore_ascii_case("true") || trimmed == "1" {
                Ok(serde_json::Value::Bool(true))
            } else if trimmed.eq_ignore_ascii_case("false") || trimmed == "0" {
                Ok(serde_json::Value::Bool(false))
            } else {
                Err("expected a boolean (`true`/`false` or `1`/`0`)".to_owned())
            }
        }
        serde_json::Value::Number(_) => {
            let trimmed = new_value_str.trim();
            if let Ok(i) = trimmed.parse::<i64>() {
                Ok(serde_json::Value::Number(i.into()))
            } else if let Ok(u) = trimmed.parse::<u64>() {
                Ok(serde_json::Value::Number(u.into()))
            } else if let Ok(f) = trimmed.parse::<f64>() {
                if f.is_finite() {
                    match serde_json::Number::from_f64(f) {
                        Some(n) => Ok(serde_json::Value::Number(n)),
                        None => Err("number cannot be represented in JSON".to_owned()),
                    }
                } else {
                    Err("expected a finite floating-point number".to_owned())
                }
            } else {
                Err("expected a valid number".to_owned())
            }
        }
        serde_json::Value::Array(_) => match serde_json::from_str::<serde_json::Value>(&new_value_str)
        {
            Ok(v) if v.is_array() => Ok(v),
            _ => Err("expected a valid JSON array (e.g. `[\"item1\", \"item2\"]`)".to_owned()),
        },
        serde_json::Value::Object(_) => Err("cannot overwrite a nested settings object directly".to_owned()),
        serde_json::Value::String(_) | serde_json::Value::Null => {
            Ok(serde_json::Value::String(new_value_str.clone()))
        }
    };

    match parsed_value {
        Ok(val) => *prop_val = val,
        Err(reason) => {
            tracing::warn!("Invalid value '{new_value_str}' for {display_path}: {reason}");
            let builder = MessageBuilder::new()
                .content(format!(
                    "Invalid value `{new_value_str}` for `{display_path}` ({reason}). Nothing was saved."
                ))
                .components(vec![]);
            modal.update_response(&ctx, &builder).await?;
            return Ok(());
        }
    }

    if let Err(err) = serde_json::from_value::<crate::core::settings::DanserSettings>(json.clone()) {
        tracing::warn!("Schema validation failed for {display_path}={new_value_str}: {err}");
        let builder = MessageBuilder::new()
            .content(format!(
                "Value `{new_value_str}` does not match the expected type for `{display_path}` ({err}). Nothing was saved."
            ))
            .components(vec![]);
        modal.update_response(&ctx, &builder).await?;
        return Ok(());
    }

    let serialized = serde_json::to_vec_pretty(&json).context("failed to serialize default.json")?;
    write_json_atomic(&default_path, &serialized).await?;

    let apply_btn = Button {
        sku_id: None,
        custom_id: Some(format!("oset_applyall_{pointer}")),
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
        .content(format!(
            "Successfully updated `{display_path}` to `{new_value_str}` in `default.json`.\n\nShould this be applied to all user configs that already exist?"
        ))
        .components(components);
    modal.update_response(&ctx, &builder).await?;

    Ok(())
}

pub async fn handle_owner_settings_skipall(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let user_id = component.user_id()?;
    if !BotConfig::get().owners.contains(&user_id) {
        component
            .callback(&ctx, MessageBuilder::new().content(NOT_OWNER))
            .await?;
        return Ok(());
    }

    let builder = MessageBuilder::new()
        .content("Settings change applied only to the default config.")
        .components(vec![]);
    component.callback(&ctx, builder).await?;
    Ok(())
}

pub async fn handle_owner_settings_applyall(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let user_id = component.user_id()?;
    if !BotConfig::get().owners.contains(&user_id) {
        component
            .callback(&ctx, MessageBuilder::new().content(NOT_OWNER))
            .await?;
        return Ok(());
    }

    let Some(pointer) = component.data.custom_id.strip_prefix("oset_applyall_") else {
        return Ok(());
    };
    if !pointer.starts_with('/') {
        return Ok(());
    }

    component.defer(&ctx).await.context("failed to defer applyall")?;

    let mut default_path = BotConfig::get().paths.danser().to_owned();
    default_path.push("settings/default.json");
    let default_bytes = tokio::fs::read(&default_path)
        .await
        .context("failed to read default.json")?;
    let default_json: serde_json::Value =
        serde_json::from_slice(&default_bytes).context("failed to parse default.json")?;

    let display_path = format_pointer_display(pointer);
    let Some(default_val) = default_json.pointer(pointer).cloned() else {
        let builder = MessageBuilder::new()
            .content("Failed to find the setting in default.json!")
            .components(vec![]);
        component.update_response(&ctx, &builder).await?;
        return Ok(());
    };

    let mut settings_dir = BotConfig::get().paths.danser().to_owned();
    settings_dir.push("settings");

    let mut applied_count = 0usize;
    let mut failed_count = 0usize;

    let mut dir = tokio::fs::read_dir(&settings_dir).await?;
    while let Ok(Some(entry)) = dir.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let Some(stem) = path.file_stem().and_then(|n| n.to_str()) else {
            continue;
        };
        if stem.parse::<u64>().is_err() {
            continue;
        }

        let Ok(bytes) = tokio::fs::read(&path).await else {
            failed_count += 1;
            continue;
        };
        let Ok(mut json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            failed_count += 1;
            continue;
        };

        if let Some(prop_val) = json.pointer_mut(pointer) {
            *prop_val = default_val.clone();
            match serde_json::to_vec_pretty(&json) {
                Ok(out) => {
                    if write_json_atomic(&path, &out).await.is_ok() {
                        applied_count += 1;
                    } else {
                        failed_count += 1;
                    }
                }
                Err(_) => failed_count += 1,
            }
        }
    }

    let msg = if failed_count > 0 {
        format!(
            "Applied `{display_path}` to {applied_count} user configs ({failed_count} failed)."
        )
    } else {
        format!("Successfully applied `{display_path}` to {applied_count} user configs.")
    };

    let builder = MessageBuilder::new().content(msg).components(vec![]);
    component.update_response(&ctx, &builder).await?;

    Ok(())
}
