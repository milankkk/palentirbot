use std::path::PathBuf;
use std::sync::Arc;

use super::queue::send_queue_status;
use crate::util::ChannelExt;
use crate::{
    core::{replay_queue::ReplaySlim, BotConfig, Context, ReplayData, TimePoints},
    util::{
        builder::MessageBuilder, interaction::InteractionCommand, Authored, InteractionCommandExt,
    },
};
use command_macros::msg_command;
use eyre::{Context as _, Report, Result};
use osu_db::Replay;
use rosu_v2::prelude::Score;
use twilight_interactions::command::CommandInputData;
use twilight_model::{
    channel::{message::embed::Embed, Message},
    id::{
        marker::{ChannelMarker, UserMarker},
        Id,
    },
    util::Timestamp,
};

/// Helper to sanitize filenames so slashes and special characters don't break file paths
pub fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other => other,
        })
        .collect()
}

async fn find_score_to_render(
    ctx: &Context,
    user_id: u32,
    beatmap_id: u32,
    timestamp: Option<Timestamp>,
) -> Result<Option<Score>> {
    if let Some(timestamp) = timestamp {
        let ts_unix = timestamp.as_secs();

        // check recents
        let recent_scores = ctx
            .osu()
            .user_scores(user_id)
            .recent()
            .include_fails(false)
            .limit(100)
            .await
            .context("failed to get recent scores")?;

        if let Some(score) = recent_scores
            .into_iter()
            .find(|score| (score.ended_at.unix_timestamp() - ts_unix).abs() <= 3)
        {
            return Ok(Some(score));
        }

        // check tops
        let top_scores = ctx
            .osu()
            .user_scores(user_id)
            .best()
            .limit(100)
            .await
            .context("failed to get top scores")?;

        if let Some(score) = top_scores
            .into_iter()
            .find(|score| (score.ended_at.unix_timestamp() - ts_unix).abs() <= 3)
        {
            return Ok(Some(score));
        }

        tracing::warn!(
            "Fetching map scores for user {} on beatmap {}",
            user_id,
            beatmap_id
        );
        let map_scores = ctx
            .osu()
            .beatmap_user_scores(beatmap_id, user_id)
            .await
            .context("failed to get map scores")?;
        tracing::warn!("Got {} map scores", map_scores.len());
        let score_opt = map_scores
            .iter()
            .find(|score| (score.ended_at.unix_timestamp() - ts_unix).abs() <= 3)
            .cloned()
            .or_else(|| map_scores.into_iter().next());

        Ok(score_opt)
    } else {
        // No timestamp (<current embed) — pick best score on this map
        tracing::warn!(
            user_id,
            beatmap_id,
            "no timestamp, fetching beatmap user scores"
        );

        let map_scores = ctx
            .osu()
            .beatmap_user_scores(beatmap_id, user_id)
            .await
            .context("failed to get user scores on beatmap")?;

        Ok(map_scores.into_iter().next())
    }
}

#[msg_command(name = "Render score", dm_permission = false)]
async fn render_from_msg(ctx: Arc<Context>, mut command: InteractionCommand) -> Result<()> {
    let guild_id = match command.guild_id {
        Some(guild) => {
            let (blacklisted, reason_opt) = match ctx.psql()._is_server_blacklisted(guild).await {
                Ok(result) => result,
                Err(err) => {
                    tracing::warn!("{err:?}");
                    command
                        .error_callback(
                            &ctx,
                            "Failed to check server status, try again later.",
                            false,
                        )
                        .await?;
                    return Ok(());
                }
            };
            if blacklisted {
                let mut content = String::from("Seems like this server has been blacklisted.");
                if let Some(reason) = reason_opt {
                    use std::fmt::Write as _;
                    let _ = write!(content, " {reason}");
                }
                command.error_callback(&ctx, content, false).await?;
                return Ok(());
            }

            let is_allowed = ctx
                .guild_settings(guild, |s| {
                    s.input_channels.is_empty() || s.input_channels.contains(&command.channel_id)
                })
                .unwrap_or(true);

            if !is_allowed {
                let content = "This channel is not an allowed input channel for rendering.";
                command.error_callback(&ctx, content, true).await?;
                return Ok(());
            }

            guild
        }
        None => {
            let content = "This command cannot be used in DMs.";
            command.error_callback(&ctx, content, true).await?;
            return Ok(());
        }
    };

    let input_data = command.input_data();

    let (user_id, beatmap_id, timestamp) = match parseembedinputdata(input_data) {
        Some(ParsedEmbed {
            user_id,
            beatmap_id,
            timestamp,
        }) => (user_id, beatmap_id, timestamp),
        None => {
            let content = "The command can only be used on Bathbot.rs embeds!";
            command.error(&ctx, content).await?;
            return Ok(());
        }
    };

    let score_to_render = match find_score_to_render(&ctx, user_id, beatmap_id, timestamp).await? {
        Some(score) => score,
        None => {
            let content = "Couldn't find the replay for this score (not in recents, tops, or map bests).";
            command.error(&ctx, content).await?;
            return Ok(());
        }
    };

    let score_id = score_to_render.id;

    let mut replay_bytes_res = ctx.osu().replay_raw(score_to_render.id).await;
    if replay_bytes_res.is_err() {
        if let Some(legacy_id) = score_to_render.legacy_score_id {
            replay_bytes_res = ctx.osu().replay_raw(legacy_id).mode(score_to_render.mode).await;
        }
    }
    
    let replay_bytes = match replay_bytes_res {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(?err, score_id, "failed to download replay for msg-derived render");
            let _ = command.error(&ctx, "Couldn't download the replay for this score. The osu! API says 'Replay not available'.").await;
            return Ok(());
        }
    };

    let fetched_user = ctx.osu().user(user_id).await.ok();
    let fetched_map = ctx.osu().beatmap().map_id(beatmap_id).await.ok();

    let osu_user = score_to_render
        .user
        .as_ref()
        .map(|user| user.username.as_str())
        .or_else(|| fetched_user.as_ref().map(|u| u.username.as_str()))
        .unwrap_or("unknown user");

    let map_title = score_to_render
        .mapset
        .as_ref()
        .map(|mapset| mapset.title.as_str())
        .or_else(|| fetched_map.as_ref().and_then(|m| m.mapset.as_ref().map(|ms| ms.title.as_str())))
        .unwrap_or("unknown map");

    let diff_name = score_to_render
        .map
        .as_ref()
        .map(|map| map.version.as_str())
        .or_else(|| fetched_map.as_ref().map(|m| m.version.as_str()))
        .unwrap_or("unknown diff");

    let mut path = BotConfig::get().paths.downloads().to_owned();
    let safe_user = sanitize_filename(osu_user);
    let safe_title = sanitize_filename(map_title);
    let safe_diff = sanitize_filename(diff_name);
    path.push(format!("{}_{safe_user} - {safe_title} [{safe_diff}].osr", command.id));

    tokio::fs::write(&path, &replay_bytes).await.context("failed to write into replay file")?;

    let mut replay = match Replay::from_bytes(&replay_bytes) {
        Ok(replay) => ReplaySlim::from(replay),
        Err(err) => {
            let content = "Failed to parse replay";
            let _ = command.error(&ctx, content).await;

            return Err(Report::new(err).wrap_err("failed to parse replay"));
        }
    };
    replay.grade = score_to_render.grade;
    let input_channel = command.channel_id;
    let user = command.user_id()?;

    let output_channel = ctx
        .guild_settings(guild_id, |server| server.output_channel)
        .flatten()
        .unwrap_or(input_channel);
    let builder = MessageBuilder::new().embed("Replay has been pushed to the queue!");
    let queue_msg = command.update(&ctx, &builder).await?;
    let replay_data = ReplayData {
        input_channel,
        output_channel,
        pitch: None,
        path,
        replay,
        user,
        time_points: TimePoints { start: 0, end: 0 },
        title: None,
        player_name: Some(osu_user.to_string()),
        map_title: Some(map_title.to_string()),
        difficulty_name: Some(diff_name.to_string()),
        queue_message: Some((queue_msg.id, queue_msg.channel_id)),
        is_lazer: score_to_render.set_on_lazer,
    };

    let was_empty = ctx.replay_queue.queue.lock().await.is_empty();
    ctx.replay_queue.push(replay_data).await;

    if was_empty {
        let ctx_clone = Arc::clone(&ctx);
        tokio::spawn(async move {
            let _ = send_queue_status(ctx_clone, output_channel).await;
        });
    }
    Ok(())
}

pub async fn render_score_from_message(
    ctx: Arc<Context>,
    message: &Message,
    input_channel: Id<ChannelMarker>,
    output_channel: Id<ChannelMarker>,
    user: Id<UserMarker>,
) -> eyre::Result<Option<PathBuf>> {
    let Some(embed) = message.embeds.first() else {
        return Ok(None);
    };

    render_score_from_embed(ctx, embed, input_channel, output_channel, user).await
}

pub async fn render_score_from_embed(
    ctx: Arc<Context>,
    embed: &Embed,
    input_channel: Id<ChannelMarker>,
    output_channel: Id<ChannelMarker>,
    user: Id<UserMarker>,
) -> eyre::Result<Option<PathBuf>> {
    tracing::warn!("render_score_from_embed: entered for map/user");

    let Some(ParsedEmbed {
        user_id,
        beatmap_id,
        timestamp,
    }) = parse_embed(embed)
    else {
        tracing::warn!("render_score_from_embed: parse_embed returned None");
        return Ok(None);
    };

    let score_to_render = match find_score_to_render(&ctx, user_id, beatmap_id, timestamp).await? {
        Some(s) => s,
        None => {
            let content = "Couldn't find the replay for this score (not in recents, tops, or map bests).";
            let _ = input_channel.error(&ctx, content).await;
            return Ok(None);
        }
    };

    let mut replay_bytes_res = ctx.osu().replay_raw(score_to_render.id).await;
    if replay_bytes_res.is_err() {
        if let Some(legacy_id) = score_to_render.legacy_score_id {
            replay_bytes_res = ctx.osu().replay_raw(legacy_id).mode(score_to_render.mode).await;
        }
    }
    let replay_bytes = match replay_bytes_res {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(
                ?err,
                score_id = score_to_render.id,
                "failed to download replay for embed-derived render"
            );
            return Ok(None);
        }
    };

    let mut replay = match Replay::from_bytes(&replay_bytes) {
        Ok(replay) => ReplaySlim::from(replay),
        Err(err) => return Err(Report::new(err).wrap_err("failed to parse replay")),
    };
    replay.grade = score_to_render.grade;
    let fetched_user = ctx.osu().user(user_id).await.ok();
    let fetched_map = ctx.osu().beatmap().map_id(beatmap_id).await.ok();

    let osu_user = score_to_render
        .user
        .as_ref()
        .map(|user| user.username.as_str())
        .or_else(|| fetched_user.as_ref().map(|u| u.username.as_str()))
        .unwrap_or("unknown user");

    let map_title = score_to_render
        .mapset
        .as_ref()
        .map(|mapset| mapset.title.as_str())
        .or_else(|| fetched_map.as_ref().and_then(|m| m.mapset.as_ref().map(|ms| ms.title.as_str())))
        .unwrap_or("unknown map");

    let diff_name = score_to_render
        .map
        .as_ref()
        .map(|map| map.version.as_str())
        .or_else(|| fetched_map.as_ref().map(|m| m.version.as_str()))
        .unwrap_or("unknown diff");

    let mut path = BotConfig::get().paths.downloads().to_owned();
    let safe_user = sanitize_filename(osu_user);
    let safe_title = sanitize_filename(map_title);
    let safe_diff = sanitize_filename(diff_name);
    path.push(format!("{}_{}_{safe_user} - {safe_title} [{safe_diff}].osr", user, score_to_render.id));

    tokio::fs::write(&path, replay_bytes).await.context("failed to write replay file")?;

    ctx.replay_queue
        .push(ReplayData {
            input_channel,
            output_channel,
            pitch: None,
            path: path.clone(),
            replay: replay,
            time_points: TimePoints { start: 0, end: 0 },
            user,
            title: None,
            player_name: Some(osu_user.to_string()),
            map_title: Some(map_title.to_string()),
            difficulty_name: Some(diff_name.to_string()),
            queue_message: None,
            is_lazer: score_to_render.set_on_lazer,
        })
        .await;

    Ok(Some(path))
}

struct ParsedEmbed {
    user_id: u32,
    beatmap_id: u32,
    timestamp: Option<Timestamp>,
}

// fn parse_embed(embed: &Embed) -> Option<ParsedEmbed> {
//     let user_url = embed.author.as_ref().and_then(|a| a.url.as_ref())?;
//     let user_id = user_url.split('/').nth(4).and_then(|id| id.parse::<u32>().ok())?;

//     let beatmap_url = embed.url.as_ref()?;
//     let beatmap_id = beatmap_url
//         .split('/')
//         .last()
//         .and_then(|id| id.parse::<u32>().ok())?;

//     let timestamp = embed
//         .timestamp
//         .clone()
//         .or_else(|| get_timestamp_from_minimized_embed(embed))?;

//     Some(ParsedEmbed {
//         user_id,
//         beatmap_id,
//         timestamp,
//     })
// }

fn parse_embed(embed: &Embed) -> Option<ParsedEmbed> {
    let user_url = embed.author.as_ref().and_then(|a| a.url.as_ref())?;
    let user_id = user_url
        .split('/')
        .nth(4)
        .and_then(|id| id.parse::<u32>().ok())?;

    let beatmap_url = embed.url.as_ref()?;
    let beatmap_id = if beatmap_url.contains("/b/") {
        // Short format: https://osu.ppy.sh/b/442905
        beatmap_url
            .split("/b/")
            .nth(1)
            .and_then(|id| id.parse::<u32>().ok())?
    } else if beatmap_url.contains("/beatmaps/") {
        // https://osu.ppy.sh/beatmaps/442905
        beatmap_url
            .split('/')
            .last()
            .and_then(|id| id.parse::<u32>().ok())?
    } else if beatmap_url.contains("/beatmapsets/") {
        if let Some(fragment) = beatmap_url.split('#').nth(1) {
            // https://osu.ppy.sh/beatmapsets/123#osu/442905
            fragment
                .split('/')
                .last()
                .and_then(|id| id.parse::<u32>().ok())?
        } else {
            tracing::warn!(
                author_url = ?embed.author.as_ref().and_then(|a| a.url.as_ref()),
                embed_url = ?embed.url,
                embed_timestamp = ?embed.timestamp,
                fields = ?embed.fields,
                "parse_embed failed, dumping embed structure"
            );
            return None;
        }
    } else {
        tracing::warn!(
            author_url = ?embed.author.as_ref().and_then(|a| a.url.as_ref()),
            embed_url = ?embed.url,
            embed_timestamp = ?embed.timestamp,
            fields = ?embed.fields,
            "parse_embed failed, dumping embed structure"
        );
        return None;
    };

    let timestamp = embed
        .timestamp
        .clone()
        .or_else(|| get_timestamp_from_minimized_embed(embed));

    Some(ParsedEmbed {
        user_id,
        beatmap_id,
        timestamp,
    })
}

fn get_timestamp_from_minimized_embed(embed: &Embed) -> Option<Timestamp> {
    let field = embed.fields.first()?;

    let discord_timestamp = field.name.rsplit('\t').next()?;

    let actual_timestamp_value = discord_timestamp
        .trim_start_matches("<t:")
        .trim_end_matches(":R>");

    let timestamp_value_as_int = actual_timestamp_value.parse().ok()?;

    Timestamp::from_secs(timestamp_value_as_int).ok()
}

fn parseembedinputdata(input_: CommandInputData<'_>) -> Option<ParsedEmbed> {
    let embed = input_
        .resolved
        .as_ref()
        .and_then(|resolved| resolved.messages.values().next())
        .and_then(|msg| msg.embeds.first())?;

    parse_embed(embed)
}
