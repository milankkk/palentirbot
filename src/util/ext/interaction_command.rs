use std::{borrow::Cow, mem, slice};

use eyre::Result;
use twilight_interactions::command::CommandInputData;
use twilight_model::{
    application::command::CommandOptionChoice,
    channel::{message::MessageFlags, Message},
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};

use crate::util::builder::EmbedBuilder;

use crate::{
    core::Context,
    util::{
        builder::MessageBuilder,
        constants::RED,
        interaction::InteractionCommand,
    },
};

pub trait InteractionCommandExt {
    fn input_data(&mut self) -> CommandInputData<'static>;

    fn callback<'a>(
        &'a self,
        ctx: &'a Context,
        builder: MessageBuilder<'a>,
        ephemeral: bool,
    ) -> impl std::future::Future<Output = Result<()>> + 'a;

    fn defer<'a>(
        &'a self,
        ctx: &'a Context,
        ephemeral: bool,
    ) -> impl std::future::Future<Output = Result<()>> + 'a;

    fn update<'a>(
        &'a self,
        ctx: &'a Context,
        builder: &'a MessageBuilder<'a>,
    ) -> impl std::future::Future<Output = Result<Message>> + 'a;

    fn error<'a>(
        &'a self,
        ctx: &'a Context,
        content: impl Into<String> + 'a,
    ) -> impl std::future::Future<Output = Result<Message>> + 'a;

    fn error_callback<'a>(
        &'a self,
        ctx: &'a Context,
        content: impl Into<String> + 'a,
        ephemeral: bool,
    ) -> impl std::future::Future<Output = Result<()>> + 'a;

    fn autocomplete<'a>(
        &'a self,
        ctx: &'a Context,
        choices: Vec<CommandOptionChoice>,
    ) -> impl std::future::Future<Output = Result<()>> + 'a;
}

impl InteractionCommandExt for InteractionCommand {
    fn input_data(&mut self) -> CommandInputData<'static> {
        CommandInputData {
            options: mem::take(&mut self.data.options),
            resolved: self.data.resolved.take().map(Cow::Owned),
        }
    }

    async fn callback<'a>(
        &'a self,
        ctx: &'a Context,
        builder: MessageBuilder<'a>,
        ephemeral: bool,
    ) -> Result<()> {
        let data = InteractionResponseData {
            components: builder.components,
            content: builder.content.map(|c| c.into_owned()),
            embeds: builder.embed.map(|e| vec![e]),
            flags: ephemeral.then_some(MessageFlags::EPHEMERAL),
            ..Default::default()
        };

        let response = InteractionResponse {
            kind: InteractionResponseType::ChannelMessageWithSource,
            data: Some(data),
        };

        ctx.interaction()
            .create_response(self.id, &self.token, &response)
            .await?;

        Ok(())
    }

    async fn defer<'a>(&'a self, ctx: &'a Context, ephemeral: bool) -> Result<()> {
        let data = InteractionResponseData {
            flags: ephemeral.then_some(MessageFlags::EPHEMERAL),
            ..Default::default()
        };

        let response = InteractionResponse {
            kind: InteractionResponseType::DeferredChannelMessageWithSource,
            data: Some(data),
        };

        ctx.interaction()
            .create_response(self.id, &self.token, &response)
            .await?;

        Ok(())
    }

    async fn update<'a>(&'a self, ctx: &'a Context, builder: &'a MessageBuilder<'a>) -> Result<Message> {
        let token = self.token.as_str();
        let client = ctx.interaction();
        let mut req = client.update_response(token);

        if let Some(ref content) = builder.content {
            req = req.content(Some(content.as_ref()));
        }

        if let Some(ref embed) = builder.embed {
            req = req.embeds(Some(slice::from_ref(embed)));
        }

        if let Some(ref components) = builder.components {
            req = req.components(Some(components));
        }

        if let Some(ref attachment) = builder.attachment {
            req = req.attachments(slice::from_ref(attachment));
        }

        Ok(req.await?.model().await? as twilight_model::channel::Message)
    }

    async fn error<'a>(&'a self, ctx: &'a Context, content: impl Into<String> + 'a) -> Result<Message> {
        let embed = EmbedBuilder::new().description(content).color(RED).build();

        Ok(ctx.interaction()
            .update_response(&self.token)
            .embeds(Some(&[embed]))
            .await?
            .model()
            .await?)
    }

    async fn error_callback<'a>(
        &'a self,
        ctx: &'a Context,
        content: impl Into<String> + 'a,
        ephemeral: bool,
    ) -> Result<()> {
        let embed = EmbedBuilder::new().description(content).color(RED);
        let builder = MessageBuilder::new().embed(embed);

        self.callback(ctx, builder, ephemeral).await
    }

    async fn autocomplete<'a>(
        &'a self,
        ctx: &'a Context,
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