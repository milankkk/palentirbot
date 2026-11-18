use std::{path::PathBuf, sync::Arc};

use eyre::{Context as _, Result};
use twilight_model::{
    channel::message::{
        component::{
            ActionRow, Button, ButtonStyle, Component, SelectMenu, SelectMenuOption, SelectMenuType,
        },
        MessageFlags,
    },
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{marker::UserMarker, Id},
    user::User,
};

use crate::{
    core::{settings::DanserSettings, BotConfig, Context},
    util::{
        builder::{MessageBuilder, ModalBuilder},
        interaction::{InteractionCommand, InteractionComponent, InteractionModal},
        Authored, ComponentExt, InteractionCommandExt, ModalExt,
    },
};

use super::{
    create_settings_embed,
    edit::{load_or_init_user_settings, resolve_skin_name, save_user_settings},
    preset::apply_preset,
    PresetChoice,
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

pub async fn menu(ctx: Arc<Context>, command: InteractionCommand) -> Result<()> {
    let user = command.user()?.clone();
    let uid = user.id;
    let (_, settings) = load_or_init_user_settings(uid, command.id).await?;
    let builder = build_menu_message(
        &ctx,
        &user,
        &settings,
        SettingsTab::Visuals,
        Some("⚙️ Use the menu below to customize your render settings. Only you can see this."),
    );
    command.callback(&ctx, builder, true).await?;
    Ok(())
}

fn build_menu_message<'a>(
    ctx: &Context,
    user: &User,
    settings: &DanserSettings,
    tab: SettingsTab,
    notice: Option<&str>,
) -> MessageBuilder<'a> {
    let embed = create_settings_embed(user, settings);
    let components = build_menu_components(ctx, user.id, settings, tab);
    let mut builder = MessageBuilder::new().embed(embed).components(components);
    if let Some(msg) = notice {
        builder = builder.content(msg.to_owned());
    }
    builder
}

fn toggle_btn(custom_id: String, label: &str, enabled: bool) -> Component {
    Component::Button(Button {
        sku_id: None,
        custom_id: Some(custom_id),
        disabled: false,
        emoji: None,
        label: Some(label.to_owned()),
        style: if enabled {
            ButtonStyle::Success
        } else {
            ButtonStyle::Secondary
        },
        url: None,
    })
}

fn action_btn(custom_id: String, label: &str, style: ButtonStyle) -> Component {
    Component::Button(Button {
        sku_id: None,
        custom_id: Some(custom_id),
        disabled: false,
        emoji: None,
        label: Some(label.to_owned()),
        style,
        url: None,
    })
}

fn build_menu_components(
    ctx: &Context,
    uid: Id<UserMarker>,
    settings: &DanserSettings,
    tab: SettingsTab,
) -> Vec<Component> {
    let mut rows = Vec::with_capacity(4);

    // Row 1: Category selector
    let tab_select = SelectMenu {
        custom_id: format!("uset_tab:{uid}"),
        disabled: false,
        max_values: Some(1),
        min_values: Some(1),
        options: Some(vec![
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
        ]),
        placeholder: Some("Switch settings category".to_owned()),
        channel_types: None,
        default_values: None,
        kind: SelectMenuType::Text,
    };
    rows.push(Component::ActionRow(ActionRow {
        components: vec![Component::SelectMenu(tab_select)],
    }));

    let t = tab.as_str();
    match tab {
        SettingsTab::Visuals => {
            if let Some(skin_row) = build_skin_select_row(ctx, uid, settings) {
                rows.push(skin_row);
            }
            rows.push(Component::ActionRow(ActionRow {
                components: vec![
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
                ],
            }));
            rows.push(Component::ActionRow(ActionRow {
                components: vec![
                    action_btn(
                        format!("uset_modal:{uid}:visuals"),
                        "🎚️ BG Dim & Cursor Scale",
                        ButtonStyle::Primary,
                    ),
                    action_btn(format!("uset_done:{uid}"), "✅ Done", ButtonStyle::Secondary),
                ],
            }));
        }
        SettingsTab::Hud => {
            rows.push(Component::ActionRow(ActionRow {
                components: vec![
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
                ],
            }));
            rows.push(Component::ActionRow(ActionRow {
                components: vec![
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
                ],
            }));
            rows.push(Component::ActionRow(ActionRow {
                components: vec![
                    action_btn(
                        format!("uset_modal:{uid}:hud"),
                        "🔢 Counter Decimals",
                        ButtonStyle::Primary,
                    ),
                    action_btn(format!("uset_done:{uid}"), "✅ Done", ButtonStyle::Secondary),
                ],
            }));
        }
        SettingsTab::Audio => {
            rows.push(Component::ActionRow(ActionRow {
                components: vec![
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
                ],
            }));
            rows.push(Component::ActionRow(ActionRow {
                components: vec![
                    action_btn(
                        format!("uset_modal:{uid}:audio"),
                        "🔊 Music & Hitsound Volume",
                        ButtonStyle::Primary,
                    ),
                    action_btn(format!("uset_done:{uid}"), "✅ Done", ButtonStyle::Secondary),
                ],
            }));
        }
        SettingsTab::Presets => {
            rows.push(Component::ActionRow(ActionRow {
                components: vec![
                    action_btn(
                        format!("uset_preset:{uid}:standard"),
                        "Standard 1080p",
                        ButtonStyle::Primary,
                    ),
                    action_btn(
                        format!("uset_preset:{uid}:clean"),
                        "Clean Showcase",
                        ButtonStyle::Primary,
                    ),
                    action_btn(
                        format!("uset_preset:{uid}:fast720p"),
                        "Fast 720p",
                        ButtonStyle::Primary,
                    ),
                    action_btn(
                        format!("uset_preset:{uid}:blur"),
                        "Cinematic Blur",
                        ButtonStyle::Primary,
                    ),
                ],
            }));
            rows.push(Component::ActionRow(ActionRow {
                components: vec![
                    action_btn(
                        format!("uset_reset:{uid}"),
                        "♻️ Reset to Default",
                        ButtonStyle::Danger,
                    ),
                    action_btn(format!("uset_done:{uid}"), "✅ Done", ButtonStyle::Secondary),
                ],
            }));
        }
    }

    rows
}

fn build_skin_select_row(
    ctx: &Context,
    uid: Id<UserMarker>,
    settings: &DanserSettings,
) -> Option<Component> {
    let skin_path = PathBuf::from(&settings.skin.current_skin);
    let current_raw = skin_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

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

    names.sort_by_key(|a| a.to_ascii_lowercase());
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
            let display: String = raw.replace('_', " ").chars().take(100).collect();
            SelectMenuOption {
                default: is_current,
                description: is_current.then(|| "Currently selected".to_owned()),
                emoji: None,
                label: display,
                value: raw,
            }
        })
        .collect();

    Some(Component::ActionRow(ActionRow {
        components: vec![Component::SelectMenu(SelectMenu {
            custom_id: format!("uset_skin:{uid}"),
            disabled: false,
            max_values: Some(1),
            min_values: Some(1),
            options: Some(options),
            placeholder: Some("Choose your skin".to_owned()),
            channel_types: None,
            default_values: None,
            kind: SelectMenuType::Text,
        })],
    }))
}

fn parse_uid_and_rest<'a>(custom_id: &'a str, prefix: &str) -> Option<(Id<UserMarker>, &'a str)> {
    let after = custom_id.strip_prefix(prefix)?;
    match after.split_once(':') {
        Some((uid_str, rest)) => Some((Id::new(uid_str.parse::<u64>().ok()?), rest)),
        None => Some((Id::new(after.parse::<u64>().ok()?), "")),
    }
}

async fn ensure_owner_component(
    ctx: &Context,
    component: &InteractionComponent,
    owner_uid: Id<UserMarker>,
) -> Result<bool> {
    if component.user_id()? == owner_uid {
        return Ok(true);
    }
    let response = InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            content: Some(
                "This settings menu belongs to another user. Run `/settings menu` to edit yours."
                    .to_owned(),
            ),
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    };
    ctx.interaction()
        .create_response(component.id, &component.token, &response)
        .await?;
    Ok(false)
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
    let Some((owner_uid, _)) = parse_uid_and_rest(&component.data.custom_id, "uset_open:") else {
        return Ok(());
    };
    if !ensure_owner_component(&ctx, &component, owner_uid).await? {
        return Ok(());
    }
    let user = component.user()?.clone();
    let (_, settings) = load_or_init_user_settings(owner_uid, component.id).await?;
    let builder = build_menu_message(
        &ctx,
        &user,
        &settings,
        SettingsTab::Visuals,
        Some("⚙️ Use the menu below to customize your render settings. Only you can see this."),
    );
    let data = InteractionResponseData {
        components: builder.components,
        embeds: builder.embed.map(|e| vec![e]),
        content: builder.content.map(|c| c.into_owned()),
        flags: Some(MessageFlags::EPHEMERAL),
        ..Default::default()
    };
    let response = InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(data),
    };
    ctx.interaction()
        .create_response(component.id, &component.token, &response)
        .await?;
    Ok(())
}

pub async fn handle_user_settings_tab(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let Some((owner_uid, _)) = parse_uid_and_rest(&component.data.custom_id, "uset_tab:") else {
        return Ok(());
    };
    if !ensure_owner_component(&ctx, &component, owner_uid).await? {
        return Ok(());
    }
    let Some(tab) = component
        .data
        .values
        .first()
        .and_then(|s| SettingsTab::from_str(s))
    else {
        return Ok(());
    };
    let user = component.user()?.clone();
    let (_, settings) = load_or_init_user_settings(owner_uid, component.id).await?;
    let builder = build_menu_message(&ctx, &user, &settings, tab, None);
    component.callback(&ctx, builder).await?;
    Ok(())
}

pub async fn handle_user_settings_skin(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let Some((owner_uid, _)) = parse_uid_and_rest(&component.data.custom_id, "uset_skin:") else {
        return Ok(());
    };
    if !ensure_owner_component(&ctx, &component, owner_uid).await? {
        return Ok(());
    }
    let Some(chosen) = component.data.values.first() else {
        return Ok(());
    };
    let Some(skin_name) = resolve_skin_name(&ctx, chosen)? else {
        let user = component.user()?.clone();
        let (_, settings) = load_or_init_user_settings(owner_uid, component.id).await?;
        let builder = build_menu_message(
            &ctx,
            &user,
            &settings,
            SettingsTab::Visuals,
            Some("❌ Skin not found on disk."),
        );
        component.callback(&ctx, builder).await?;
        return Ok(());
    };

    let user = component.user()?.clone();
    let (user_path, mut settings) = load_or_init_user_settings(owner_uid, component.id).await?;
    let mut curr_skin_path = PathBuf::from(&settings.skin.current_skin);
    if curr_skin_path.parent().is_some_and(|p| !p.as_os_str().is_empty()) {
        curr_skin_path.pop();
        curr_skin_path.push(&skin_name);
    } else {
        curr_skin_path = BotConfig::get().paths.skins().join(&skin_name);
    }
    settings.skin.current_skin = curr_skin_path.to_string_lossy().into_owned();
    save_user_settings(owner_uid, component.id, &user_path, &settings).await?;

    let display = skin_name.replace('_', " ");
    let notice = format!("✅ Skin set to `{display}`");
    let builder = build_menu_message(&ctx, &user, &settings, SettingsTab::Visuals, Some(&notice));
    component.callback(&ctx, builder).await?;
    Ok(())
}

pub async fn handle_user_settings_toggle(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let Some((owner_uid, rest)) = parse_uid_and_rest(&component.data.custom_id, "uset_tog:") else {
        return Ok(());
    };
    if !ensure_owner_component(&ctx, &component, owner_uid).await? {
        return Ok(());
    }
    let Some((tab_str, key)) = rest.split_once(':') else {
        return Ok(());
    };
    let Some(tab) = SettingsTab::from_str(tab_str) else {
        return Ok(());
    };

    let user = component.user()?.clone();
    let (user_path, mut settings) = load_or_init_user_settings(owner_uid, component.id).await?;
    let Some(notice) = apply_toggle(&mut settings, key) else {
        return Ok(());
    };
    save_user_settings(owner_uid, component.id, &user_path, &settings).await?;

    let builder = build_menu_message(&ctx, &user, &settings, tab, Some(&notice));
    component.callback(&ctx, builder).await?;
    Ok(())
}

fn apply_toggle(settings: &mut DanserSettings, key: &str) -> Option<String> {
    let state_word = |on: bool| if on { "enabled" } else { "disabled" };
    let vis_word = |on: bool| if on { "shown" } else { "hidden" };
    match key {
        "storyboard" => {
            settings.playfield.background.load_storyboards =
                !settings.playfield.background.load_storyboards;
            Some(format!(
                "✅ Storyboard {}",
                state_word(settings.playfield.background.load_storyboards)
            ))
        }
        "video" => {
            settings.playfield.background.load_videos = !settings.playfield.background.load_videos;
            Some(format!(
                "✅ BG Video {}",
                state_word(settings.playfield.background.load_videos)
            ))
        }
        "motion_blur" => {
            settings.recording.motion_blur.enabled = !settings.recording.motion_blur.enabled;
            Some(format!(
                "✅ Motion Blur {}",
                state_word(settings.recording.motion_blur.enabled)
            ))
        }
        "cursor_ripples" => {
            settings.cursor.cursor_ripples = !settings.cursor.cursor_ripples;
            Some(format!(
                "✅ Cursor Ripples {}",
                vis_word(settings.cursor.cursor_ripples)
            ))
        }
        "pp" => {
            settings.gameplay.pp_counter.show = !settings.gameplay.pp_counter.show;
            Some(format!(
                "✅ PP Counter {}",
                vis_word(settings.gameplay.pp_counter.show)
            ))
        }
        "leaderboard" => {
            settings.gameplay.score_board.show = !settings.gameplay.score_board.show;
            Some(format!(
                "✅ Leaderboard {}",
                vis_word(settings.gameplay.score_board.show)
            ))
        }
        "key_overlay" => {
            settings.gameplay.key_overlay.show = !settings.gameplay.key_overlay.show;
            Some(format!(
                "✅ Key Overlay {}",
                vis_word(settings.gameplay.key_overlay.show)
            ))
        }
        "hit_counter" => {
            settings.gameplay.hit_counter.show = !settings.gameplay.hit_counter.show;
            Some(format!(
                "✅ Hit Counter {}",
                vis_word(settings.gameplay.hit_counter.show)
            ))
        }
        "sliderbreaks" => {
            settings.gameplay.hit_counter.show_sliderbreaks =
                !settings.gameplay.hit_counter.show_sliderbreaks;
            Some(format!(
                "✅ Sliderbreaks {}",
                vis_word(settings.gameplay.hit_counter.show_sliderbreaks)
            ))
        }
        "strain_graph" => {
            settings.gameplay.strain_graph.show = !settings.gameplay.strain_graph.show;
            Some(format!(
                "✅ Strain Graph {}",
                vis_word(settings.gameplay.strain_graph.show)
            ))
        }
        "strain_outline" => {
            settings.gameplay.strain_graph.outline.show =
                !settings.gameplay.strain_graph.outline.show;
            Some(format!(
                "✅ Strain Graph Outline {}",
                vis_word(settings.gameplay.strain_graph.outline.show)
            ))
        }
        "hit_error" => {
            settings.gameplay.hit_error_meter.show = !settings.gameplay.hit_error_meter.show;
            Some(format!(
                "✅ Hit Error Meter {}",
                vis_word(settings.gameplay.hit_error_meter.show)
            ))
        }
        "aim_error" => {
            settings.gameplay.aim_error_meter.show = !settings.gameplay.aim_error_meter.show;
            Some(format!(
                "✅ Aim Error Meter {}",
                vis_word(settings.gameplay.aim_error_meter.show)
            ))
        }
        "beatmap_hitsounds" => {
            settings.audio.ignore_beatmap_samples = !settings.audio.ignore_beatmap_samples;
            Some(format!(
                "✅ Beatmap Hitsounds {}",
                state_word(!settings.audio.ignore_beatmap_samples)
            ))
        }
        "skip_intro" => {
            settings.gameplay.always_skip_intro = !settings.gameplay.always_skip_intro;
            Some(format!(
                "✅ Skip Intro {}",
                state_word(settings.gameplay.always_skip_intro)
            ))
        }
        "results_screen" => {
            settings.gameplay.show_results_screen = !settings.gameplay.show_results_screen;
            Some(format!(
                "✅ Results Screen {}",
                state_word(settings.gameplay.show_results_screen)
            ))
        }
        _ => None,
    }
}

pub async fn handle_user_settings_open_modal(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let Some((owner_uid, tab_str)) =
        parse_uid_and_rest(&component.data.custom_id, "uset_modal:")
    else {
        return Ok(());
    };
    if !ensure_owner_component(&ctx, &component, owner_uid).await? {
        return Ok(());
    }
    let Some(tab) = SettingsTab::from_str(tab_str) else {
        return Ok(());
    };
    let (_, settings) = load_or_init_user_settings(owner_uid, component.id).await?;
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

    match tab {
        SettingsTab::Visuals => {
            let dim: u8 = find("dim")
                .parse::<i64>()
                .ok()
                .filter(|&v| (0..=100).contains(&v))
                .and_then(|v| u8::try_from(v).ok())
                .ok_or_else(|| "Background Dim must be an integer between 0 and 100.".to_owned())?;
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
            let parse_pct = |key: &str, label: &str| -> std::result::Result<u8, String> {
                find(key)
                    .parse::<i64>()
                    .ok()
                    .filter(|&v| (0..=100).contains(&v))
                    .and_then(|v| u8::try_from(v).ok())
                    .ok_or_else(|| format!("{label} must be an integer between 0 and 100."))
            };
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

    let pairs: Vec<(String, String)> = modal
        .data
        .components
        .iter()
        .flat_map(|row| row.components.iter())
        .map(|inp| (inp.custom_id.clone(), inp.value.clone().unwrap_or_default()))
        .collect();
    let pair_refs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let (user_path, mut settings) = load_or_init_user_settings(owner_uid, modal.id).await?;
    let notice = match parse_and_apply_menu_modal(tab, &pair_refs, &mut settings) {
        Ok(msg) => msg,
        Err(reason) => {
            modal
                .callback(&ctx, MessageBuilder::new().content(format!("❌ {reason}")))
                .await?;
            return Ok(());
        }
    };

    save_user_settings(owner_uid, modal.id, &user_path, &settings).await?;
    let user = modal.user()?.clone();
    let builder = build_menu_message(&ctx, &user, &settings, tab, Some(&notice));
    modal.update_callback(&ctx, builder).await?;
    Ok(())
}

pub async fn handle_user_settings_preset(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let Some((owner_uid, preset_key)) =
        parse_uid_and_rest(&component.data.custom_id, "uset_preset:")
    else {
        return Ok(());
    };
    if !ensure_owner_component(&ctx, &component, owner_uid).await? {
        return Ok(());
    }

    let choice = match preset_key {
        "standard" => PresetChoice::Standard,
        "clean" => PresetChoice::CleanRecord,
        "fast720p" => PresetChoice::Performance720p,
        "blur" => PresetChoice::CinematicBlur,
        _ => return Ok(()),
    };

    let user = component.user()?.clone();
    let (user_path, mut settings) = load_or_init_user_settings(owner_uid, component.id).await?;
    let label = apply_preset(&mut settings, choice);
    save_user_settings(owner_uid, component.id, &user_path, &settings).await?;

    let notice = format!("✅ Applied preset **{label}**");
    let builder = build_menu_message(&ctx, &user, &settings, SettingsTab::Presets, Some(&notice));
    component.callback(&ctx, builder).await?;
    Ok(())
}

pub async fn handle_user_settings_reset(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let Some((owner_uid, _)) = parse_uid_and_rest(&component.data.custom_id, "uset_reset:") else {
        return Ok(());
    };
    if !ensure_owner_component(&ctx, &component, owner_uid).await? {
        return Ok(());
    }

    let danser_dir = BotConfig::get().paths.danser();
    let default_path = danser_dir.join("settings/default.json");
    let bytes = tokio::fs::read(&default_path)
        .await
        .context("failed to read default.json")?;
    let default_settings: DanserSettings =
        serde_json::from_slice(&bytes).context("failed to deserialize default.json")?;

    let user_path = danser_dir.join(format!("settings/{owner_uid}.json"));
    save_user_settings(owner_uid, component.id, &user_path, &default_settings).await?;

    let user = component.user()?.clone();
    let builder = build_menu_message(
        &ctx,
        &user,
        &default_settings,
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
    let Some((owner_uid, _)) = parse_uid_and_rest(&component.data.custom_id, "uset_done:") else {
        return Ok(());
    };
    if !ensure_owner_component(&ctx, &component, owner_uid).await? {
        return Ok(());
    }

    let user = component.user()?.clone();
    let (_, settings) = load_or_init_user_settings(owner_uid, component.id).await?;
    let embed = create_settings_embed(&user, &settings);
    let builder = MessageBuilder::new()
        .content("✅ Settings saved!")
        .embed(embed)
        .components(vec![]);
    component.callback(&ctx, builder).await?;
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
