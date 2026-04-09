use std::borrow::Cow;

use eyre::Result;
use twilight_model::{
    channel::Message,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};

use crate::util::builder::ModalBuilder;

use crate::{
    core::Context,
    util::{
        builder::MessageBuilder,
        interaction::InteractionComponent,
    },
};

use super::MessageExt;

pub trait ComponentExt {
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

    fn modal<'a>(
        &'a self,
        ctx: &'a Context,
        modal: ModalBuilder,
    ) -> impl std::future::Future<Output = Result<()>> + 'a;
}

impl ComponentExt for InteractionComponent {
    async fn callback<'a>(&'a self, ctx: &'a Context, builder: MessageBuilder<'a>) -> Result<()> {
        let data = InteractionResponseData {
            components: builder.components,
            embeds: builder.embed.map(|e| vec![e]),
            content: builder.content.map(Cow::into_owned),
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
        self.message.update(ctx, builder).await
    }

    async fn modal<'a>(&'a self, ctx: &'a Context, modal: ModalBuilder) -> Result<()> {
        let response = InteractionResponse {
            kind: InteractionResponseType::Modal,
            data: Some(modal.build()),
        };

        ctx.interaction()
            .create_response(self.id, &self.token, &response)
            .await?;

        Ok(())
    }
}