use eyre::Result;
use twilight_model::http::interaction::{InteractionResponse, InteractionResponseType};

use crate::{
    core::Context,
    util::{
        builder::{MessageBuilder, ModalBuilder},
        interaction::InteractionComponent,
    },
};

use super::{build_response_data, send_update_response};

pub trait ComponentExt {
    async fn callback(&self, ctx: &Context, builder: MessageBuilder<'_>) -> Result<()>;

    async fn ephemeral_callback(&self, ctx: &Context, builder: MessageBuilder<'_>) -> Result<()>;

    async fn ephemeral_text(&self, ctx: &Context, content: impl Into<String>) -> Result<()>;

    async fn defer(&self, ctx: &Context) -> Result<()>;

    async fn defer_ephemeral(&self, ctx: &Context) -> Result<()>;

    async fn update_response(&self, ctx: &Context, builder: &MessageBuilder<'_>) -> Result<()>;

    async fn modal(&self, ctx: &Context, modal: ModalBuilder) -> Result<()>;
}

impl ComponentExt for InteractionComponent {
    async fn callback(&self, ctx: &Context, builder: MessageBuilder<'_>) -> Result<()> {
        let response = InteractionResponse {
            kind: InteractionResponseType::UpdateMessage,
            data: Some(build_response_data(builder, false)),
        };

        ctx.interaction()
            .create_response(self.id, &self.token, &response)
            .await?;

        Ok(())
    }

    async fn ephemeral_callback(&self, ctx: &Context, builder: MessageBuilder<'_>) -> Result<()> {
        let response = InteractionResponse {
            kind: InteractionResponseType::ChannelMessageWithSource,
            data: Some(build_response_data(builder, true)),
        };

        ctx.interaction()
            .create_response(self.id, &self.token, &response)
            .await?;

        Ok(())
    }

    async fn ephemeral_text(&self, ctx: &Context, content: impl Into<String>) -> Result<()> {
        self.ephemeral_callback(ctx, MessageBuilder::new().content(content.into()))
            .await
    }

    async fn defer(&self, ctx: &Context) -> Result<()> {
        let response = InteractionResponse {
            kind: InteractionResponseType::DeferredUpdateMessage,
            data: None,
        };

        ctx.interaction()
            .create_response(self.id, &self.token, &response)
            .await?;

        Ok(())
    }

    async fn defer_ephemeral(&self, ctx: &Context) -> Result<()> {
        let response = InteractionResponse {
            kind: InteractionResponseType::DeferredChannelMessageWithSource,
            data: Some(build_response_data(MessageBuilder::new(), true)),
        };

        ctx.interaction()
            .create_response(self.id, &self.token, &response)
            .await?;

        Ok(())
    }

    async fn update_response(&self, ctx: &Context, builder: &MessageBuilder<'_>) -> Result<()> {
        send_update_response(ctx, &self.token, builder).await?;
        Ok(())
    }

    async fn modal(&self, ctx: &Context, modal: ModalBuilder) -> Result<()> {
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
