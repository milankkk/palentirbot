use std::sync::Arc;

use eyre::{Context as _, Result};
use twilight_model::{
    channel::message::component::{ButtonStyle, Component, SelectMenuOption},
    id::{marker::UserMarker, Id},
    user::User,
};

use crate::{
    core::{settings::DanserSettings, Context},
    util::{
        builder::{action_row, button, text_select_row, MessageBuilder, ModalBuilder},
        interaction::{InteractionCommand, InteractionComponent, InteractionModal},
        Authored, ComponentExt, InteractionCommandExt, ModalExt,
    },
};

use super::{
    create_settings_embed, current_skin_basename, default_settings_path,
    edit::{load_or_init_user_settings, resolve_skin_name, save_user_settings},
    preset::apply_preset,
    rewrite_current_skin_path, user_settings_path, PresetChoice,
};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum SettingsTab {
    Visuals,
    Hud,
    Audio,
    Presets,
}

impl SettingsTab {
    fn as_str(self) -> &'static str {
        match self {
            Self::Visuals => "visuals",
            Self::Hud => "hud",
            Self::Audio => "audio",
            Self::Presets => "presets",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "visuals" => Some(Self::Visuals),
            "hud" => Some(Self::Hud),
            "audio" => Some(Self::Audio),
            "presets" => Some(Self::Presets),
            _ => None,
        }
    }
}

async fn is_discord_linked(ctx: &Context, uid: Id<UserMarker>) -> bool {
    ctx.psql()
        .get_linked_osu_user(uid)
        .await
        .ok()
        .flatten()
        .is_some()
}

pub async fn menu(ctx: Arc<Context>, command: InteractionCommand) -> Result<()> {
    let user = command.user()?.clone();
    let uid = user.id;
    let (_, settings) = load_or_init_user_settings(uid).await?;
    let is_linked = is_discord_linked(&ctx, uid).await;
    let builder = build_menu_message(
        &ctx,
        &user,
        &settings,
        is_linked,
        SettingsTab::Visuals,
        Some("⚙️ Use the menu below to customize your render settings. Only you can see this."),
    );
    command.callback(&ctx, builder, true).await?;
    crate::core::events::interaction::spawn_interaction_component_expiry(
        Arc::clone(&ctx),
        command.token,
    );
    Ok(())
}

fn build_menu_message<'a>(
    ctx: &Context,
    user: &User,
    settings: &DanserSettings,
    is_linked: bool,
    tab: SettingsTab,
    notice: Option<&str>,
) -> MessageBuilder<'a> {
    let embed = create_settings_embed(user, settings, is_linked);
    let components = build_menu_components(ctx, user.id, settings, is_linked, tab);
    let mut builder = MessageBuilder::new().embed(embed).components(components);
    if let Some(msg) = notice {
        builder = builder.content(msg.to_owned());
    }
    builder
}

fn toggle_btn(custom_id: String, label: &str, enabled: bool) -> Component {
    let style = if enabled {
        ButtonStyle::Success
    } else {
        ButtonStyle::Secondary
    };
    button(custom_id, label, style)
}

fn build_menu_components(
    ctx: &Context,
    uid: Id<UserMarker>,
    settings: &DanserSettings,
    is_linked: bool,
    tab: SettingsTab,
) -> Vec<Component> {
    let mut rows = Vec::with_capacity(4);

    // Row 1: Category selector
    let tab_options = vec![
        SelectMenuOption {
            default: tab == SettingsTab::Visuals,
            description: Some("Skin, background dim, storyboard, video, blur, cursor".to_owned()),
            emoji: None,
            label: "🎨 Skin & Visuals".to_owned(),
            value: "visuals".to_owned(),
        },
        SelectMenuOption {
            default: tab == SettingsTab::Hud,
            description: Some("PP counter, hit/aim error, leaderboard, key overlay".to_owned()),
            emoji: None,
            label: "📊 HUD & Overlays".to_owned(),
            value: "hud".to_owned(),
        },
        SelectMenuOption {
            default: tab == SettingsTab::Audio,
            description: Some("Music/hitsound volume, map hitsounds, intro, results".to_owned()),
            emoji: None,
            label: "🔊 Audio & Playback".to_owned(),
            value: "audio".to_owned(),
        },
        SelectMenuOption {
            default: tab == SettingsTab::Presets,
            description: Some("One-click render profiles or reset to defaults".to_owned()),
            emoji: None,
            label: "⚡ Presets & Reset".to_owned(),
            value: "presets".to_owned(),
        },
    ];
    rows.push(text_select_row(
        format!("uset_tab:{uid}"),
        "Switch settings category",
        tab_options,
    ));

    let t = tab.as_str();
    match tab {
        SettingsTab::Visuals => {
            if let Some(skin_row) = build_skin_select_row(ctx, uid, settings) {
                rows.push(skin_row);
            }
            rows.push(action_row(vec![
                toggle_btn(
                    format!("uset_tog:{uid}:{t}:storyboard"),
                    "Storyboard",
                    settings.playfield.background.load_storyboards,
                ),
                toggle_btn(
                    format!("uset_tog:{uid}:{t}:video"),
                    "BG Video",
                    settings.playfield.background.load_videos,
                ),
                toggle_btn(
                    format!("uset_tog:{uid}:{t}:motion_blur"),
                    "Motion Blur",
                    settings.recording.motion_blur.enabled,
                ),
                toggle_btn(
                    format!("uset_tog:{uid}:{t}:cursor_ripples"),
                    "Cursor Ripples",
                    settings.cursor.cursor_ripples,
                ),
            ]));
            let eff_q = settings
                .current_render_quality()
                .cap_for_entitlement(is_linked);
            let is_hd_plus = !matches!(eff_q, crate::core::settings::RenderQuality::Hd720p60);
            let quality_label = match eff_q {
                crate::core::settings::RenderQuality::FullHd1080p60 => "Quality: 1080p60",
                crate::core::settings::RenderQuality::HdPlus900p60 => "Quality: 900p60",
                crate::core::settings::RenderQuality::Hd720p60 if is_linked => "Quality: 720p60",
                crate::core::settings::RenderQuality::Hd720p60 => "Quality: 720p60 (🔒 900p/1080p)",
            };
            rows.push(action_row(vec![
                toggle_btn(format!("uset_tog:{uid}:{t}:quality"), quality_label, is_hd_plus),
                button(
                    format!("uset_modal:{uid}:visuals"),
                    "🎚️ BG Dim & Cursor Scale",
                    ButtonStyle::Primary,
                ),
                button(format!("uset_done:{uid}"), "✅ Done", ButtonStyle::Secondary),
            ]));
        }
        SettingsTab::Hud => {
            rows.push(action_row(vec![
                toggle_btn(
                    format!("uset_tog:{uid}:{t}:pp"),
                    "PP Counter",
                    settings.gameplay.pp_counter.show,
                ),
                toggle_btn(
                    format!("uset_tog:{uid}:{t}:leaderboard"),
                    "Leaderboard",
                    settings.gameplay.score_board.show,
                ),
                toggle_btn(
                    format!("uset_tog:{uid}:{t}:key_overlay"),
                    "Key Overlay",
                    settings.gameplay.key_overlay.show,
                ),
                toggle_btn(
                    format!("uset_tog:{uid}:{t}:hit_counter"),
                    "Hit Counter",
                    settings.gameplay.hit_counter.show,
                ),
                toggle_btn(
                    format!("uset_tog:{uid}:{t}:sliderbreaks"),
                    "Sliderbreaks",
                    settings.gameplay.hit_counter.show_sliderbreaks,
                ),
            ]));
            rows.push(action_row(vec![
                toggle_btn(
                    format!("uset_tog:{uid}:{t}:strain_graph"),
                    "Strain Graph",
                    settings.gameplay.strain_graph.show,
                ),
                toggle_btn(
                    format!("uset_tog:{uid}:{t}:strain_outline"),
                    "Strain Outline",
                    settings.gameplay.strain_graph.outline.show,
                ),
                toggle_btn(
                    format!("uset_tog:{uid}:{t}:hit_error"),
                    "Hit Error",
                    settings.gameplay.hit_error_meter.show,
                ),
                toggle_btn(
                    format!("uset_tog:{uid}:{t}:aim_error"),
                    "Aim Error",
                    settings.gameplay.aim_error_meter.show,
                ),
            ]));
            rows.push(action_row(vec![
                button(
                    format!("uset_modal:{uid}:hud"),
                    "🔢 Counter Decimals",
                    ButtonStyle::Primary,
                ),
                button(format!("uset_done:{uid}"), "✅ Done", ButtonStyle::Secondary),
            ]));
        }
        SettingsTab::Audio => {
            rows.push(action_row(vec![
                toggle_btn(
                    format!("uset_tog:{uid}:{t}:beatmap_hitsounds"),
                    "Map Hitsounds",
                    !settings.audio.ignore_beatmap_samples,
                ),
                toggle_btn(
                    format!("uset_tog:{uid}:{t}:skip_intro"),
                    "Skip Intro",
                    settings.gameplay.always_skip_intro,
                ),
                toggle_btn(
                    format!("uset_tog:{uid}:{t}:results_screen"),
                    "Results Screen",
                    settings.gameplay.show_results_screen,
                ),
            ]));
            rows.push(action_row(vec![
                button(
                    format!("uset_modal:{uid}:audio"),
                    "🔊 Music & Hitsound Volume",
                    ButtonStyle::Primary,
                ),
                button(format!("uset_done:{uid}"), "✅ Done", ButtonStyle::Secondary),
            ]));
        }
        SettingsTab::Presets => {
            rows.push(action_row(vec![
                button(
                    format!("uset_preset:{uid}:standard"),
                    "Default Profile",
                    ButtonStyle::Primary,
                ),
                button(
                    format!("uset_preset:{uid}:clean"),
                    "Clean Showcase",
                    ButtonStyle::Primary,
                ),
                button(
                    format!("uset_preset:{uid}:fast720p"),
                    "Fast Render",
                    ButtonStyle::Primary,
                ),
                button(
                    format!("uset_preset:{uid}:blur"),
                    "Cinematic Blur",
                    ButtonStyle::Primary,
                ),
            ]));
            rows.push(action_row(vec![
                button(
                    format!("uset_reset:{uid}"),
                    "♻️ Reset to Default",
                    ButtonStyle::Danger,
                ),
                button(format!("uset_done:{uid}"), "✅ Done", ButtonStyle::Secondary),
            ]));
        }
    }

    rows
}

fn build_skin_select_row(
    ctx: &Context,
    uid: Id<UserMarker>,
    settings: &DanserSettings,
) -> Option<Component> {
    let current_raw = current_skin_basename(settings);

    let mut skin_list = ctx.skin_list();
    let skins = skin_list.get().ok()?;
    let mut names: Vec<String> = skins
        .iter()
        .filter_map(|os| {
            let s = os.to_string_lossy().into_owned();
            (s.len() <= 100).then_some(s)
        })
        .collect();
    if names.is_empty() {
        return None;
    }

    if names.len() > 25 {
        if let Some(pos) = names.iter().position(|n| n.eq_ignore_ascii_case(&current_raw)) {
            if pos >= 25 {
                let current_entry = names.remove(pos);
                names.truncate(24);
                names.insert(0, current_entry);
            } else {
                names.truncate(25);
            }
        } else {
            names.truncate(25);
        }
    }

    let options: Vec<SelectMenuOption> = names
        .into_iter()
        .map(|raw| {
            let is_current = !current_raw.is_empty() && raw.eq_ignore_ascii_case(&current_raw);
            let display: String = super::format_skin_display_name(&raw).chars().take(100).collect();
            SelectMenuOption {
                default: is_current,
                description: is_current.then(|| "Currently selected".to_owned()),
                emoji: None,
                label: display,
                value: raw,
            }
        })
        .collect();

    Some(text_select_row(
        format!("uset_skin:{uid}"),
        "Choose your skin",
        options,
    ))
}

fn parse_uid_and_rest<'a>(custom_id: &'a str, prefix: &str) -> Option<(Id<UserMarker>, &'a str)> {
    let after = custom_id.strip_prefix(prefix)?;
    match after.split_once(':') {
        Some((uid_str, rest)) => Some((Id::new(uid_str.parse::<u64>().ok()?), rest)),
        None => Some((Id::new(after.parse::<u64>().ok()?), "")),
    }
}

async fn authorize_settings_component<'a>(
    ctx: &Context,
    component: &'a InteractionComponent,
    prefix: &str,
) -> Result<Option<(Id<UserMarker>, &'a str)>> {
    let Some((owner_uid, rest)) = parse_uid_and_rest(&component.data.custom_id, prefix) else {
        return Ok(None);
    };
    if component.user_id()? == owner_uid {
        return Ok(Some((owner_uid, rest)));
    }
    component
        .ephemeral_text(
            ctx,
            "This settings menu belongs to another user. Run `/settings menu` to edit yours.",
        )
        .await?;
    Ok(None)
}

async fn ensure_owner_modal(
    ctx: &Context,
    modal: &InteractionModal,
    owner_uid: Id<UserMarker>,
) -> Result<bool> {
    if modal.user_id()? == owner_uid {
        return Ok(true);
    }
    modal
        .callback(
            ctx,
            MessageBuilder::new().content(
                "This settings menu belongs to another user. Run `/settings menu` to edit yours.",
            ),
        )
        .await?;
    Ok(false)
}

pub async fn handle_user_settings_open(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let Some((owner_uid, _)) =
        authorize_settings_component(&ctx, &component, "uset_open:").await?
    else {
        return Ok(());
    };
    let user = component.user()?.clone();
    let (_, settings) = load_or_init_user_settings(owner_uid).await?;
    let is_linked = is_discord_linked(&ctx, owner_uid).await;
    let builder = build_menu_message(
        &ctx,
        &user,
        &settings,
        is_linked,
        SettingsTab::Visuals,
        Some("⚙️ Use the menu below to customize your render settings. Only you can see this."),
    );
    component.ephemeral_callback(&ctx, builder).await?;
    crate::core::events::interaction::spawn_interaction_component_expiry(
        Arc::clone(&ctx),
        component.token,
    );
    Ok(())
}

pub async fn handle_user_settings_tab(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let Some((owner_uid, _)) = authorize_settings_component(&ctx, &component, "uset_tab:").await?
    else {
        return Ok(());
    };
    let Some(tab) = component
        .data
        .values
        .first()
        .and_then(|s| SettingsTab::from_str(s))
    else {
        return Ok(());
    };
    let user = component.user()?.clone();
    let (_, settings) = load_or_init_user_settings(owner_uid).await?;
    let is_linked = is_discord_linked(&ctx, owner_uid).await;
    let builder = build_menu_message(&ctx, &user, &settings, is_linked, tab, None);
    component.callback(&ctx, builder).await?;
    Ok(())
}

pub async fn handle_user_settings_skin(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let Some((owner_uid, _)) =
        authorize_settings_component(&ctx, &component, "uset_skin:").await?
    else {
        return Ok(());
    };
    let Some(chosen) = component.data.values.first() else {
        return Ok(());
    };
    let is_linked = is_discord_linked(&ctx, owner_uid).await;
    let Some(skin_name) = resolve_skin_name(&ctx, chosen)? else {
        let user = component.user()?.clone();
        let (_, settings) = load_or_init_user_settings(owner_uid).await?;
        let builder = build_menu_message(
            &ctx,
            &user,
            &settings,
            is_linked,
            SettingsTab::Visuals,
            Some("❌ Skin not found on disk."),
        );
        component.callback(&ctx, builder).await?;
        return Ok(());
    };

    let user = component.user()?.clone();
    let (user_path, mut settings) = load_or_init_user_settings(owner_uid).await?;
    rewrite_current_skin_path(&mut settings, &skin_name);
    save_user_settings(&user_path, &settings).await?;

    let display = super::format_skin_display_name(&skin_name);
    let notice = format!("✅ Skin set to `{display}`");
    let builder = build_menu_message(
        &ctx,
        &user,
        &settings,
        is_linked,
        SettingsTab::Visuals,
        Some(&notice),
    );
    component.callback(&ctx, builder).await?;
    Ok(())
}

pub async fn handle_user_settings_toggle(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let Some((owner_uid, rest)) =
        authorize_settings_component(&ctx, &component, "uset_tog:").await?
    else {
        return Ok(());
    };
    let Some((tab_str, key)) = rest.split_once(':') else {
        return Ok(());
    };
    let Some(tab) = SettingsTab::from_str(tab_str) else {
        return Ok(());
    };

    let user = component.user()?.clone();
    let (user_path, mut settings) = load_or_init_user_settings(owner_uid).await?;
    let is_linked = is_discord_linked(&ctx, owner_uid).await;
    let notice = if key == "quality" {
        if !is_linked {
            settings.apply_render_quality(crate::core::settings::RenderQuality::Hd720p60);
            "🔒 Link your osu! account with `/link` to unlock **900p60** & **1080p60** rendering!".to_owned()
        } else {
            let next = match settings.current_render_quality() {
                crate::core::settings::RenderQuality::Hd720p60 => {
                    crate::core::settings::RenderQuality::HdPlus900p60
                }
                crate::core::settings::RenderQuality::HdPlus900p60 => {
                    crate::core::settings::RenderQuality::FullHd1080p60
                }
                crate::core::settings::RenderQuality::FullHd1080p60 => {
                    crate::core::settings::RenderQuality::Hd720p60
                }
            };
            settings.apply_render_quality(next);
            save_user_settings(&user_path, &settings).await?;
            if matches!(next, crate::core::settings::RenderQuality::FullHd1080p60) {
                format!(
                    "✅ Render quality set to **{}** — ⚠️ takes noticeably longer to render.",
                    next.display_label()
                )
            } else {
                format!("✅ Render quality set to **{}**", next.display_label())
            }
        }
    } else {
        let Some(notice) = apply_toggle(&mut settings, key) else {
            return Ok(());
        };
        save_user_settings(&user_path, &settings).await?;
        notice
    };

    let builder = build_menu_message(&ctx, &user, &settings, is_linked, tab, Some(&notice));
    component.callback(&ctx, builder).await?;
    Ok(())
}

fn toggle_flag(field: &mut bool, label: &str, is_visibility: bool) -> Option<String> {
    *field = !*field;
    let word = match (*field, is_visibility) {
        (true, false) => "enabled",
        (false, false) => "disabled",
        (true, true) => "shown",
        (false, true) => "hidden",
    };
    Some(format!("✅ {label} {word}"))
}

fn apply_toggle(settings: &mut DanserSettings, key: &str) -> Option<String> {
    match key {
        "storyboard" => toggle_flag(&mut settings.playfield.background.load_storyboards, "Storyboard", false),
        "video" => toggle_flag(&mut settings.playfield.background.load_videos, "BG Video", false),
        "motion_blur" => toggle_flag(&mut settings.recording.motion_blur.enabled, "Motion Blur", false),
        "cursor_ripples" => toggle_flag(&mut settings.cursor.cursor_ripples, "Cursor Ripples", true),
        "pp" => toggle_flag(&mut settings.gameplay.pp_counter.show, "PP Counter", true),
        "leaderboard" => toggle_flag(&mut settings.gameplay.score_board.show, "Leaderboard", true),
        "key_overlay" => toggle_flag(&mut settings.gameplay.key_overlay.show, "Key Overlay", true),
        "hit_counter" => toggle_flag(&mut settings.gameplay.hit_counter.show, "Hit Counter", true),
        "sliderbreaks" => toggle_flag(&mut settings.gameplay.hit_counter.show_sliderbreaks, "Sliderbreaks", true),
        "strain_graph" => toggle_flag(&mut settings.gameplay.strain_graph.show, "Strain Graph", true),
        "strain_outline" => toggle_flag(&mut settings.gameplay.strain_graph.outline.show, "Strain Graph Outline", true),
        "hit_error" => toggle_flag(&mut settings.gameplay.hit_error_meter.show, "Hit Error Meter", true),
        "aim_error" => toggle_flag(&mut settings.gameplay.aim_error_meter.show, "Aim Error Meter", true),
        "beatmap_hitsounds" => {
            settings.audio.ignore_beatmap_samples = !settings.audio.ignore_beatmap_samples;
            let state = if !settings.audio.ignore_beatmap_samples {
                "enabled"
            } else {
                "disabled"
            };
            Some(format!("✅ Beatmap Hitsounds {state}"))
        }
        "skip_intro" => toggle_flag(&mut settings.gameplay.always_skip_intro, "Skip Intro", false),
        "results_screen" => toggle_flag(&mut settings.gameplay.show_results_screen, "Results Screen", false),
        _ => None,
    }
}

pub async fn handle_user_settings_open_modal(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let Some((owner_uid, tab_str)) =
        authorize_settings_component(&ctx, &component, "uset_modal:").await?
    else {
        return Ok(());
    };
    let Some(tab) = SettingsTab::from_str(tab_str) else {
        return Ok(());
    };
    let (_, settings) = load_or_init_user_settings(owner_uid).await?;
    let modal_id = format!("uset_m:{owner_uid}:{}", tab.as_str());

    let modal = match tab {
        SettingsTab::Visuals => ModalBuilder::new("dim", "Background Dim (0 - 100%)")
            .modal_id(modal_id)
            .title("Visual Settings")
            .placeholder("85")
            .max_len(3)
            .value(
                ((settings.playfield.background.dim.normal * 100.0).round() as i64)
                    .clamp(0, 100)
                    .to_string(),
            )
            .add_input("cursor_scale", "Cursor Scale (0.1 - 2.0)")
            .placeholder("1.0")
            .max_len(6)
            .value(format!("{:.2}", settings.skin.cursor.scale)),
        SettingsTab::Hud => ModalBuilder::new("pp_decimals", "PP Counter Decimals (0 - 3)")
            .modal_id(modal_id)
            .title("Counter Precision")
            .placeholder("2")
            .max_len(1)
            .value(settings.gameplay.pp_counter.decimals.to_string())
            .add_input("hit_error_decimals", "Hit Error UR Decimals (0 - 3)")
            .placeholder("2")
            .max_len(1)
            .value(
                settings
                    .gameplay
                    .hit_error_meter
                    .unstable_rate_decimals
                    .to_string(),
            )
            .add_input("aim_error_decimals", "Aim Error UR Decimals (0 - 3)")
            .placeholder("2")
            .max_len(1)
            .value(
                settings
                    .gameplay
                    .aim_error_meter
                    .unstable_rate_decimals
                    .to_string(),
            ),
        SettingsTab::Audio => ModalBuilder::new("music_volume", "Music Volume (0 - 100%)")
            .modal_id(modal_id)
            .title("Audio Volumes")
            .placeholder("100")
            .max_len(3)
            .value(
                ((settings.audio.music_volume * 100.0).round() as i64)
                    .clamp(0, 100)
                    .to_string(),
            )
            .add_input("hitsound_volume", "Hitsound Volume (0 - 100%)")
            .placeholder("100")
            .max_len(3)
            .value(
                ((settings.audio.sample_volume * 100.0).round() as i64)
                    .clamp(0, 100)
                    .to_string(),
            ),
        SettingsTab::Presets => return Ok(()),
    };

    component.modal(&ctx, modal).await?;
    Ok(())
}

pub(crate) fn parse_and_apply_menu_modal(
    tab: SettingsTab,
    fields: &[(&str, &str)],
    settings: &mut DanserSettings,
) -> std::result::Result<String, String> {
    let find = |name: &str| {
        fields
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.trim())
            .unwrap_or("")
    };
    let parse_pct = |key: &str, label: &str| -> std::result::Result<u8, String> {
        find(key)
            .parse::<i64>()
            .ok()
            .filter(|&v| (0..=100).contains(&v))
            .and_then(|v| u8::try_from(v).ok())
            .ok_or_else(|| format!("{label} must be an integer between 0 and 100."))
    };

    match tab {
        SettingsTab::Visuals => {
            let dim = parse_pct("dim", "Background Dim")?;
            let cursor_scale: f64 = find("cursor_scale")
                .replace(',', ".")
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && (0.1..=2.0).contains(v))
                .ok_or_else(|| "Cursor Scale must be a number between 0.1 and 2.0.".to_owned())?;

            settings.playfield.background.dim.normal = f64::from(dim) / 100.0;
            settings.skin.cursor.scale = cursor_scale;
            Ok(format!(
                "✅ Updated BG Dim (`{dim}%`) and Cursor Scale (`{cursor_scale:.2}x`)"
            ))
        }
        SettingsTab::Hud => {
            let parse_dec = |key: &str, label: &str| -> std::result::Result<u32, String> {
                find(key)
                    .parse::<u32>()
                    .ok()
                    .filter(|&v| v <= 3)
                    .ok_or_else(|| format!("{label} must be between 0 and 3."))
            };
            let pp_dec = parse_dec("pp_decimals", "PP Decimals")?;
            let hit_dec = parse_dec("hit_error_decimals", "Hit Error Decimals")?;
            let aim_dec = parse_dec("aim_error_decimals", "Aim Error Decimals")?;

            settings.gameplay.pp_counter.decimals = pp_dec;
            settings.gameplay.hit_error_meter.unstable_rate_decimals = hit_dec;
            settings.gameplay.aim_error_meter.unstable_rate_decimals = aim_dec;
            Ok(format!(
                "✅ Updated decimals — PP: `{pp_dec}`, Hit Error: `{hit_dec}`, Aim Error: `{aim_dec}`"
            ))
        }
        SettingsTab::Audio => {
            let music = parse_pct("music_volume", "Music Volume")?;
            let hitsounds = parse_pct("hitsound_volume", "Hitsound Volume")?;

            settings.audio.music_volume = f64::from(music) / 100.0;
            settings.audio.sample_volume = f64::from(hitsounds) / 100.0;
            Ok(format!(
                "✅ Updated Music Volume (`{music}%`) and Hitsound Volume (`{hitsounds}%`)"
            ))
        }
        SettingsTab::Presets => Err("No modal fields exist for the Presets tab.".to_owned()),
    }
}

pub async fn handle_user_settings_modal(
    ctx: Arc<Context>,
    modal: InteractionModal,
) -> Result<()> {
    let Some((owner_uid, tab_str)) = parse_uid_and_rest(&modal.data.custom_id, "uset_m:") else {
        return Ok(());
    };
    if !ensure_owner_modal(&ctx, &modal, owner_uid).await? {
        return Ok(());
    }
    let Some(tab) = SettingsTab::from_str(tab_str) else {
        return Ok(());
    };

    let pair_refs: Vec<(&str, &str)> = modal
        .data
        .components
        .iter()
        .flat_map(|row| row.components.iter())
        .map(|inp| (inp.custom_id.as_str(), inp.value.as_deref().unwrap_or("")))
        .collect();

    let (user_path, mut settings) = load_or_init_user_settings(owner_uid).await?;
    let notice = match parse_and_apply_menu_modal(tab, &pair_refs, &mut settings) {
        Ok(msg) => msg,
        Err(reason) => {
            modal
                .callback(&ctx, MessageBuilder::new().content(format!("❌ {reason}")))
                .await?;
            return Ok(());
        }
    };

    save_user_settings(&user_path, &settings).await?;
    let user = modal.user()?.clone();
    let is_linked = is_discord_linked(&ctx, owner_uid).await;
    let builder = build_menu_message(&ctx, &user, &settings, is_linked, tab, Some(&notice));
    modal.update_callback(&ctx, builder).await?;
    Ok(())
}

pub async fn handle_user_settings_preset(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let Some((owner_uid, preset_key)) =
        authorize_settings_component(&ctx, &component, "uset_preset:").await?
    else {
        return Ok(());
    };

    let choice = match preset_key {
        "standard" => PresetChoice::Standard,
        "clean" => PresetChoice::CleanRecord,
        "fast720p" => PresetChoice::Performance720p,
        "blur" => PresetChoice::CinematicBlur,
        _ => return Ok(()),
    };

    let user = component.user()?.clone();
    let (user_path, mut settings) = load_or_init_user_settings(owner_uid).await?;
    let is_linked = is_discord_linked(&ctx, owner_uid).await;
    let label = apply_preset(&mut settings, choice, is_linked);
    save_user_settings(&user_path, &settings).await?;

    let notice = format!("✅ Applied preset **{label}**");
    let builder = build_menu_message(
        &ctx,
        &user,
        &settings,
        is_linked,
        SettingsTab::Presets,
        Some(&notice),
    );
    component.callback(&ctx, builder).await?;
    Ok(())
}

pub async fn handle_user_settings_reset(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let Some((owner_uid, _)) =
        authorize_settings_component(&ctx, &component, "uset_reset:").await?
    else {
        return Ok(());
    };

    let is_linked = is_discord_linked(&ctx, owner_uid).await;
    let bytes = tokio::fs::read(default_settings_path())
        .await
        .context("failed to read default.json")?;
    let mut default_settings: DanserSettings =
        serde_json::from_slice(&bytes).context("failed to deserialize default.json")?;
    default_settings.apply_render_quality(
        crate::core::settings::RenderQuality::HdPlus900p60.cap_for_entitlement(is_linked),
    );

    let user_path = user_settings_path(owner_uid);
    save_user_settings(&user_path, &default_settings).await?;

    let user = component.user()?.clone();
    let builder = build_menu_message(
        &ctx,
        &user,
        &default_settings,
        is_linked,
        SettingsTab::Presets,
        Some("♻️ Restored all settings to server defaults."),
    );
    component.callback(&ctx, builder).await?;
    Ok(())
}

pub async fn handle_user_settings_done(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let Some((owner_uid, _)) =
        authorize_settings_component(&ctx, &component, "uset_done:").await?
    else {
        return Ok(());
    };

    let user = component.user()?.clone();
    let (_, settings) = load_or_init_user_settings(owner_uid).await?;
    let is_linked = is_discord_linked(&ctx, owner_uid).await;
    let embed = create_settings_embed(&user, &settings, is_linked);
    let builder = MessageBuilder::new()
        .content("✅ Settings saved!")
        .embed(embed)
        .components(vec![]);
    component.callback(&ctx, builder).await?;
    crate::core::events::interaction::clear_interaction_component_expiry(component.message.id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_and_apply_menu_modal_valid_and_invalid() {
        let mut settings = DanserSettings::default();

        let ok_vis = parse_and_apply_menu_modal(
            SettingsTab::Visuals,
            &[("dim", "85"), ("cursor_scale", "1.25")],
            &mut settings,
        );
        assert!(ok_vis.is_ok());
        assert!((settings.playfield.background.dim.normal - 0.85).abs() < 1e-6);
        assert!((settings.skin.cursor.scale - 1.25).abs() < 1e-6);

        assert!(parse_and_apply_menu_modal(
            SettingsTab::Visuals,
            &[("dim", "120"), ("cursor_scale", "1.0")],
            &mut settings,
        )
        .is_err());
        assert!(parse_and_apply_menu_modal(
            SettingsTab::Visuals,
            &[("dim", "80"), ("cursor_scale", "0.05")],
            &mut settings,
        )
        .is_err());

        let ok_hud = parse_and_apply_menu_modal(
            SettingsTab::Hud,
            &[
                ("pp_decimals", "1"),
                ("hit_error_decimals", "2"),
                ("aim_error_decimals", "3"),
            ],
            &mut settings,
        );
        assert!(ok_hud.is_ok());
        assert_eq!(settings.gameplay.pp_counter.decimals, 1);
        assert_eq!(settings.gameplay.hit_error_meter.unstable_rate_decimals, 2);
        assert_eq!(settings.gameplay.aim_error_meter.unstable_rate_decimals, 3);
        assert!(parse_and_apply_menu_modal(
            SettingsTab::Hud,
            &[
                ("pp_decimals", "4"),
                ("hit_error_decimals", "2"),
                ("aim_error_decimals", "2"),
            ],
            &mut settings,
        )
        .is_err());

        let ok_audio = parse_and_apply_menu_modal(
            SettingsTab::Audio,
            &[("music_volume", "60"), ("hitsound_volume", "90")],
            &mut settings,
        );
        assert!(ok_audio.is_ok());
        assert!((settings.audio.music_volume - 0.60).abs() < 1e-6);
        assert!((settings.audio.sample_volume - 0.90).abs() < 1e-6);
        assert!(parse_and_apply_menu_modal(
            SettingsTab::Audio,
            &[("music_volume", "-5"), ("hitsound_volume", "90")],
            &mut settings,
        )
        .is_err());
    }
}
