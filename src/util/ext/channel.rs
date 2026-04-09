use std::slice;

use eyre::Result;
use twilight_model::{
    channel::Message,
    id::{marker::ChannelMarker, Id},
};

use crate::{
    core::Context,
    util::{
        builder::{EmbedBuilder, MessageBuilder},
        constants::RED,
    },
};

pub trait ChannelExt {
    async fn create_message(&self, ctx: &Context, builder: &MessageBuilder<'_>) -> Result<Message>;
    async fn error(&self, ctx: &Context, content: impl Into<String>) -> Result<Message>;
    async fn plain_message(&self, ctx: &Context, content: &str) -> Result<Message>;
}

impl ChannelExt for Id<ChannelMarker> {
    async fn create_message(&self, ctx: &Context, builder: &MessageBuilder<'_>) -> Result<Message> {
        let mut req = ctx.http.create_message(*self);

        if let Some(ref content) = builder.content {
            req = req.content(content.as_ref());
        }
        if let Some(ref embed) = builder.embed {
            req = req.embeds(slice::from_ref(embed));
        }
        if let Some(components) = builder.components.as_deref() {
            req = req.components(components);
        }
        if let Some(ref attachment) = builder.attachment {
            req = req.attachments(slice::from_ref(attachment));
        }

        Ok(req.await?.model().await?)
    }

    async fn error(&self, ctx: &Context, content: impl Into<String>) -> Result<Message> {
        let embed = EmbedBuilder::new().color(RED).description(content).build();
        Ok(ctx.http.create_message(*self).embeds(&[embed]).await?.model().await?)
    }

    async fn plain_message(&self, ctx: &Context, content: &str) -> Result<Message> {
        Ok(ctx.http.create_message(*self).content(content).await?.model().await?)
    }
}

impl ChannelExt for Message {
    async fn create_message(&self, ctx: &Context, builder: &MessageBuilder<'_>) -> Result<Message> {
        self.channel_id.create_message(ctx, builder).await
    }
    async fn error(&self, ctx: &Context, content: impl Into<String>) -> Result<Message> {
        self.channel_id.error(ctx, content).await
    }
    async fn plain_message(&self, ctx: &Context, content: &str) -> Result<Message> {
        self.channel_id.plain_message(ctx, content).await
    }
}
