use std::{mem, sync::Arc};

use eyre::Context as _;

use crate::{
    commands::{
        danser::{slash_render, slash_screenshot, slash_settings},
        owner::slash_owner_autocomplete,
    },
    core::Context,
    util::interaction::InteractionCommand,
};

pub async fn handle_autocomplete(ctx: Arc<Context>, mut command: InteractionCommand) {
    let name = mem::take(&mut command.data.name);
    super::log_interaction(&ctx, &command, "autocompleted", &name);

    let res = match name.as_str() {
        "render" => slash_render(ctx, command).await,
        "screenshot" => slash_screenshot(ctx, command).await,
        "settings" => slash_settings(ctx, command).await,
        "owner" => slash_owner_autocomplete(ctx, command).await,
        _ => return error!("unknown autocomplete command `{name}`"),
    };

    if let Err(err) = res.with_context(|| format!("failed to process autocomplete `{name}`")) {
        error!("{err:?}");
    }
}
