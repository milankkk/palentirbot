use std::{borrow::Cow, mem};

use eyre::Result;
use twilight_interactions::command::CommandInputData;
use twilight_model::{
    application::command::CommandOptionChoice,
    channel::Message,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};

use crate::{
    core::Context,
    util::{
        builder::{EmbedBuilder, MessageBuilder},
        constants::RED,
        interaction::InteractionCommand,
    },
};

use super::{build_response_data, send_update_response};

pub trait InteractionCommandExt {
    fn input_data(&mut self) -> CommandInputData<'static>;

    async fn callback(
        &self,
        ctx: &Context,
        builder: MessageBuilder<'_>,
        ephemeral: bool,
    ) -> Result<()>;

    async fn defer(&self, ctx: &Context, ephemeral: bool) -> Result<()>;

    async fn update(&self, ctx: &Context, builder: &MessageBuilder<'_>) -> Result<Message>;

    async fn error(&self, ctx: &Context, content: impl Into<String>) -> Result<Message>;

    async fn error_callback(
        &self,
        ctx: &Context,
        content: impl Into<String>,
        ephemeral: bool,
    ) -> Result<()>;

    async fn autocomplete(
        &self,
        ctx: &Context,
        choices: Vec<CommandOptionChoice>,
    ) -> Result<()>;
}

impl InteractionCommandExt for InteractionCommand {
    fn input_data(&mut self) -> CommandInputData<'static> {
        CommandInputData {
            options: mem::take(&mut self.data.options),
            resolved: self.data.resolved.take().map(Cow::Owned),
        }
    }

    async fn callback(
        &self,
        ctx: &Context,
        builder: MessageBuilder<'_>,
        ephemeral: bool,
    ) -> Result<()> {
        let response = InteractionResponse {
            kind: InteractionResponseType::ChannelMessageWithSource,
            data: Some(build_response_data(builder, ephemeral)),
        };

        ctx.interaction()
            .create_response(self.id, &self.token, &response)
            .await?;

        Ok(())
    }

    async fn defer(&self, ctx: &Context, ephemeral: bool) -> Result<()> {
        let response = InteractionResponse {
            kind: InteractionResponseType::DeferredChannelMessageWithSource,
            data: Some(build_response_data(MessageBuilder::new(), ephemeral)),
        };

        ctx.interaction()
            .create_response(self.id, &self.token, &response)
            .await?;

        Ok(())
    }

    async fn update(&self, ctx: &Context, builder: &MessageBuilder<'_>) -> Result<Message> {
        send_update_response(ctx, &self.token, builder).await
    }

    async fn error(&self, ctx: &Context, content: impl Into<String>) -> Result<Message> {
        let embed = EmbedBuilder::new().description(content).color(RED).build();

        Ok(ctx
            .interaction()
            .update_response(&self.token)
            .content(None)
            .embeds(Some(&[embed]))
            .components(Some(&[]))
            .await?
            .model()
            .await?)
    }

    async fn error_callback(
        &self,
        ctx: &Context,
        content: impl Into<String>,
        ephemeral: bool,
    ) -> Result<()> {
        let embed = EmbedBuilder::new().description(content).color(RED);
        let builder = MessageBuilder::new().embed(embed);

        self.callback(ctx, builder, ephemeral).await
    }

    async fn autocomplete(
        &self,
        ctx: &Context,
        choices: Vec<CommandOptionChoice>,
    ) -> Result<()> {
        let data = InteractionResponseData {
            choices: Some(choices),
            ..Default::default()
        };

        let response = InteractionResponse {
            kind: InteractionResponseType::ApplicationCommandAutocompleteResult,
            data: Some(data),
        };

        ctx.interaction()
            .create_response(self.id, &self.token, &response)
            .await?;

        Ok(())
    }
}
