use eyre::Result;
use twilight_model::http::interaction::{InteractionResponse, InteractionResponseType};

use crate::{
    core::Context,
    util::{builder::MessageBuilder, interaction::InteractionModal},
};

use super::{build_response_data, send_update_response};

pub trait ModalExt {
    async fn callback(&self, ctx: &Context, builder: MessageBuilder<'_>) -> Result<()>;

    async fn update_callback(&self, ctx: &Context, builder: MessageBuilder<'_>) -> Result<()>;

    async fn defer_ephemeral(&self, ctx: &Context) -> Result<()>;

    async fn update_response(&self, ctx: &Context, builder: &MessageBuilder<'_>) -> Result<()>;
}

impl ModalExt for InteractionModal {
    async fn callback(&self, ctx: &Context, builder: MessageBuilder<'_>) -> Result<()> {
        let response = InteractionResponse {
            kind: InteractionResponseType::ChannelMessageWithSource,
            data: Some(build_response_data(builder, true)),
        };

        ctx.interaction()
            .create_response(self.id, &self.token, &response)
            .await?;

        Ok(())
    }

    async fn update_callback(&self, ctx: &Context, builder: MessageBuilder<'_>) -> Result<()> {
        let response = InteractionResponse {
            kind: InteractionResponseType::UpdateMessage,
            data: Some(build_response_data(builder, false)),
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
}
