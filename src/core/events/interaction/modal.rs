use std::sync::Arc;

use eyre::Context as _;

use crate::{
    core::Context,
    pagination::components::handle_pagination_modal,
    util::{builder::MessageBuilder, interaction::InteractionModal, ModalExt},
};

pub async fn handle_modal(ctx: Arc<Context>, modal: InteractionModal) {
    let name = modal.data.custom_id.clone();
    if let Some(ref msg) = modal.message {
        super::touch_interaction_component_expiry(msg.id);
    }

    super::log_interaction(&ctx, &modal, "invoked modal", &name);

    let res = match name.as_str() {
        "pagination_page" => handle_pagination_modal(ctx, modal).await,
        _ if name.starts_with("oset_modal_") => crate::commands::owner::settings::handle_owner_settings_modal(ctx, modal).await,
        _ if name.starts_with("uset_m:") => crate::commands::danser::settings::menu::handle_user_settings_modal(ctx, modal).await,
        _ => {
            let _ = modal
                .callback(
                    &ctx,
                    MessageBuilder::new().content("This modal is unknown or has expired."),
                )
                .await;
            return error!("unknown modal `{name}`");
        }
    };

    if let Err(err) = res.with_context(|| format!("failed to process modal `{name}`")) {
        error!("{err:?}");
    }
}
