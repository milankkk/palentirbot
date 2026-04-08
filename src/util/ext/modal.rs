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

    fn defer<'a>(
        &'a self,
        ctx: &'a Context,
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
            ..Default::default()
        };

        let response = InteractionResponse {
            kind: InteractionResponseType::UpdateMessage,
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

    async fn update<'a>(&'a self, ctx: &'a Context, builder: &'a MessageBuilder<'a>) -> Result<Message> {
        self.message
            .as_ref()
            .expect("no message in modal")
            .update(ctx, builder)
            .await
    }
}