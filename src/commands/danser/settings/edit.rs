use std::{
    convert::TryInto,
    path::{Path, PathBuf},
    sync::Arc,
};

use eyre::{Context as _, Report, Result};
use tokio::fs;
use twilight_model::{
    application::command::CommandOptionChoice,
    id::{marker::UserMarker, Id},
};

use crate::{
    core::{settings::DanserSettings, Context},
    util::{
        builder::MessageBuilder,
        fs::{copy_atomic, write_json_atomic},
        interaction::InteractionCommand,
        levenshtein_distance, Authored, InteractionCommandExt,
    },
};

use super::{
    current_skin_basename, default_settings_path, rewrite_current_skin_path, user_settings_path,
    create_settings_embed, QualityChoice, SettingsEdit, SettingsEditAutocomplete, SettingsQuality,
    State, Visibility,
};

fn normalize_skin(s: &str) -> String {
    crate::util::str::percent_decode_utf8(s)
        .replace('_', " ")
        .to_ascii_lowercase()
}

fn eq_skin_name_ci(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes().zip(b.bytes()).all(|(x, y)| {
            let nx = if x == b'_' { b' ' } else { x.to_ascii_lowercase() };
            let ny = if y == b'_' { b' ' } else { y.to_ascii_lowercase() };
            nx == ny
        })
}

pub(crate) fn autocomplete_skins(ctx: &Context, input: &str) -> Result<Vec<CommandOptionChoice>> {
    let skin = normalize_skin(input);

    let mut skins: Vec<(String, String)> = ctx
        .skin_list()
        .get()?
        .iter()
        .filter_map(|skin_os| {
            let raw_name = skin_os.to_string_lossy().into_owned();
            if raw_name.len() > 100 {
                return None;
            }
            let norm = normalize_skin(&raw_name);
            norm.contains(&skin).then_some((norm, raw_name))
        })
        .collect();

    skins.sort_by_cached_key(|(norm, _)| levenshtein_distance(norm, &skin).0);
    skins.truncate(25);

    Ok(skins
        .into_iter()
        .map(|(_, raw_name)| {
            let pretty = super::format_skin_display_name(&raw_name);
            let display = crate::util::str::truncate_chars(&pretty, 100).to_owned();
            CommandOptionChoice {
                name: display,
                name_localizations: None,
                value: twilight_model::application::command::CommandOptionChoiceValue::String(
                    raw_name,
                ),
            }
        })
        .collect())
}

pub(crate) fn resolve_skin_name(ctx: &Context, skin: &str) -> Result<Option<String>> {
    let trimmed = skin.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let decoded_input = crate::util::str::percent_decode_utf8(trimmed);

    let mut skin_list = ctx.skin_list();
    let skins = skin_list.get()?;
    Ok(skins.iter().find_map(|skin_| {
        let s = skin_.to_string_lossy();
        let decoded_entry = crate::util::str::percent_decode_utf8(&s);
        let matched = s.eq_ignore_ascii_case(trimmed)
            || eq_skin_name_ci(&s, trimmed)
            || eq_skin_name_ci(&decoded_entry, trimmed)
            || eq_skin_name_ci(&decoded_entry, &decoded_input);
        matched.then(|| s.into_owned())
    }))
}

pub(crate) async fn load_or_init_user_settings(
    author: Id<UserMarker>,
) -> Result<(PathBuf, DanserSettings)> {
    let user_path = user_settings_path(author);

    if !fs::try_exists(&user_path).await.unwrap_or(false) {
        copy_atomic(&default_settings_path(), &user_path)
            .await
            .context("failed to initialize user settings file")?;
    }

    let bytes = fs::read(&user_path)
        .await
        .with_context(|| format!("failed to read settings file at {user_path:?}"))?;
    let mut settings: DanserSettings = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to deserialize settings file at {user_path:?}"))?;
    settings.normalize_current_skin();
    if !settings.has_valid_render_quality() {
        let normalized = settings.current_render_quality();
        settings.apply_render_quality(normalized);
    }

    Ok((user_path, settings))
}

pub(crate) async fn save_user_settings(
    user_path: &Path,
    settings: &DanserSettings,
) -> Result<()> {
    if settings.has_valid_render_quality() && settings.has_normalized_skin() {
        write_json_atomic(user_path, settings, true).await
    } else {
        let mut normalized = settings.clone();
        normalized.normalize_current_skin();
        let quality = normalized.current_render_quality();
        normalized.apply_render_quality(quality);
        write_json_atomic(user_path, &normalized, true).await
    }
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

    command.defer(&ctx, false).await?;

    let author = command.user_id()?;

    // Retrieve attachment if provided
    let attached = if let Some(ref attachment) = args.file {
        if !matches!(attachment.filename.rsplit('.').next(), Some(ext) if ext.eq_ignore_ascii_case("json")) {
            let content = "The attached file must be of type .json";
            command.error(&ctx, content).await?;

            return Ok(());
        }

        const MAX_SETTINGS_JSON_BYTES: u64 = 256 * 1024;
        if attachment.size > MAX_SETTINGS_JSON_BYTES {
            let content = "The attached settings file is too large (max 256 KB).";
            command.error(&ctx, content).await?;

            return Ok(());
        }

        match ctx
            .client()
            .get_discord_attachment_limited(attachment, MAX_SETTINGS_JSON_BYTES as usize)
            .await
        {
            Ok(bytes) => match serde_json::from_slice::<DanserSettings>(&bytes) {
                Ok(mut settings) => {
                    settings.sanitize_user_upload();
                    Some(settings)
                }
                Err(err) => {
                    let content = format!(
                        "Failed to deserialize the attached .json data into danser settings: {err}\n\
                        Be sure you provide valid danser settings."
                    );
                    command.error(&ctx, content).await?;

                    return Ok(());
                }
            },
            Err(err) => {
                let content = "Failed to download the provided attachment";
                let _ = command.error(&ctx, content).await;

                return Err(err.wrap_err("failed to download settings attachment"));
            }
        }
    } else {
        None
    };

    let (user_path, mut settings) = load_or_init_user_settings(author).await?;
    let is_linked = ctx
        .psql()
        .get_linked_osu_user(author)
        .await
        .ok()
        .flatten()
        .is_some();

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
            settings.clamp_render_quality(is_linked);
            if let Err(err) = save_user_settings(&user_path, &settings).await {
                let content = "Failed to save modified settings";
                let _ = command.error(&ctx, content).await;
                return Err(err);
            }

            let user = command.user()?;
            let embed = create_settings_embed(user, &settings, is_linked);
            let content = "Successfully changed settings!";
            let builder = MessageBuilder::new().content(content).embed(embed);
            command.update(&ctx, &builder).await?;

            Ok(())
        }
        ModifyResult::Change(_) => {
            let user = command.user()?;
            let embed = create_settings_embed(user, &settings, is_linked);
            let builder = MessageBuilder::new().embed(embed);
            command.update(&ctx, &builder).await?;

            Ok(())
        }
        ModifyResult::SkinNotFound => {
            let content = "No skin with the specified name is stored.\n\
                Check `/skin list` to see available skins.";
            command.error(&ctx, content).await?;

            Ok(())
        }
        ModifyResult::Err(err) => {
            let content = "Something went wrong while modifying the settings";
            let _ = command.error(&ctx, content).await;

            Err(err.wrap_err("failed to modify settings"))
        }
    }
}

pub async fn quality(
    ctx: Arc<Context>,
    command: InteractionCommand,
    args: SettingsQuality,
) -> Result<()> {
    let author = command.user_id()?;
    let is_linked = ctx
        .psql()
        .get_linked_osu_user(author)
        .await
        .ok()
        .flatten()
        .is_some();

    if matches!(
        args.quality,
        QualityChoice::HdPlus900p | QualityChoice::FullHd1080p
    ) && !is_linked
    {
        let content = "🔒 **900p60** and **1080p60** rendering require a verified osu! link.\n\
            Run `/link` once to verify your osu! account via OAuth, then try again (unlinked accounts default to **720p60**).";
        command.error_callback(&ctx, content, true).await?;
        return Ok(());
    }

    let (user_path, mut settings) = load_or_init_user_settings(author).await?;
    let target = match args.quality {
        QualityChoice::FullHd1080p => crate::core::settings::RenderQuality::FullHd1080p60,
        QualityChoice::HdPlus900p => crate::core::settings::RenderQuality::HdPlus900p60,
        QualityChoice::Hd720p => crate::core::settings::RenderQuality::Hd720p60,
    };
    settings.apply_render_quality(target);

    if let Err(err) = save_user_settings(&user_path, &settings).await {
        let _ = command
            .error_callback(&ctx, "Failed to save render quality setting", false)
            .await;
        return Err(err);
    }

    let user = command.user()?;
    let embed = create_settings_embed(user, &settings, is_linked);
    let mut msg = format!("✅ Render quality set to **{}**!", target.display_label());
    if matches!(target, crate::core::settings::RenderQuality::FullHd1080p60) {
        msg.push_str("\n⚠️ Heads up: **1080p60** takes noticeably longer to render than 900p60 or 720p60.");
    }
    let builder = MessageBuilder::new().content(msg).embed(embed);
    command.callback(&ctx, builder, false).await?;
    Ok(())
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

        if current_skin_basename(settings) != skin_name {
            rewrite_current_skin_path(settings, &skin_name);
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
