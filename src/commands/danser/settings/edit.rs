use std::{
    convert::TryInto,
    path::PathBuf,
    sync::Arc,
};

use eyre::{Context as _, Report, Result};
use tokio::fs;
use twilight_model::{
    application::command::CommandOptionChoice,
    id::{marker::UserMarker, Id},
};

use crate::{
    core::{settings::DanserSettings, BotConfig, Context},
    util::{
        builder::MessageBuilder, interaction::InteractionCommand, levenshtein_distance, Authored,
        CowUtils, InteractionCommandExt,
    },
};

use super::{create_settings_embed, SettingsEdit, SettingsEditAutocomplete, State, Visibility};

pub(crate) fn autocomplete_skins(ctx: &Context, input: &str) -> Result<Vec<CommandOptionChoice>> {
    let no_underscores = input.cow_replace('_', " ");
    let skin = no_underscores.cow_to_ascii_lowercase();

    let mut skins: Vec<_> = ctx
        .skin_list()
        .get()?
        .iter()
        .map(|skin| {
            skin.to_string_lossy()
                .cow_replace('_', " ")
                .to_ascii_lowercase()
        })
        .filter(|haystack| haystack.contains(skin.as_ref()))
        .take(25)
        .collect();

    skins.sort_unstable_by_key(|opt| levenshtein_distance(opt, skin.as_ref()).0);

    Ok(skins
        .into_iter()
        .map(|skin| CommandOptionChoice {
            name: skin.clone(),
            name_localizations: None,
            value: twilight_model::application::command::CommandOptionChoiceValue::String(skin),
        })
        .collect())
}

pub(crate) fn resolve_skin_name(ctx: &Context, skin: &str) -> Result<Option<String>> {
    let skin_lower = skin.trim().cow_replace('_', " ").to_ascii_lowercase();
    if skin_lower.is_empty() {
        return Ok(None);
    }

    let mut skin_list = ctx.skin_list();
    let skins = skin_list.get()?;
    Ok(skins.iter().find_map(|skin_| {
        let s = skin_.to_string_lossy();
        let norm = s.cow_replace('_', " ").to_ascii_lowercase();
        (norm == skin_lower).then(|| s.into_owned())
    }))
}

pub(crate) async fn load_or_init_user_settings(
    author: Id<UserMarker>,
    tx_id: impl std::fmt::Display,
) -> Result<(PathBuf, DanserSettings)> {
    let danser_path = BotConfig::get().paths.danser();
    let mut user_path = danser_path.to_owned();
    user_path.push(format!("settings/{author}.json"));

    if !fs::try_exists(&user_path).await.unwrap_or(false) {
        let mut default_path = danser_path.to_owned();
        default_path.push("settings/default.json");
        let mut tmp_default = danser_path.to_owned();
        tmp_default.push(format!("settings/.{author}.json.{tx_id}.tmp"));
        fs::copy(&default_path, &tmp_default)
            .await
            .context("failed to copy default danser settings")?;
        if let Err(err) = fs::rename(&tmp_default, &user_path).await {
            let _ = fs::remove_file(&tmp_default).await;
            return Err(Report::from(err).wrap_err("failed to initialize user settings file"));
        }
    }

    let bytes = fs::read(&user_path)
        .await
        .with_context(|| format!("failed to read settings file at {user_path:?}"))?;
    let settings = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to deserialize settings file at {user_path:?}"))?;

    Ok((user_path, settings))
}

pub(crate) async fn save_user_settings(
    author: Id<UserMarker>,
    tx_id: impl std::fmt::Display,
    user_path: &PathBuf,
    settings: &DanserSettings,
) -> Result<()> {
    let json_bytes = serde_json::to_vec_pretty(settings)
        .with_context(|| format!("failed to serialize settings into {user_path:?}"))?;

    let mut tmp_path = BotConfig::get().paths.danser().to_owned();
    tmp_path.push(format!("settings/.{author}.json.{tx_id}.tmp"));
    if let Err(err) = fs::write(&tmp_path, &json_bytes).await {
        let _ = fs::remove_file(&tmp_path).await;
        return Err(Report::from(err).wrap_err(format!("failed to write settings into {tmp_path:?}")));
    }

    if let Err(err) = fs::rename(&tmp_path, user_path).await {
        let _ = fs::remove_file(&tmp_path).await;
        return Err(Report::from(err).wrap_err(format!("failed to commit settings into {user_path:?}")));
    }

    Ok(())
}

pub async fn edit(
    ctx: Arc<Context>,
    command: InteractionCommand,
    args: SettingsEditAutocomplete,
) -> Result<()> {
    let args: SettingsEdit = match args.try_into() {
        Ok(args) => args,
        Err(autocomplete) => {
            let choices = autocomplete_skins(&ctx, &autocomplete)?;
            command.autocomplete(&ctx, choices).await?;
            return Ok(());
        }
    };

    let author = command.user_id()?;

    // Retrieve attachment if provided
    let attached = if let Some(ref attachment) = args.file {
        if !matches!(attachment.filename.rsplit('.').next(), Some(ext) if ext.eq_ignore_ascii_case("json")) {
            let content = "The attached file must be of type .json";
            command.error_callback(&ctx, content, false).await?;

            return Ok(());
        }

        match ctx.client().get_discord_attachment(attachment).await {
            Ok(bytes) => match serde_json::from_slice::<DanserSettings>(&bytes) {
                Ok(settings) => Some(settings),
                Err(err) => {
                    let content = format!(
                        "Failed to deserialize the attached .json data into danser settings: {err}\n\
                        Be sure you provide valid danser settings."
                    );
                    command.error_callback(&ctx, content, false).await?;

                    return Ok(());
                }
            },
            Err(err) => {
                let content = "Failed to download the provided attachment";
                let _ = command.error_callback(&ctx, content, false).await;

                return Err(err.wrap_err("failed to download settings attachment"));
            }
        }
    } else {
        None
    };

    let (user_path, mut settings) = load_or_init_user_settings(author, command.id).await?;

    let had_attachment = attached.is_some();
    if let Some(attached) = attached {
        settings.audio = attached.audio;
        settings.input = attached.input;
        settings.gameplay = attached.gameplay;
        settings.cursor = attached.cursor;
        settings.objects = attached.objects;
        settings.playfield = attached.playfield;

        settings.skin.use_colors_from_skin = attached.skin.use_colors_from_skin;
        settings.skin.use_beatmap_colors = attached.skin.use_beatmap_colors;
        settings.skin.cursor = attached.skin.cursor;
    }

    // Modify the settings and check if something changed
    match modify_settings(&ctx, &mut settings, args) {
        ModifyResult::Change(changed) if changed || had_attachment => {
            if let Err(err) = save_user_settings(author, command.id, &user_path, &settings).await {
                let content = "Failed to save modified settings";
                let _ = command.error_callback(&ctx, content, false).await;
                return Err(err);
            }

            let user = command.user()?;
            let embed = create_settings_embed(user, &settings);
            let content = "Successfully changed settings!";
            let builder = MessageBuilder::new().content(content).embed(embed);
            command.callback(&ctx, builder, false).await?;

            Ok(())
        }
        ModifyResult::Change(_) => {
            let user = command.user()?;
            let embed = create_settings_embed(user, &settings);
            let builder = MessageBuilder::new().embed(embed);
            command.callback(&ctx, builder, false).await?;

            Ok(())
        }
        ModifyResult::SkinNotFound => {
            let content = "No skin with the specified name is stored.\n\
                Check `/skinlist` to see available skins.";
            command.error_callback(&ctx, content, false).await?;

            Ok(())
        }
        ModifyResult::Err(err) => {
            let content = "Something went wrong while modifying the settings";
            let _ = command.error_callback(&ctx, content, false).await;

            Err(err.wrap_err("failed to modify settings"))
        }
    }
}

enum ModifyResult {
    Change(bool),
    SkinNotFound,
    Err(Report),
}

fn modify_settings(
    ctx: &Context,
    settings: &mut DanserSettings,
    args: SettingsEdit,
) -> ModifyResult {
    let SettingsEdit {
        file: _,
        skin,
        cursor_scale,
        cursor_ripples,
        storyboard,
        video,
        dim,
        music_volume,
        hitsound_volume,
        leaderboard,
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
    } = args;

    let mut changed = false;

    if let Some(skin) = skin {
        let skin_name = match resolve_skin_name(ctx, &skin) {
            Ok(Some(name)) => name,
            Ok(None) => return ModifyResult::SkinNotFound,
            Err(err) => return ModifyResult::Err(err),
        };

        // Get skin of current settings
        let mut curr_skin_path = PathBuf::from(&settings.skin.current_skin);

        let is_different_skin = curr_skin_path
            .file_name()
            .is_none_or(|skin| skin.to_string_lossy() != skin_name);

        // Compare new with old
        if is_different_skin {
            curr_skin_path.pop();
            curr_skin_path.push(skin_name);
            settings.skin.current_skin = curr_skin_path.to_string_lossy().into_owned();
            changed = true;
        }
    }

    // Some simple macros:
    // - Compare $new with $field
    // - If they're different, assign $new to $field and set $changed to true

    macro_rules! assign_f64 {
        ($changed:ident: $($new:ident ~ $field:expr;)*) => {
            $(
                if let Some($new) = $new {
                    if ($field - $new).abs() > f64::EPSILON {
                        $field = $new;
                        $changed = true;
                    }
                }
            )*
        };
    }

    macro_rules! assign_cmp {
        ($changed:ident: $($new:ident ~ $field:expr;)*) => {
            $(
                if let Some($new) = $new {
                    if $field != $new {
                        $field = $new;
                        $changed = true;
                    }
                }
            )*
        };
    }

    macro_rules! assign_state {
        ($changed:ident: $($new:ident ~ $field:expr;)*) => {
            $(
                if let Some($new) = $new.map(|vis| matches!(vis, State::Enabled)) {
                    if $field != $new {
                        $field = $new;
                        $changed = true;
                    }
                }
            )*
        };
    }

    macro_rules! assign_reverse_state_cmp {
        ($changed:ident: $($new:ident ~ $field:expr;)*) => {
            $(
                if let Some($new) = $new.map(|vis| matches!(vis, State::Enabled)) {
                    if $field != !$new {
                        $field = !$new;
                        $changed = true;
                    }
                }
            )*
        };
    }

    macro_rules! assign_visibility {
        ($changed:ident: $($new:ident ~ $field:expr;)*) => {
            $(
                if let Some($new) = $new.map(|vis| matches!(vis, Visibility::Show)) {
                    if $field != $new {
                        $field = $new;
                        $changed = true;
                    }
                }
            )*
        };
    }

    macro_rules! assign_percent {
        ($changed:ident: $($new:ident ~ $field:expr;)*) => {
            $(
                if let Some($new) = $new {
                    let mapped = $new as f64 / 100.0;

                    if  ($field - mapped).abs() > f64::EPSILON {
                        $field = mapped;
                        $changed = true;
                    }
                }
            )*
        };
    }

    assign_f64! { changed:
        cursor_scale ~ settings.skin.cursor.scale;
    };

    assign_cmp! { changed:
        pp_decimals ~ settings.gameplay.pp_counter.decimals;
        hit_error_decimals ~ settings.gameplay.hit_error_meter.unstable_rate_decimals;
        aim_error_decimals ~ settings.gameplay.aim_error_meter.unstable_rate_decimals;
    }

    assign_reverse_state_cmp! { changed:
        beatmap_hitsounds ~ settings.audio.ignore_beatmap_samples;
    }

    assign_visibility! { changed:
        pp ~ settings.gameplay.pp_counter.show;
        hit_error_meter ~ settings.gameplay.hit_error_meter.show;
        aim_error_meter ~ settings.gameplay.aim_error_meter.show;
        hit_counter ~ settings.gameplay.hit_counter.show;
        sliderbreaks ~ settings.gameplay.hit_counter.show_sliderbreaks;
        strain_graph ~ settings.gameplay.strain_graph.show;
        strain_graph_outline ~ settings.gameplay.strain_graph.outline.show;
        cursor_ripples ~ settings.cursor.cursor_ripples;
        leaderboard ~ settings.gameplay.score_board.show;
        key_overlay ~ settings.gameplay.key_overlay.show;
    }

    assign_state! { changed:
        storyboard ~ settings.playfield.background.load_storyboards;
        video ~ settings.playfield.background.load_videos;
        motion_blur ~ settings.recording.motion_blur.enabled;
        results_screen ~ settings.gameplay.show_results_screen;
        skip_intro ~ settings.gameplay.always_skip_intro;
    }

    assign_percent! { changed:
        dim ~ settings.playfield.background.dim.normal;
        music_volume ~ settings.audio.music_volume;
        hitsound_volume ~ settings.audio.sample_volume;
    }

    ModifyResult::Change(changed)
}
