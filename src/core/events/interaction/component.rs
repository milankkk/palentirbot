use std::sync::Arc;

use eyre::Context as _;

use crate::{
    commands::help::{handle_help_basecommand, handle_help_subcommand},
    core::{events::EventLocation, Context},
    pagination::components::*,
    util::{interaction::InteractionComponent, Authored},
};

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
        "profile_compact" => handle_profile_compact(ctx, component).await,
        "profile_medium" => handle_profile_medium(ctx, component).await,
        "profile_full" => handle_profile_full(ctx, component).await,
        "owner_settings_section" => crate::commands::owner::settings::handle_owner_settings_section(ctx, component).await,
        _ if name.starts_with("owner_settings_section_") => crate::commands::owner::settings::handle_owner_settings_section(ctx, component).await,
        _ if name.starts_with("owner_settings_prop_") => crate::commands::owner::settings::handle_owner_settings_prop(ctx, component).await,
        _ if name.starts_with("oset_applyall_") => crate::commands::owner::settings::handle_owner_settings_applyall(ctx, component).await,
        "oset_skipall" => crate::commands::owner::settings::handle_owner_settings_skipall(ctx, component).await,
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
    let values = &component.data.values;
    if values.is_empty() {
        return Ok(());
    }
    let value = &values[0];

    let parts: Vec<&str> = value.split('|').collect();
    if parts.len() < 5 {
        return Ok(());
    }

    let is_perfect_fc = parts[1] == "true";
    let pp = parts[2];
    let max_pp = parts[3];
    let nochoke_pp = parts[4];

    let pp_value = if is_perfect_fc {
        format!("{}pp / {}pp", pp, max_pp)
    } else {
        format!("{}pp *(No Choke: {}pp)* / {}pp", pp, nochoke_pp, max_pp)
    };

    let breakdown = (parts.len() >= 8).then(|| {
        format!(
            "Aim: `{}` | Speed: `{}` | Acc: `{}`",
            parts[5], parts[6], parts[7]
        )
    });

    let mut message = component.message.clone();

    if let Some(embed) = message.embeds.first_mut() {
        for field in &mut embed.fields {
            if field.name == "pp" {
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
    let is_admin = crate::core::commands::checks::check_authority(
        &ctx,
        user_id,
        channel_id,
        component.guild_id,
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
