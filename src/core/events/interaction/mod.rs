use std::sync::Arc;

use twilight_model::application::interaction::{Interaction, InteractionData, InteractionType};

use crate::{
    core::{events::EventLocation, Context},
    util::{
        interaction::{InteractionCommand, InteractionComponent, InteractionModal},
        Authored,
    },
};

use self::{
    autocomplete::handle_autocomplete, command::handle_command, component::handle_component,
    modal::handle_modal,
};
pub(crate) use self::component::{
    clear_interaction_component_expiry, clear_pp_select, last_pp_select_activity,
    spawn_interaction_component_expiry, touch_interaction_component_expiry, wait_idle_expiry,
    COMPONENT_IDLE_TIMEOUT_SECS,
};

mod autocomplete;
mod command;
mod component;
mod modal;

pub(super) fn log_interaction(
    ctx: &Context,
    authored: &impl Authored,
    verb: &str,
    name: &str,
) {
    let username = authored
        .user()
        .map(|u| u.name.as_str())
        .unwrap_or("<unknown user>");
    let location = EventLocation::new(ctx, authored);
    info!("[{location}] {username} {verb} `{name}`");
}

pub async fn handle_interaction(ctx: Arc<Context>, interaction: Interaction) {
    let Interaction {
        channel,
        #[allow(deprecated)]
        channel_id,
        data,
        guild_id,
        id,
        kind,
        member,
        message,
        token,
        user,
        ..
    } = interaction;

    let channel_id = match channel.map(|c| c.id).or(channel_id) {
        Some(channel_id) => channel_id,
        None => return warn!("no channel id for interaction kind {kind:?}"),
    };

    match data {
        Some(InteractionData::ApplicationCommand(data)) => {
            let cmd = InteractionCommand {
                channel_id,
                data,
                guild_id,
                id,
                member,
                token,
                user,
            };

            match kind {
                InteractionType::ApplicationCommand => handle_command(ctx, cmd).await,
                InteractionType::ApplicationCommandAutocomplete => {
                    handle_autocomplete(ctx, cmd).await
                }
                _ => warn!("got unexpected interaction kind {kind:?}"),
            }
        }
        Some(InteractionData::MessageComponent(data)) => {
            let message = match message {
                Some(message) => message,
                None => return warn!("no message in interaction component"),
            };

            let component = InteractionComponent {
                channel_id,
                data: *data,
                guild_id,
                id,
                member,
                message,
                token,
                user,
            };

            handle_component(ctx, component).await
        }
        Some(InteractionData::ModalSubmit(data)) => {
            let modal = InteractionModal {
                channel_id,
                data,
                guild_id,
                id,
                member,
                message,
                token,
                user,
            };

            handle_modal(ctx, modal).await
        }
        _ => {}
    }
}
