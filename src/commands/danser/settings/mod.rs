use std::{convert::TryFrom, path::PathBuf, sync::Arc};

use command_macros::SlashCommand;
use eyre::Result;
use twilight_interactions::command::{
    AutocompleteValue, CommandModel, CommandOption, CreateCommand, CreateOption, ResolvedUser,
};
use twilight_model::channel::message::embed::{Embed, EmbedField};
use twilight_model::{
    channel::Attachment,
    id::{marker::UserMarker, Id},
    user::User,
};

use crate::{
    core::{settings::DanserSettings, Context},
    util::{builder::EmbedBuilder, interaction::InteractionCommand, InteractionCommandExt},
};

pub use self::{copy::*, default::*, edit::*, menu::*, preset::*, view::*};

mod copy;
mod default;
mod edit;
pub mod menu;
mod preset;
mod view;

#[derive(CreateCommand, SlashCommand)]
#[command(name = "settings")]
#[flags(SKIP_DEFER)]
#[allow(unused)]
/// Adjust your danser settings
pub enum Settings {
    #[command(name = "copy")]
    Copy(SettingsCopy),
    #[command(name = "default")]
    Default(SettingsDefault),
    #[command(name = "edit")]
    Edit(SettingsEdit),
    #[command(name = "menu")]
    Menu(SettingsMenu),
    #[command(name = "preset")]
    Preset(SettingsPreset),
    #[command(name = "view")]
    View(SettingsView),
}

#[derive(CommandModel)]
pub enum SettingsParsable {
    #[command(name = "copy")]
    Copy(SettingsCopy),
    #[command(name = "default")]
    Default(SettingsDefault),
    #[command(name = "edit")]
    Edit(SettingsEditAutocomplete),
    #[command(name = "menu")]
    Menu(SettingsMenu),
    #[command(name = "preset")]
    Preset(SettingsPreset),
    #[command(name = "view")]
    View(SettingsView),
}

#[derive(CreateCommand, CommandModel)]
#[command(name = "menu")]
/// Open an interactive menu to easily adjust your render settings
pub struct SettingsMenu;

#[derive(CreateCommand, CommandModel)]
#[command(name = "copy")]
/// Copy over the settings from another user
pub struct SettingsCopy {
    /// User from which the settings will be copied
    user: Id<UserMarker>,
}

#[derive(CreateCommand, CommandModel)]
#[command(name = "default")]
/// Restore your settings to the default values
pub struct SettingsDefault {
    /// Confirm that you want to overwrite your current settings
    confirm: SettingsConfirm,
}

#[derive(CreateOption, CommandOption)]
pub enum SettingsConfirm {
    #[option(name = "Oops missclick", value = "cancel")]
    Cancel,
    #[option(name = "I want to overwrite my current settings", value = "confirm")]
    Confirm,
}

#[derive(CreateCommand)]
#[command(name = "edit")]
/// Make specific changes to your settings
pub struct SettingsEdit {
    /// Provide your own .json settings file
    file: Option<Attachment>,
    #[command(autocomplete = true)]
    /// Specify one of the available skins
    skin: Option<String>,
    #[command(min_value = 0.1, max_value = 2.0)]
    /// Scale the size of the cursor
    cursor_scale: Option<f64>,
    /// Whether the cursor should generate ripples
    cursor_ripples: Option<Visibility>,
    /// Whether the leaderboard should be displayed
    leaderboard: Option<Visibility>,
    /// Whether the storyboard should play
    storyboard: Option<State>,
    /// Whether the video should play
    video: Option<State>,
    #[command(min_value = 0, max_value = 100)]
    /// Percent background dim
    dim: Option<u8>,
    #[command(min_value = 0, max_value = 100)]
    /// Percent music volume
    music_volume: Option<u8>,
    #[command(min_value = 0, max_value = 100)]
    /// Percent hitsound volume
    hitsound_volume: Option<u8>,
    /// Whether the beatmap's hitsounds should be used
    beatmap_hitsounds: Option<State>,
    /// Whether pp should be displayed
    pp: Option<Visibility>,
    #[command(min_value = 0, max_value = 3)]
    /// How many decimal places the pp value should have
    pp_decimals: Option<u32>,
    /// Whether the hit error meter should be displayed
    hit_error_meter: Option<Visibility>,
    #[command(min_value = 0, max_value = 3)]
    /// How many decimal places the hit error should have
    hit_error_decimals: Option<u32>,
    /// Whether the aim error meter should be displayed
    aim_error_meter: Option<Visibility>,
    #[command(min_value = 0, max_value = 3)]
    /// How many decimal places the aim error UR should have
    aim_error_decimals: Option<u32>,
    /// Whether the hit counter should be displayed
    hit_counter: Option<Visibility>,
    /// Whether sliderbreaks should be displayed
    sliderbreaks: Option<Visibility>,
    /// Whether the strain graph should be displayed
    strain_graph: Option<Visibility>,
    /// Whether the outline of the strain graph should be displayed
    strain_graph_outline: Option<Visibility>,
    /// whether the key overlay should be displayed
    key_overlay: Option<Visibility>,
    /// Whether motion blur is enabled
    motion_blur: Option<State>,
    /// Whether the results screen is shown after a replay
    results_screen: Option<State>,
    /// Whether intros/breaks are automatically skipped
    skip_intro: Option<State>,
}

impl TryFrom<SettingsEditAutocomplete> for SettingsEdit {
    type Error = String;

    #[inline]
    fn try_from(edit: SettingsEditAutocomplete) -> Result<Self, Self::Error> {
        let SettingsEditAutocomplete {
            file,
            skin,
            cursor_scale,
            cursor_ripples,
            leaderboard,
            storyboard,
            video,
            dim,
            music_volume,
            hitsound_volume,
            beatmap_hitsounds,
            pp,
            pp_decimals,
            hit_error_meter,
            hit_error_decimals,
            aim_error_meter,
            aim_error_decimals,
            hit_counter,
            sliderbreaks,
            strain_graph,
            strain_graph_outline,
            key_overlay,
            motion_blur,
            results_screen,
            skip_intro,
        } = edit;

        let skin = match skin {
            AutocompleteValue::Focused(skin) => return Err(skin),
            AutocompleteValue::None => None,
            AutocompleteValue::Completed(skin) => Some(skin),
        };

        let edit = Self {
            file,
            skin,
            cursor_scale,
            cursor_ripples,
            leaderboard,
            storyboard,
            video,
            dim,
            music_volume,
            hitsound_volume,
            beatmap_hitsounds,
            pp,
            pp_decimals,
            hit_error_meter,
            hit_error_decimals,
            aim_error_meter,
            aim_error_decimals,
            hit_counter,
            sliderbreaks,
            strain_graph,
            strain_graph_outline,
            key_overlay,
            motion_blur,
            results_screen,
            skip_intro,
        };

        Ok(edit)
    }
}

#[derive(CommandModel)]
#[command(autocomplete = true)]
pub struct SettingsEditAutocomplete {
    file: Option<Attachment>,
    skin: AutocompleteValue<String>,
    cursor_scale: Option<f64>,
    cursor_ripples: Option<Visibility>,
    leaderboard: Option<Visibility>,
    storyboard: Option<State>,
    video: Option<State>,
    dim: Option<u8>,
    music_volume: Option<u8>,
    hitsound_volume: Option<u8>,
    beatmap_hitsounds: Option<State>,
    pp: Option<Visibility>,
    pp_decimals: Option<u32>,
    hit_error_meter: Option<Visibility>,
    hit_error_decimals: Option<u32>,
    aim_error_meter: Option<Visibility>,
    aim_error_decimals: Option<u32>,
    hit_counter: Option<Visibility>,
    sliderbreaks: Option<Visibility>,
    strain_graph: Option<Visibility>,
    strain_graph_outline: Option<Visibility>,
    key_overlay: Option<Visibility>,
    motion_blur: Option<State>,
    results_screen: Option<State>,
    skip_intro: Option<State>,
}

#[derive(CreateCommand, CommandModel)]
#[command(name = "preset")]
/// Apply a curated render preset to your danser settings
pub struct SettingsPreset {
    /// Preset profile to apply
    preset: PresetChoice,
}

#[derive(CreateOption, CommandOption)]
pub enum PresetChoice {
    #[option(name = "Standard 1080p60 (balanced defaults)", value = "standard")]
    Standard,
    #[option(name = "Clean Showcase (minimal HUD)", value = "clean")]
    CleanRecord,
    #[option(name = "Fast 720p60 (no storyboard/video)", value = "fast720p")]
    Performance720p,
    #[option(name = "Cinematic Motion Blur (1080p60 + blur)", value = "blur")]
    CinematicBlur,
}

#[derive(CreateOption, CommandOption)]
pub enum Visibility {
    #[option(name = "Show", value = "show")]
    Show,
    #[option(name = "Hide", value = "hide")]
    Hide,
}

#[derive(CreateOption, CommandOption)]
pub enum State {
    #[option(name = "Enabled", value = "enabled")]
    Enabled,
    #[option(name = "Disabled", value = "disabled")]
    Disabled,
}

#[derive(CreateCommand, CommandModel)]
#[command(name = "view")]
/// View your own or another user's render settings
pub struct SettingsView {
    /// User to view (defaults to yourself)
    pub(crate) user: Option<ResolvedUser>,
}

pub async fn slash_settings(ctx: Arc<Context>, mut command: InteractionCommand) -> Result<()> {
    match SettingsParsable::from_interaction(command.input_data())? {
        SettingsParsable::Copy(args) => copy(ctx, command, args).await,
        SettingsParsable::Default(args) => default(ctx, command, args).await,
        SettingsParsable::Edit(args) => edit(ctx, command, args).await,
        SettingsParsable::Menu(_) => menu(ctx, command).await,
        SettingsParsable::Preset(args) => preset(ctx, command, args).await,
        SettingsParsable::View(args) => view(ctx, command, args).await,
    }
}

pub(crate) fn create_settings_embed(user: &User, settings: &DanserSettings) -> Embed {
    let skin_path = PathBuf::from(&settings.skin.current_skin);
    let skin = skin_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "default".to_owned())
        .replace('_', " ");

    let flag = |b: bool| if b { "✅" } else { "❌" };
    let pct = |value: f64| format!("{}%", (value * 100.0).round());

    let pp_text = if settings.gameplay.pp_counter.show {
        format!("✅ ({} dec)", settings.gameplay.pp_counter.decimals)
    } else {
        "❌".to_owned()
    };
    let hit_err_text = if settings.gameplay.hit_error_meter.show {
        format!(
            "✅ ({} dec)",
            settings.gameplay.hit_error_meter.unstable_rate_decimals
        )
    } else {
        "❌".to_owned()
    };
    let aim_err_text = if settings.gameplay.aim_error_meter.show {
        format!(
            "✅ ({} dec)",
            settings.gameplay.aim_error_meter.unstable_rate_decimals
        )
    } else {
        "❌".to_owned()
    };

    let fields = vec![
        EmbedField {
            inline: true,
            name: "🎨 Skin & Visuals".to_owned(),
            value: format!(
                "**Skin:** `{skin}`\n\
                **Resolution:** `{}x{} @ {}fps`\n\
                **Cursor Scale:** `{:.2}x`\n\
                **Cursor Ripples:** {}\n\
                **Motion Blur:** {}",
                settings.recording.frame_width,
                settings.recording.frame_height,
                settings.recording.fps,
                settings.skin.cursor.scale,
                flag(settings.cursor.cursor_ripples),
                flag(settings.recording.motion_blur.enabled),
            ),
        },
        EmbedField {
            inline: true,
            name: "🔊 Background & Audio".to_owned(),
            value: format!(
                "**BG Dim:** `{}`\n\
                **Storyboard:** {} | **BG Video:** {}\n\
                **Music:** `{}` | **Hitsounds:** `{}`\n\
                **Map Hitsounds:** {}",
                pct(settings.playfield.background.dim.normal),
                flag(settings.playfield.background.load_storyboards),
                flag(settings.playfield.background.load_videos),
                pct(settings.audio.music_volume),
                pct(settings.audio.sample_volume),
                flag(!settings.audio.ignore_beatmap_samples),
            ),
        },
        EmbedField {
            inline: false,
            name: "📊 HUD & Overlays".to_owned(),
            value: format!(
                "**PP:** {pp_text} • **Leaderboard:** {} • **Keys:** {}\n\
                **Hit Counter:** {} *(SB: {})* • **Strain Graph:** {} *(Outline: {})*\n\
                **Hit Error:** {hit_err_text} • **Aim Error:** {aim_err_text}",
                flag(settings.gameplay.score_board.show),
                flag(settings.gameplay.key_overlay.show),
                flag(settings.gameplay.hit_counter.show),
                flag(settings.gameplay.hit_counter.show_sliderbreaks),
                flag(settings.gameplay.strain_graph.show),
                flag(settings.gameplay.strain_graph.outline.show),
            ),
        },
        EmbedField {
            inline: false,
            name: "▶️ Playback".to_owned(),
            value: format!(
                "**Skip Intro:** {} • **Results Screen:** {}",
                flag(settings.gameplay.always_skip_intro),
                flag(settings.gameplay.show_results_screen),
            ),
        },
    ];

    EmbedBuilder::new()
        .color(0x96DFE3)
        .title(format!("Render Settings — {}", user.name))
        .fields(fields)
        .footer("Tip: Run /settings menu for one-click controls")
        .build()
}
