use crate::core::Context as TwilightContext;
use crate::pagination::SkinListPagination;
use crate::util::builder::MessageBuilder;
use crate::util::interaction::InteractionCommand;
use crate::util::InteractionCommandExt;
use eyre::Result;
use std::sync::Arc;

pub async fn list(ctx: Arc<TwilightContext>, command: InteractionCommand) -> Result<()> {
    let skins: Vec<String> = ctx
        .skin_list()
        .get()?
        .iter()
        .map(|skin| crate::util::str::percent_decode_utf8(&skin.to_string_lossy()).replace('_', " "))
        .collect();

    if skins.is_empty() {
        let builder = MessageBuilder::new().embed("No skins available.");
        command.callback(&ctx, builder, false).await?;
        return Ok(());
    }

    SkinListPagination::builder(skins).start(ctx, command).await
}
