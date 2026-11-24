use std::{
    collections::HashMap,
    mem,
    sync::{Arc, Mutex, OnceLock},
};

use eyre::Context as _;
use tokio::time::{sleep_until, Duration, Instant};
use twilight_model::{
    channel::message::component::Component,
    id::{marker::MessageMarker, Id},
};

use crate::{
    commands::help::{handle_help_basecommand, handle_help_subcommand},
    core::{
        commands::checks::check_authority_with_perms,
        replay_queue::{
            process::{format_pp_field, pp_version_label},
            CancelResult,
        },
        Context,
    },
    pagination::components::*,
    util::{
        builder::MessageBuilder, hasher::IntBuildHasher, interaction::InteractionComponent,
        Authored, ComponentExt,
    },
};

/// Default idle timeout before interactive buttons/menus are removed from embeds (90s).
pub(crate) const COMPONENT_IDLE_TIMEOUT_SECS: u64 = 90;
const COMPONENT_EXTEND_SECS: u64 = 60;
const COMPONENT_HARD_CAP_SECS: u64 = 300;

struct ActivityMap(OnceLock<Mutex<HashMap<Id<MessageMarker>, Instant, IntBuildHasher>>>);

impl ActivityMap {
    const fn new() -> Self {
        Self(OnceLock::new())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Id<MessageMarker>, Instant, IntBuildHasher>> {
        self.0
            .get_or_init(|| Mutex::new(HashMap::with_hasher(IntBuildHasher)))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn insert(&self, id: Id<MessageMarker>, now: Instant, prune_older_than: Option<Duration>) {
        let mut map = self.lock();
        if let Some(max_age) = prune_older_than {
            map.retain(|_, ts| now.saturating_duration_since(*ts) < max_age);
        }
        map.insert(id, now);
    }

    fn touch_existing(&self, id: Id<MessageMarker>) {
        if let Some(last) = self.lock().get_mut(&id) {
            *last = Instant::now();
        }
    }

    fn get(&self, id: Id<MessageMarker>) -> Option<Instant> {
        self.lock().get(&id).copied()
    }

    fn remove(&self, id: Id<MessageMarker>) {
        self.lock().remove(&id);
    }
}

static PP_SELECT_ACTIVITY: ActivityMap = ActivityMap::new();
static INTERACTION_EXPIRY: ActivityMap = ActivityMap::new();

pub(crate) fn touch_pp_select(msg_id: Id<MessageMarker>) {
    PP_SELECT_ACTIVITY.insert(msg_id, Instant::now(), None);
}

pub(crate) fn last_pp_select_activity(msg_id: Id<MessageMarker>) -> Option<Instant> {
    PP_SELECT_ACTIVITY.get(msg_id)
}

pub(crate) fn clear_pp_select(msg_id: Id<MessageMarker>) {
    PP_SELECT_ACTIVITY.remove(msg_id);
}

pub(crate) fn touch_interaction_component_expiry(msg_id: Id<MessageMarker>) {
    INTERACTION_EXPIRY.touch_existing(msg_id);
}

pub(crate) fn clear_interaction_component_expiry(msg_id: Id<MessageMarker>) {
    INTERACTION_EXPIRY.remove(msg_id);
}

/// Sleep until an interactive message goes idle (extending on activity up to `hard_cap`).
/// Returns `true` when the idle/cap timer elapsed, or `false` if the entry was cleared early.
pub(crate) async fn wait_idle_expiry(
    start: Instant,
    idle: Duration,
    extend: Duration,
    hard_cap_dur: Duration,
    mut get_last_activity: impl FnMut() -> Option<Instant>,
) -> bool {
    let hard_cap = start + hard_cap_dur;
    let mut prev_check = start;
    let mut deadline = start + idle;

    loop {
        sleep_until(deadline).await;
        let Some(last_act) = get_last_activity() else {
            return false;
        };
        if last_act > prev_check && last_act < hard_cap {
            prev_check = last_act;
            let extended = (last_act + extend).min(hard_cap);
            if extended > deadline {
                deadline = extended;
                continue;
            }
        }
        return true;
    }
}

/// Strip interactive components from an interaction response (public or ephemeral)
/// after `COMPONENT_IDLE_TIMEOUT_SECS` (90s) of inactivity (extended +60s per click, 5m cap).
pub(crate) fn spawn_interaction_component_expiry(ctx: Arc<Context>, token: String) {
    tokio::spawn(async move {
        let start = Instant::now();
        let msg_id = match ctx.interaction().response(&token).await {
            Ok(resp) => resp.model().await.ok().map(|m| m.id),
            Err(_) => None,
        };

        if let Some(id) = msg_id {
            INTERACTION_EXPIRY.insert(
                id,
                start,
                Some(Duration::from_secs(COMPONENT_HARD_CAP_SECS + 120)),
            );
        }

        if let Some(id) = msg_id {
            let expired = wait_idle_expiry(
                start,
                Duration::from_secs(COMPONENT_IDLE_TIMEOUT_SECS),
                Duration::from_secs(COMPONENT_EXTEND_SECS),
                Duration::from_secs(COMPONENT_HARD_CAP_SECS),
                || INTERACTION_EXPIRY.get(id),
            )
            .await;

            clear_interaction_component_expiry(id);
            if !expired {
                return;
            }
        } else {
            tokio::time::sleep(Duration::from_secs(COMPONENT_IDLE_TIMEOUT_SECS)).await;
        }

        if let Err(err) = ctx
            .interaction()
            .update_response(&token)
            .components(Some(&[]))
            .await
        {
            tracing::debug!(
                ?err,
                ?msg_id,
                "failed to remove expired interaction components"
            );
        }
    });
}

pub async fn handle_component(ctx: Arc<Context>, component: InteractionComponent) {
    let name = component.data.custom_id.clone();
    touch_interaction_component_expiry(component.message.id);
    super::log_interaction(&ctx, &component, "invoked component", &name);

    let res = match name.as_str() {
        "pp_version_select" => handle_pp_version_select(ctx, component).await,
        "help_basecommand" => handle_help_basecommand(&ctx, component).await,
        "help_subcommand" => handle_help_subcommand(&ctx, component).await,
        "pagination_start" => handle_pagination_start(ctx, component).await,
        "pagination_back" => handle_pagination_back(ctx, component).await,
        "pagination_custom" => handle_pagination_custom(ctx, component).await,
        "pagination_step" => handle_pagination_step(ctx, component).await,
        "pagination_end" => handle_pagination_end(ctx, component).await,
        s if s == "owner_settings_section" || s.starts_with("owner_settings_section_") => {
            crate::commands::owner::settings::handle_owner_settings_section(ctx, component).await
        }
        _ if name.starts_with("owner_settings_prop_") => crate::commands::owner::settings::handle_owner_settings_prop(ctx, component).await,
        _ if name.starts_with("oset_applyall_") => crate::commands::owner::settings::handle_owner_settings_applyall(ctx, component).await,
        "oset_skipall" => crate::commands::owner::settings::handle_owner_settings_skipall(ctx, component).await,
        _ if name.starts_with("uset_open:") => crate::commands::danser::settings::menu::handle_user_settings_open(ctx, component).await,
        _ if name.starts_with("uset_tab:") => crate::commands::danser::settings::menu::handle_user_settings_tab(ctx, component).await,
        _ if name.starts_with("uset_skin:") => crate::commands::danser::settings::menu::handle_user_settings_skin(ctx, component).await,
        _ if name.starts_with("uset_tog:") => crate::commands::danser::settings::menu::handle_user_settings_toggle(ctx, component).await,
        _ if name.starts_with("uset_modal:") => crate::commands::danser::settings::menu::handle_user_settings_open_modal(ctx, component).await,
        _ if name.starts_with("uset_preset:") => crate::commands::danser::settings::menu::handle_user_settings_preset(ctx, component).await,
        _ if name.starts_with("uset_reset:") => crate::commands::danser::settings::menu::handle_user_settings_reset(ctx, component).await,
        _ if name.starts_with("uset_done:") => crate::commands::danser::settings::menu::handle_user_settings_done(ctx, component).await,
        _ if name.starts_with("pr_pg:") => crate::commands::danser::profile_render::handle_profile_page_button(ctx, component).await,
        _ if name.starts_with("pr_ren:") => crate::commands::danser::profile_render::handle_profile_render_button(ctx, component).await,
        _ if name.starts_with("render_cancel:") => handle_render_cancel(ctx, component).await,
        _ => {
            let _ = component
                .ephemeral_text(&ctx, "This interaction is unknown or has expired.")
                .await;
            return error!("unknown message component `{name}`");
        }
    };

    if let Err(err) = res.with_context(|| format!("failed to process component `{name}`")) {
        error!("{err:?}");
    }
}

async fn handle_pp_version_select(
    ctx: Arc<Context>,
    mut component: InteractionComponent,
) -> eyre::Result<()> {
    touch_pp_select(component.message.id);
    let Some(value) = component.data.values.first() else {
        return Ok(());
    };

    let parts: Vec<&str> = value.split('|').collect();
    if parts.len() < 5 {
        return Ok(());
    }

    let version_id = parts[0];
    let is_perfect_fc = parts[1] == "true";
    let pp = parts[2];
    let max_pp = parts[3];
    let nochoke_pp = parts[4];
    let breakdown_tuple = (parts.len() >= 8).then(|| (parts[5], parts[6], parts[7]));

    let Some(mut embed) = mem::take(&mut component.message.embeds).into_iter().next() else {
        return Ok(());
    };

    for field in &mut embed.fields {
        match field.name.as_str() {
            "PP" => {
                field.value = format_pp_field(
                    pp,
                    max_pp,
                    nochoke_pp,
                    is_perfect_fc,
                    breakdown_tuple,
                    pp_version_label(version_id),
                );
            }
            "pp" => {
                field.value = if is_perfect_fc {
                    format!("{pp}pp / {max_pp}pp")
                } else {
                    format!("{pp}pp *(No Choke: {nochoke_pp}pp)* / {max_pp}pp")
                };
            }
            "PP Breakdown" => {
                field.value = breakdown_tuple.map_or_else(
                    || "*Not exposed for this rework (see rosu-pp v4)*".to_owned(),
                    |(aim, speed, acc)| format!("Aim: `{aim}` | Speed: `{speed}` | Acc: `{acc}`"),
                );
            }
            _ => {}
        }
    }

    let mut components = mem::take(&mut component.message.components);
    for row in &mut components {
        if let Component::ActionRow(action_row) = row {
            for comp in &mut action_row.components {
                if let Component::SelectMenu(sm) = comp {
                    if let Some(opts) = &mut sm.options {
                        for option in opts {
                            option.default = option.value == *value;
                        }
                    }
                }
            }
        }
    }

    let builder = MessageBuilder::new().embed(embed).components(components);
    component.callback(&ctx, builder).await?;

    Ok(())
}

async fn handle_render_cancel(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> eyre::Result<()> {
    let Some(job_id) = component
        .data
        .custom_id
        .strip_prefix("render_cancel:")
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return Ok(());
    };

    let user_id = component.user_id()?;
    let channel_id = component.message.channel_id;
    let member_perms = component.member.as_ref().and_then(|m| m.permissions);
    let is_admin =
        check_authority_with_perms(&ctx, user_id, channel_id, component.guild_id, member_perms)
            .is_none();

    match ctx.replay_queue.cancel_job(job_id, user_id, is_admin).await {
        CancelResult::CancelledRunning => {
            component
                .ephemeral_text(&ctx, "🛑 Cancelling active render...")
                .await?;
        }
        CancelResult::RemovedQueued(removed) => {
            let _ = tokio::fs::remove_file(&removed.path).await;
            let builder = MessageBuilder::new()
                .embed(format!("🛑 Render cancelled by <@{user_id}>."))
                .components(vec![]);
            component.callback(&ctx, builder).await?;
        }
        CancelResult::Forbidden => {
            component
                .ephemeral_text(
                    &ctx,
                    "Only the user who requested this render (or a server admin) can cancel it.",
                )
                .await?;
        }
        CancelResult::NotFound => {
            component
                .ephemeral_text(
                    &ctx,
                    "This render job is already finished or no longer in the queue.",
                )
                .await?;
        }
    }

    Ok(())
}
