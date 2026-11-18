use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

use eyre::Context as _;
use tokio::time::Instant;
use twilight_model::id::{marker::MessageMarker, Id};

use crate::{
    commands::help::{handle_help_basecommand, handle_help_subcommand},
    core::{
        events::EventLocation,
        replay_queue::process::{format_pp_field, pp_version_label},
        Context,
    },
    pagination::components::*,
    util::{interaction::InteractionComponent, Authored},
};

static PP_SELECT_ACTIVITY: OnceLock<Mutex<HashMap<Id<MessageMarker>, Instant>>> = OnceLock::new();

fn pp_select_map() -> &'static Mutex<HashMap<Id<MessageMarker>, Instant>> {
    PP_SELECT_ACTIVITY.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn touch_pp_select(msg_id: Id<MessageMarker>) {
    let mut map = pp_select_map().lock().unwrap_or_else(|p| p.into_inner());
    map.insert(msg_id, Instant::now());
}

pub(crate) fn last_pp_select_activity(msg_id: Id<MessageMarker>) -> Option<Instant> {
    let map = pp_select_map().lock().unwrap_or_else(|p| p.into_inner());
    map.get(&msg_id).copied()
}

pub(crate) fn clear_pp_select(msg_id: Id<MessageMarker>) {
    let mut map = pp_select_map().lock().unwrap_or_else(|p| p.into_inner());
    map.remove(&msg_id);
}

pub async fn handle_component(ctx: Arc<Context>, component: InteractionComponent) {
    let name = component.data.custom_id.clone();

    {
        let username = component
            .user()
            .map(|u| u.name.as_str())
            .unwrap_or("<unknown user>");

        let location = EventLocation::new(&ctx, &component);
        info!("[{location}] {username} invoked component `{name}`");
    }

    let res = match name.as_str() {
        "pp_version_select" => handle_pp_version_select(ctx, component).await,
        "help_basecommand" => handle_help_basecommand(&ctx, component).await,
        "help_subcommand" => handle_help_subcommand(&ctx, component).await,
        "pagination_start" => handle_pagination_start(ctx, component).await,
        "pagination_back" => handle_pagination_back(ctx, component).await,
        "pagination_custom" => handle_pagination_custom(ctx, component).await,
        "pagination_step" => handle_pagination_step(ctx, component).await,
        "pagination_end" => handle_pagination_end(ctx, component).await,
        "owner_settings_section" => crate::commands::owner::settings::handle_owner_settings_section(ctx, component).await,
        _ if name.starts_with("owner_settings_section_") => crate::commands::owner::settings::handle_owner_settings_section(ctx, component).await,
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
        _ if name.starts_with("render_cancel:") => handle_render_cancel(ctx, component).await,
        _ => {
            let _ = respond_ephemeral(
                &ctx,
                &component,
                "This interaction is unknown or has expired.".to_owned(),
            )
            .await;
            return error!("unknown message component `{name}`");
        }
    };

    if let Err(err) = res.with_context(|| format!("failed to process component `{name}`")) {
        error!("{err:?}");
    }
}

async fn handle_pp_version_select(ctx: Arc<Context>, component: InteractionComponent) -> eyre::Result<()> {
    touch_pp_select(component.message.id);
    let values = &component.data.values;
    if values.is_empty() {
        return Ok(());
    }
    let value = &values[0];

    let parts: Vec<&str> = value.split('|').collect();
    if parts.len() < 5 {
        return Ok(());
    }

    let version_id = parts[0];
    let is_perfect_fc = parts[1] == "true";
    let pp = parts[2];
    let max_pp = parts[3];
    let nochoke_pp = parts[4];

    let pp_value = if is_perfect_fc {
        format!("{}pp / {}pp", pp, max_pp)
    } else {
        format!("{}pp *(No Choke: {}pp)* / {}pp", pp, nochoke_pp, max_pp)
    };

    let breakdown_tuple = (parts.len() >= 8).then(|| (parts[5], parts[6], parts[7]));
    let breakdown = breakdown_tuple.map(|(aim, speed, acc)| {
        format!("Aim: `{aim}` | Speed: `{speed}` | Acc: `{acc}`")
    });
    let compact_pp_value = format_pp_field(
        pp,
        max_pp,
        nochoke_pp,
        is_perfect_fc,
        breakdown_tuple,
        pp_version_label(version_id),
    );

    let mut message = component.message.clone();

    if let Some(embed) = message.embeds.first_mut() {
        for field in &mut embed.fields {
            if field.name == "PP" {
                field.value = compact_pp_value.clone();
            } else if field.name == "pp" {
                field.value = pp_value.clone();
            } else if field.name == "PP Breakdown" {
                field.value = breakdown
                    .clone()
                    .unwrap_or_else(|| "*Not exposed for this rework (see rosu-pp v4)*".to_owned());
            }
        }
    }

    let embed = match message.embeds.first() {
        Some(e) => e.clone(),
        None => return Ok(()),
    };

    use crate::util::builder::MessageBuilder;
    use crate::util::ComponentExt;
    
    let mut components = message.components.clone();
    for row in &mut components {
        if let twilight_model::channel::message::component::Component::ActionRow(action_row) = row {
            for comp in &mut action_row.components {
                if let twilight_model::channel::message::component::Component::SelectMenu(sm) = comp {
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

async fn respond_ephemeral(
    ctx: &Context,
    component: &InteractionComponent,
    content: String,
) -> eyre::Result<()> {
    let response = twilight_model::http::interaction::InteractionResponse {
        kind: twilight_model::http::interaction::InteractionResponseType::ChannelMessageWithSource,
        data: Some(twilight_model::http::interaction::InteractionResponseData {
            content: Some(content),
            flags: Some(twilight_model::channel::message::MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    };
    ctx.interaction()
        .create_response(component.id, &component.token, &response)
        .await?;
    Ok(())
}

async fn handle_render_cancel(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> eyre::Result<()> {
    use crate::core::replay_queue::CancelResult;
    use crate::util::builder::MessageBuilder;
    use crate::util::ComponentExt;

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
    let is_admin = crate::core::commands::checks::check_authority_with_perms(
        &ctx,
        user_id,
        channel_id,
        component.guild_id,
        member_perms,
    )
    .await
    .is_none();

    match ctx.replay_queue.cancel_job(job_id, user_id, is_admin).await {
        CancelResult::CancelledRunning => {
            respond_ephemeral(&ctx, &component, "🛑 Cancelling active render...".to_owned()).await?;
        }
        CancelResult::RemovedQueued(removed) => {
            let _ = tokio::fs::remove_file(&removed.path).await;
            let builder = MessageBuilder::new()
                .embed(format!("🛑 Render cancelled by <@{user_id}>."))
                .components(vec![]);
            component.callback(&ctx, builder).await?;
        }
        CancelResult::Forbidden => {
            respond_ephemeral(
                &ctx,
                &component,
                "Only the user who requested this render (or a server admin) can cancel it."
                    .to_owned(),
            )
            .await?;
        }
        CancelResult::NotFound => {
            respond_ephemeral(
                &ctx,
                &component,
                "This render job is already finished or no longer in the queue.".to_owned(),
            )
            .await?;
        }
    }

    Ok(())
}
