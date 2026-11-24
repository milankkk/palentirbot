use std::{convert::TryFrom, sync::Arc};

use command_macros::SlashCommand;
use eyre::Result;
use twilight_interactions::command::{AutocompleteValue, CommandModel, CreateCommand};
use twilight_model::channel::Attachment;

use super::render_score::{
    check_render_access, check_user_queue_limit, enqueue_replay_interaction, parse_slash_score_url,
    resolve_optional_skin, resolve_slash_replay_input,
};
use super::settings::autocomplete_skins;
use crate::{
    core::{replay_queue::RenderOptions, Context, TimePoints},
    util::{interaction::InteractionCommand, Authored, InteractionCommandExt},
};

#[derive(CreateCommand, SlashCommand)]
#[command(name = "render")]
#[flags(SKIP_DEFER)]
/// Render a replay file or osu! score URL and upload it
pub struct Render {
    #[command(rename = "replay")]
    /// Specify the replay through a .osr file
    attachment: Option<Attachment>,
    /// Paste an osu! score URL (e.g. https://osu.ppy.sh/scores/...) or score ID
    score_url: Option<String>,
    /// Specify a start timestamp in seconds or MM:SS
    start: Option<String>,
    /// Specify an end timestamp in seconds or MM:SS
    end: Option<String>,
    #[command(min_value = 0.5, max_value = 5.0)]
    /// Specify the music pitch (Nightcore is 1.5)
    pitch: Option<f64>,
    #[command(autocomplete = true)]
    /// Temporarily override skin for this render
    skin: Option<String>,
    /// Skip straight to map drain time (intro skip)
    skip_intro: Option<bool>,
    #[command(min_value = -5000, max_value = 5000)]
    /// Local audio offset override in ms
    offset: Option<i64>,
    /// Bypass the render cache and force a fresh render
    force: Option<bool>,
}

#[derive(CommandModel)]
#[command(autocomplete = true)]
pub struct RenderAutocomplete {
    #[command(rename = "replay")]
    attachment: Option<Attachment>,
    score_url: Option<String>,
    start: Option<String>,
    end: Option<String>,
    pitch: Option<f64>,
    skin: AutocompleteValue<String>,
    skip_intro: Option<bool>,
    offset: Option<i64>,
    force: Option<bool>,
}

impl TryFrom<RenderAutocomplete> for Render {
    type Error = String;

    fn try_from(args: RenderAutocomplete) -> std::result::Result<Self, Self::Error> {
        let skin = match args.skin {
            AutocompleteValue::Focused(focused) => return Err(focused),
            AutocompleteValue::None => None,
            AutocompleteValue::Completed(val) => Some(val),
        };

        Ok(Self {
            attachment: args.attachment,
            score_url: args.score_url,
            start: args.start,
            end: args.end,
            pitch: args.pitch,
            skin,
            skip_intro: args.skip_intro,
            offset: args.offset,
            force: args.force,
        })
    }
}

pub async fn slash_render(ctx: Arc<Context>, mut command: InteractionCommand) -> Result<()> {
    let raw_args = RenderAutocomplete::from_interaction(command.input_data())?;
    let Render {
        attachment,
        score_url,
        start,
        end,
        pitch,
        skin,
        skip_intro,
        offset,
        force,
    } = match Render::try_from(raw_args) {
        Ok(args) => args,
        Err(focused) => {
            let choices = autocomplete_skins(&ctx, &focused)?;
            command.autocomplete(&ctx, choices).await?;
            return Ok(());
        }
    };

    if attachment.is_none() && score_url.is_none() {
        command
            .error_callback(
                &ctx,
                "Please attach a `.osr` replay file or provide an osu! `score_url`.",
                true,
            )
            .await?;
        return Ok(());
    }

    if let Some(p) = pitch {
        if !p.is_finite() || p <= 0.0 {
            command
                .error_callback(&ctx, "Pitch must be a positive number!", true)
                .await?;
            return Ok(());
        }
    }

    if let Some(ref att) = attachment {
        if !matches!(att.filename.rsplit('.').next(), Some(ext) if ext.eq_ignore_ascii_case("osr")) {
            command
                .error_callback(&ctx, "The attachment must be a .osr file!", true)
                .await?;
            return Ok(());
        }
    }

    let parsed_score_id = match parse_slash_score_url(score_url.as_deref(), attachment.is_some()) {
        Ok(parsed) => parsed,
        Err(msg) => {
            command.error_callback(&ctx, msg, true).await?;
            return Ok(());
        }
    };

    let start_in_seconds = match start {
        Some(ref s) => match TimePoints::parse_single(s) {
            Ok(v) => v,
            Err(err) => {
                command.error_callback(&ctx, err, true).await?;
                return Ok(());
            }
        },
        None => 0,
    };

    let end_in_seconds = match end {
        Some(ref s) => match TimePoints::parse_single(s) {
            Ok(v) => v,
            Err(err) => {
                command.error_callback(&ctx, err, true).await?;
                return Ok(());
            }
        },
        None => 0,
    };

    if end_in_seconds > 0 && end_in_seconds <= start_in_seconds {
        command
            .error_callback(
                &ctx,
                "`end` timestamp must be greater than `start` timestamp.",
                true,
            )
            .await?;
        return Ok(());
    }

    let resolved_skin = match resolve_optional_skin(
        &ctx,
        skin.as_deref(),
        "Use `/skin list` to browse skins.",
    ) {
        Ok(name) => name,
        Err(msg) => {
            command.error_callback(&ctx, msg, true).await?;
            return Ok(());
        }
    };

    command.defer(&ctx, false).await?;

    let user = command.user_id()?;
    if let Err(msg) = check_render_access(&ctx, user, command.guild_id, command.channel_id).await {
        command.error(&ctx, msg).await?;
        return Ok(());
    }

    if let Err(msg) = check_user_queue_limit(&ctx, user).await {
        command.error(&ctx, msg).await?;
        return Ok(());
    }

    let opts = RenderOptions {
        start: start_in_seconds,
        end: end_in_seconds,
        pitch,
        skin: resolved_skin,
        skip_intro: skip_intro.unwrap_or(false),
        offset_ms: offset.map(|v| v.clamp(-5000, 5000) as i32),
        force_rerender: force.unwrap_or(false),
        skip_info_embed: false,
    };

    let replay_data = match resolve_slash_replay_input(
        &ctx,
        attachment.as_ref(),
        parsed_score_id,
        command.id,
        command.channel_id,
        user,
        opts,
    )
    .await
    {
        Ok(data) => data,
        Err(err_msg) => {
            command.error(&ctx, err_msg).await?;
            return Ok(());
        }
    };

    enqueue_replay_interaction(ctx, &command, replay_data, None).await;
    Ok(())
}
