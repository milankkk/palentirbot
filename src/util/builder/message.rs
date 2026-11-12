use std::borrow::Cow;

use twilight_model::{
    channel::message::component::Component, channel::message::embed::Embed,
    http::attachment::Attachment, id::{marker::MessageMarker, Id},
};

use super::EmbedBuilder;

#[derive(Default)]
pub struct MessageBuilder<'c> {
    pub content: Option<Cow<'c, str>>,
    pub embed: Option<Embed>,
    pub attachments: Vec<Attachment>,
    pub components: Option<Vec<Component>>,
    pub reply: Option<Id<MessageMarker>>,
}

impl<'c> MessageBuilder<'c> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn content(mut self, content: impl Into<Cow<'c, str>>) -> Self {
        self.content = Some(content.into());

        self
    }

    pub fn embed(mut self, embed: impl IntoEmbed) -> Self {
        self.embed = Some(embed.into_embed());

        self
    }

    pub fn attachment(mut self, name: impl Into<String>, bytes: Vec<u8>) -> Self {
        let id = u64::try_from(self.attachments.len() + 1).unwrap_or(1);
        self.attachments
            .push(Attachment::from_bytes(name.into(), bytes, id));

        self
    }

    pub fn attachments<I>(mut self, files: I) -> Self
    where
        I: IntoIterator<Item = (String, Vec<u8>)>,
    {
        for (name, bytes) in files {
            let id = u64::try_from(self.attachments.len() + 1).unwrap_or(1);
            self.attachments.push(Attachment::from_bytes(name, bytes, id));
        }

        self
    }

    pub fn components(mut self, components: Vec<Component>) -> Self {
        self.components = Some(components);

        self
    }

    pub fn reply(mut self, message_id: Id<MessageMarker>) -> Self {
        self.reply = Some(message_id);

        self
    }
}

impl<'c> From<Embed> for MessageBuilder<'c> {
    #[inline]
    fn from(embed: Embed) -> Self {
        Self {
            embed: Some(embed),
            ..Default::default()
        }
    }
}

pub trait IntoEmbed {
    fn into_embed(self) -> Embed;
}

impl IntoEmbed for Embed {
    #[inline]
    fn into_embed(self) -> Embed {
        self
    }
}

impl IntoEmbed for EmbedBuilder {
    #[inline]
    fn into_embed(self) -> Embed {
        self.build()
    }
}

impl IntoEmbed for String {
    #[inline]
    fn into_embed(self) -> Embed {
        EmbedBuilder::new().description(self).build()
    }
}

impl IntoEmbed for &str {
    #[inline]
    fn into_embed(self) -> Embed {
        EmbedBuilder::new().description(self).build()
    }
}
