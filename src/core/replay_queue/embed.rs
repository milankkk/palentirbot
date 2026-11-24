use std::sync::Arc;

use tokio::time::{Duration, Instant};
use twilight_model::{
    channel::message::{
        component::{Component, SelectMenuOption},
        embed::{Embed, EmbedField},
    },
    id::{
        marker::{ChannelMarker, MessageMarker},
        Id,
    },
};

use crate::{
    core::{BotConfig, Context},
    util::{
        builder::{text_select_row, EmbedBuilder, MessageBuilder},
        str::truncate_with_ellipsis,
        MessageExt,
    },
};

use super::{
    adaptive::{pp_select_timeout_secs, show_pp_select_menu, AdaptiveRenderPlan},
    data::ReplaySlim,
    mapset::{resolve_and_ensure_beatmap, BeatmapResolveError, ResolvedBeatmap},
    pp::{create_title, TitleResult},
    PpTriple,
};

pub(crate) struct ReplayInfoEmbed {
    pub title: String,
    pub embed: Embed,
    pub pp_select_row: Option<Component>,
}

pub(crate) fn build_replay_info_embed(
    replay: &ReplaySlim,
    map_id: u32,
    title_result: &TitleResult,
    adaptive_plan: Option<&AdaptiveRenderPlan>,
) -> ReplayInfoEmbed {
    use crate::util::numbers::with_comma_int;

    let acc = replay.accuracy();
    let emojis = &BotConfig::get().emojis;
    let score_value = with_comma_int(replay.score as u64).to_string();
    let rank_value = emojis.grade_emoji(replay.grade);

    let max_possible_combo = title_result.max_possible_combo;
    let is_perfect_fc = u32::from(replay.max_combo) == max_possible_combo && replay.count_miss == 0;
    let replay_datetime = replay
        .timestamp
        .and_then(|ts| time::OffsetDateTime::from_unix_timestamp(ts).ok());

    let active_pp = title_result.pp_latest.unwrap_or(title_result.pp_v4);
    let embed_description = format!(
        "{rank_value}  **{score_value}**  •  **{acc}%**  •  **{}/{}x**\n{} {}  {} {}  {} {}  {} {}",
        replay.max_combo,
        max_possible_combo,
        emojis.hit_300,
        replay.count_300,
        emojis.hit_100,
        replay.count_100,
        emojis.hit_50,
        replay.count_50,
        emojis.hit_miss,
        replay.count_miss,
    );

    let pp_str = format!("{:.2}", active_pp.pp);
    let max_pp_str = format!("{:.2}", active_pp.max_pp);
    let nochoke_pp_str = format!("{:.2}", active_pp.nochoke_pp);
    let breakdown_strs = match (active_pp.aim_pp, active_pp.speed_pp, active_pp.acc_pp) {
        (Some(aim), Some(speed), Some(acc_val)) => Some((
            format!("{aim:.1}"),
            format!("{speed:.1}"),
            format!("{acc_val:.1}"),
        )),
        _ => None,
    };
    let pp_field_value = format_pp_field(
        &pp_str,
        &max_pp_str,
        &nochoke_pp_str,
        is_perfect_fc,
        breakdown_strs
            .as_ref()
            .map(|(a, s, c)| (a.as_str(), s.as_str(), c.as_str())),
        None,
    );
    let embed_fields = vec![EmbedField {
        inline: false,
        name: "PP".to_owned(),
        value: pp_field_value,
    }];

    let embed_title = truncate_with_ellipsis(&title_result.title, 256, "...").into_owned();

    let footer_text = match adaptive_plan {
        Some(plan) => format!(
            "Played • Adaptive {}p{} ({:.1} Mbps)",
            plan.height,
            plan.fps,
            f64::from(plan.video_kbps) / 1000.0
        ),
        None => "Played".to_owned(),
    };

    let mut embed_builder = EmbedBuilder::new()
        .color(0x96DFE3)
        .title(embed_title)
        .url(format!("https://osu.ppy.sh/beatmaps/{map_id}"))
        .description(embed_description)
        .fields(embed_fields)
        .footer(footer_text);
    if let Some(dt) = replay_datetime {
        embed_builder = embed_builder.timestamp(dt);
    }

    let embed = embed_builder.build();

    let mut options = Vec::new();
    let mut push_opt = |id: &str, label: &str, desc: &str, t: PpTriple, is_default: bool| {
        let mut value = format!(
            "{id}|{is_perfect_fc}|{:.2}|{:.2}|{:.2}",
            t.pp, t.max_pp, t.nochoke_pp
        );
        if value.len() > 100 {
            return;
        }
        if let (Some(aim), Some(speed), Some(acc_val)) = (t.aim_pp, t.speed_pp, t.acc_pp) {
            let ext = format!("{value}|{aim:.1}|{speed:.1}|{acc_val:.1}");
            if ext.len() <= 100 {
                value = ext;
            }
        }
        options.push(SelectMenuOption {
            default: is_default,
            description: Some(desc.to_owned()),
            emoji: None,
            label: label.to_owned(),
            value,
        });
    };

    if let Some(lat) = title_result.pp_latest {
        push_opt(
            "pp_lat",
            "Latest Rework (C# backend)",
            "Real-time official osu! backend calculations",
            lat,
            true,
        );
    }
    push_opt(
        "pp_v4",
        "rosu-pp v4 (July 2026)",
        "Performance points & star rating updates",
        title_result.pp_v4,
        title_result.pp_latest.is_none(),
    );
    if let Some(t) = title_result.pp_2025 {
        push_opt("pp_v25", "March 2025 Rework", "March 2025 Rework", t, false);
    }
    if let Some(t) = title_result.pp_2024 {
        push_opt(
            "pp_v24",
            "October 2024 Combo Scaling Removal",
            "Removal of combo scaling from standard PP",
            t,
            false,
        );
    }
    if let Some(t) = title_result.pp_2022 {
        push_opt(
            "pp_v22",
            "September 2022 Rework",
            "Major performance & difficulty changes",
            t,
            false,
        );
    }
    if let Some(t) = title_result.pp_2021 {
        push_opt(
            "pp_v21",
            "November 2021 Xexxar Rework",
            "Rhythm complexity & aim rework",
            t,
            false,
        );
    }

    let pp_select_row = (show_pp_select_menu() && options.len() > 1)
        .then(|| text_select_row("pp_version_select", "Select PP Version", options));

    ReplayInfoEmbed {
        title: title_result.title.clone(),
        embed,
        pp_select_row,
    }
}

pub(crate) fn spawn_pp_select_expiry(
    ctx: Arc<Context>,
    channel_id: Id<ChannelMarker>,
    embed_msg_id: Id<MessageMarker>,
    remaining_components: Vec<Component>,
) {
    let pp_timeout_secs = pp_select_timeout_secs();
    if pp_timeout_secs == 0 {
        return;
    }
    tokio::spawn(async move {
        crate::core::events::interaction::wait_idle_expiry(
            Instant::now(),
            Duration::from_secs(pp_timeout_secs),
            Duration::from_secs(60),
            Duration::from_secs(pp_timeout_secs.saturating_mul(2).max(300)),
            || crate::core::events::interaction::last_pp_select_activity(embed_msg_id),
        )
        .await;

        crate::core::events::interaction::clear_pp_select(embed_msg_id);
        let builder = MessageBuilder::new().components(remaining_components);
        if let Err(err) = (embed_msg_id, channel_id).update(&ctx, &builder).await {
            tracing::debug!(?err, msg_id = %embed_msg_id, "failed to remove expired PP dropdown");
        }
    });
}

/// Download/verify the beatmap for a replay, calculate PP & difficulty, and build the score info embed.
pub(crate) async fn analyze_replay_for_info(
    ctx: &Context,
    replay: &ReplaySlim,
    is_lazer: bool,
) -> std::result::Result<ReplayInfoEmbed, String> {
    let ResolvedBeatmap {
        map_id,
        map_version,
        mapset_title,
        osu_path,
        osu_bytes,
        ..
    } = resolve_and_ensure_beatmap(ctx, replay.beatmap_hash.as_deref(), false, None)
        .await
        .map_err(|err| match err {
            BeatmapResolveError::Failure { user_msg, .. } => user_msg.to_owned(),
            BeatmapResolveError::Cancelled => "Cancelled.".to_owned(),
        })?;

    let title_for_display = format!("{mapset_title} [{map_version}]");
    let title_result = create_title(
        replay,
        osu_path,
        Arc::from(osu_bytes),
        &title_for_display,
        is_lazer,
        true,
    )
    .await
    .map_err(|err| {
        warn!(?err, "failed to analyze replay PP/title");
        "There was an error while analyzing the replay file.".to_owned()
    })?;

    Ok(build_replay_info_embed(replay, map_id, &title_result, None))
}

pub(crate) fn pp_version_label(id: &str) -> Option<&'static str> {
    match id {
        "pp_v4" => Some("rosu-pp v4"),
        "pp_v25" => Some("March 2025"),
        "pp_v24" => Some("Oct 2024"),
        "pp_v22" => Some("Sept 2022"),
        "pp_v21" => Some("Nov 2021"),
        _ => None,
    }
}

pub(crate) fn format_pp_field(
    pp: &str,
    max_pp: &str,
    nochoke_pp: &str,
    is_perfect_fc: bool,
    breakdown: Option<(&str, &str, &str)>,
    version_tag: Option<&str>,
) -> String {
    let base = if is_perfect_fc {
        format!("**{pp}pp** / {max_pp}pp")
    } else {
        format!("**{pp}pp** (FC: **{nochoke_pp}pp**) / {max_pp}pp")
    };
    let mut out = match breakdown {
        Some((aim, speed, acc)) => {
            format!("{base}\nAim `{aim}` • Speed `{speed}` • Acc `{acc}`")
        }
        None => base,
    };
    if let Some(tag) = version_tag {
        out.push_str(&format!("  •  *{tag}*"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::format_pp_field;

    #[test]
    fn test_format_pp_field() {
        assert_eq!(
            format_pp_field(
                "512.34",
                "600.00",
                "512.34",
                true,
                Some(("250.1", "210.2", "52.0")),
                None
            ),
            "**512.34pp** / 600.00pp\nAim `250.1` • Speed `210.2` • Acc `52.0`"
        );
        assert_eq!(
            format_pp_field(
                "400.00",
                "550.00",
                "480.00",
                false,
                None,
                Some("March 2025")
            ),
            "**400.00pp** (FC: **480.00pp**) / 550.00pp  •  *March 2025*"
        );
    }
}
