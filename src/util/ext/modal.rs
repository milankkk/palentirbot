use eyre::Result;
use twilight_model::{
    channel::Message,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};

use crate::{
    core::Context,
    util::{builder::MessageBuilder, interaction::InteractionModal},
};

use super::MessageExt;

pub trait ModalExt {
    fn callback<'a>(
        &'a self,
        ctx: &'a Context,
        builder: MessageBuilder<'a>,
    ) -> impl std::future::Future<Output = Result<()>> + 'a;

    fn defer<'a>(&'a self, ctx: &'a Context) -> impl std::future::Future<Output = Result<()>> + 'a;

    fn defer_ephemeral<'a>(&'a self, ctx: &'a Context) -> impl std::future::Future<Output = Result<()>> + 'a;

    fn update_response<'a>(
        &'a self,
        ctx: &'a Context,
        builder: &'a MessageBuilder<'a>,
    ) -> impl std::future::Future<Output = Result<()>> + 'a;

    fn update<'a>(
        &'a self,
        ctx: &'a Context,
        builder: &'a MessageBuilder<'a>,
    ) -> impl std::future::Future<Output = Result<Message>> + 'a;
}

impl ModalExt for InteractionModal {
    async fn callback<'a>(&'a self, ctx: &'a Context, builder: MessageBuilder<'a>) -> Result<()> {
        let data = InteractionResponseData {
            components: builder.components,
            embeds: builder.embed.map(|e| vec![e]),
            content: builder.content.map(|c| c.into_owned()),
            flags: Some(twilight_model::channel::message::MessageFlags::EPHEMERAL),
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

    async fn defer<'a>(&'a self, ctx: &'a Context) -> Result<()> {
        let response = InteractionResponse {
            kind: InteractionResponseType::DeferredUpdateMessage,
            data: None,
        };

        ctx.interaction()
            .create_response(self.id, &self.token, &response)
            .await?;

        Ok(())
    }

    async fn defer_ephemeral<'a>(&'a self, ctx: &'a Context) -> Result<()> {
        let data = InteractionResponseData {
            flags: Some(twilight_model::channel::message::MessageFlags::EPHEMERAL),
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

    async fn update_response<'a>(&'a self, ctx: &'a Context, builder: &'a MessageBuilder<'a>) -> Result<()> {
        let client = ctx.interaction();
        let mut req = client.update_response(&self.token);

        if let Some(ref content) = builder.content {
            req = req.content(Some(content.as_ref()));
        }

        if let Some(ref embed) = builder.embed {
            req = req.embeds(Some(std::slice::from_ref(embed)));
        }

        if let Some(ref components) = builder.components {
            req = req.components(Some(components));
        }

        req.await?;
        Ok(())
    }

    async fn update<'a>(
        &'a self,
        ctx: &'a Context,
        builder: &'a MessageBuilder<'a>,
    ) -> Result<Message> {
        self.message
            .as_ref()
            .expect("no message in modal")
            .update(ctx, builder)
            .await
    }
}
