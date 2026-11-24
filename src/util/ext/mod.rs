use std::{borrow::Cow, slice};

use eyre::Result;
use twilight_model::{
    channel::{message::MessageFlags, Message},
    http::interaction::InteractionResponseData,
};

use crate::{core::Context, util::builder::MessageBuilder};

pub use self::{
    authored::Authored, channel::ChannelExt, component::ComponentExt,
    interaction_command::InteractionCommandExt, message::MessageExt, modal::*,
};

mod authored;
mod channel;
mod component;
mod interaction_command;
mod message;
mod modal;

pub(super) fn build_response_data(
    builder: MessageBuilder<'_>,
    ephemeral: bool,
) -> InteractionResponseData {
    InteractionResponseData {
        components: builder.components,
        content: builder.content.map(Cow::into_owned),
        embeds: builder.embed.map(|e| vec![e]),
        flags: ephemeral.then_some(MessageFlags::EPHEMERAL),
        ..Default::default()
    }
}

pub(super) async fn send_update_response(
    ctx: &Context,
    token: &str,
    builder: &MessageBuilder<'_>,
) -> Result<Message> {
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

    if !builder.attachments.is_empty() {
        req = req.attachments(&builder.attachments);
    }

    Ok(req.await?.model().await?)
}
