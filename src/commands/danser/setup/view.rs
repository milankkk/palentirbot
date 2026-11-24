use std::{fmt::Write, sync::Arc};

use eyre::Result;

use crate::{
    core::Context,
    util::{builder::MessageBuilder, interaction::InteractionCommand, InteractionCommandExt},
};

pub async fn view(ctx: Arc<Context>, command: InteractionCommand) -> Result<()> {
    let Some(guild_id) = command.guild_id else {
        command
            .error_callback(&ctx, "This command can only be used in a server.", true)
            .await?;
        return Ok(());
    };

    let (prefix, input_channels, ai_channel) = ctx
        .guild_settings(guild_id, |s| {
            let mut iter = s.input_channels.iter();
            let channels = iter
                .next()
                .map(|first| {
                    let mut text = format!("<#{first}>");
                    for ch in iter {
                        let _ = write!(text, ", <#{ch}>");
                    }
                    text
                })
                .unwrap_or_else(|| "All channels (unrestricted)".to_owned());
            let ai = s
                .ai_channel
                .map_or_else(|| "None".to_owned(), |ch| format!("<#{ch}>"));
            (s.effective_prefix().to_owned(), channels, ai)
        })
        .unwrap_or_else(|| {
            (
                "!".to_owned(),
                "All channels (unrestricted)".to_owned(),
                "None".to_owned(),
            )
        });

    let content = format!(
        "Prefix: `{prefix}`\nInput channels: {input_channels}\nOutput: Requesting channel\nAI chat channel: {ai_channel}"
    );
    let builder = MessageBuilder::new().embed(content);
    command.callback(&ctx, builder, false).await?;

    Ok(())
}
