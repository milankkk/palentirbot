use std::sync::Arc;

use eyre::Context as _;

use crate::{
    core::{events::EventLocation, Context},
    pagination::components::handle_pagination_modal,
    util::{interaction::InteractionModal, Authored},
};

pub async fn handle_modal(ctx: Arc<Context>, modal: InteractionModal) {
    let name = modal.data.custom_id.clone();

    {
        let username = modal
            .user()
            .map(|u| u.name.as_str())
            .unwrap_or("<unknown user>");

        let location = EventLocation::new(&ctx, &modal);
        info!("[{location}] {username} invoked modal `{name}`");
    }

    let res = match name.as_str() {
        "pagination_page" => handle_pagination_modal(ctx, modal).await,
        _ if name.starts_with("oset_modal_") => crate::commands::owner::settings::handle_owner_settings_modal(ctx, modal).await,
        _ if name.starts_with("uset_m:") => crate::commands::danser::settings::menu::handle_user_settings_modal(ctx, modal).await,
        _ => {
            let response = twilight_model::http::interaction::InteractionResponse {
                kind: twilight_model::http::interaction::InteractionResponseType::ChannelMessageWithSource,
                data: Some(twilight_model::http::interaction::InteractionResponseData {
                    content: Some("This modal is unknown or has expired.".to_owned()),
                    flags: Some(twilight_model::channel::message::MessageFlags::EPHEMERAL),
                    ..Default::default()
                }),
            };
            let _ = ctx
                .interaction()
                .create_response(modal.id, &modal.token, &response)
                .await;
            return error!("unknown modal `{name}`");
        }
    };

    if let Err(err) = res.with_context(|| format!("failed to process modal `{name}`")) {
        error!("{err:?}");
    }
}
