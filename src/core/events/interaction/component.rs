use std::{mem, sync::Arc};

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
        _ if name.starts_with("owner_settings_prop_") => crate::commands::owner::settings::handle_owner_settings_prop(ctx, component).await,
        _ if name.starts_with("oset_applyall_") => crate::commands::owner::settings::handle_owner_settings_applyall(ctx, component).await,
        "oset_skipall" => crate::commands::owner::settings::handle_owner_settings_skipall(ctx, component).await,
        _ => return error!("unknown message component `{name}`"),
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

    let mut message = component.message.clone();

    if let Some(embed) = message.embeds.first_mut() {
        for field in &mut embed.fields {
            if field.name == "pp" {
                field.value = pp_value.clone();
                break;
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
