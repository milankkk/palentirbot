use std::sync::Arc;

use eyre::Result;

use crate::{
    core::{settings::DanserSettings, Context},
    util::{
        builder::MessageBuilder, interaction::InteractionCommand, Authored, InteractionCommandExt,
    },
};

use super::{
    create_settings_embed,
    edit::{load_or_init_user_settings, save_user_settings},
    PresetChoice, SettingsPreset,
};

pub(crate) fn apply_preset(settings: &mut DanserSettings, preset: PresetChoice) -> &'static str {
    match preset {
        PresetChoice::Standard => {
            settings.recording.frame_width = 1920;
            settings.recording.frame_height = 1080;
            settings.recording.fps = 60;
            settings.recording.motion_blur.enabled = false;
            settings.playfield.background.load_storyboards = true;
            settings.playfield.background.load_videos = true;
            settings.gameplay.show_results_screen = true;
            settings.gameplay.score_board.show = true;
            settings.gameplay.hit_error_meter.show = true;
            settings.gameplay.strain_graph.show = true;
            settings.gameplay.key_overlay.show = true;
            "Standard 1080p60"
        }
        PresetChoice::CleanRecord => {
            settings.recording.frame_width = 1920;
            settings.recording.frame_height = 1080;
            settings.recording.fps = 60;
            settings.gameplay.show_results_screen = false;
            settings.gameplay.score_board.show = false;
            settings.gameplay.hit_error_meter.show = false;
            settings.gameplay.aim_error_meter.show = false;
            settings.gameplay.strain_graph.show = false;
            settings.gameplay.key_overlay.show = false;
            "Clean Showcase"
        }
        PresetChoice::Performance720p => {
            settings.recording.frame_width = 1280;
            settings.recording.frame_height = 720;
            settings.recording.fps = 60;
            settings.recording.motion_blur.enabled = false;
            settings.playfield.background.load_storyboards = false;
            settings.playfield.background.load_videos = false;
            "Fast 720p60"
        }
        PresetChoice::CinematicBlur => {
            settings.recording.frame_width = 1920;
            settings.recording.frame_height = 1080;
            settings.recording.fps = 60;
            settings.recording.motion_blur.enabled = true;
            settings.gameplay.show_results_screen = true;
            "Cinematic Motion Blur"
        }
    }
}

pub async fn preset(
    ctx: Arc<Context>,
    command: InteractionCommand,
    args: SettingsPreset,
) -> Result<()> {
    let author = command.user_id()?;
    let (user_path, mut settings) = load_or_init_user_settings(author, command.id).await?;

    let label = apply_preset(&mut settings, args.preset);

    if let Err(err) = save_user_settings(author, command.id, &user_path, &settings).await {
        let _ = command
            .error_callback(&ctx, "Failed to apply settings preset", false)
            .await;
        return Err(err);
    }

    let user = command.user()?;
    let embed = create_settings_embed(user, &settings);
    let builder = MessageBuilder::new()
        .content(format!("Applied preset **{label}**!"))
        .embed(embed);
    command.callback(&ctx, builder, false).await?;

    Ok(())
}
