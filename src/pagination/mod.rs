use std::{sync::Arc, time::Duration};

use eyre::Result;
use tokio::{
    sync::watch::{self, Receiver, Sender},
    time::sleep,
};
use twilight_model::{
    channel::message::{
        component::{ButtonStyle, Component},
        embed::Embed,
        EmojiReactionType,
    },
    id::{
        marker::{ChannelMarker, MessageMarker, UserMarker},
        Id,
    },
};

use crate::{
    core::Context,
    util::{
        builder::{action_row, emoji_button, MessageBuilder},
        interaction::InteractionCommand,
        numbers::last_multiple,
        Authored, InteractionCommandExt, MessageExt,
    },
};

pub use self::skin_list::*;

mod skin_list;

pub mod components;

pub enum PaginationKind {
    SkinList(Box<SkinListPagination>),
}

impl PaginationKind {
    fn build_page(&mut self, pages: &Pages) -> Embed {
        match self {
            Self::SkinList(kind) => kind.build_page(pages),
        }
    }
}

pub struct Pagination {
    pub pages: Pages,
    author: Id<UserMarker>,
    kind: PaginationKind,
    tx: Sender<()>,
}

impl Pagination {
    async fn start(
        ctx: Arc<Context>,
        command: InteractionCommand,
        builder: PaginationBuilder,
    ) -> Result<()> {
        let PaginationBuilder { mut kind, pages } = builder;

        let embed = kind.build_page(&pages);
        let components = pages.components();
        let msg_builder = MessageBuilder::new().embed(embed).components(components);

        command.callback(&ctx, msg_builder, false).await?;
        let response = ctx.interaction().response(&command.token).await?.model().await?;

        if pages.last_index == 0 {
            return Ok(());
        }

        let channel = response.channel_id;
        let msg = response.id;

        let (tx, rx) = watch::channel(());
        Self::spawn_timeout(Arc::clone(&ctx), rx, msg, channel);

        let pagination = Pagination {
            author: command.user_id()?,
            kind,
            pages,
            tx,
        };

        ctx.paginations.own(msg).await.insert(pagination);

        Ok(())
    }

    fn is_author(&self, user: Id<UserMarker>) -> bool {
        self.author == user
    }

    fn reset_timeout(&self) {
        let _ = self.tx.send(());
    }

    fn build(&mut self) -> MessageBuilder<'static> {
        let embed = self.kind.build_page(&self.pages);
        let components = self.pages.components();

        MessageBuilder::new().embed(embed).components(components)
    }

    fn spawn_timeout(
        ctx: Arc<Context>,
        mut rx: Receiver<()>,
        msg: Id<MessageMarker>,
        channel: Id<ChannelMarker>,
    ) {
        static IDLE_TIMEOUT: Duration =
            Duration::from_secs(crate::core::events::interaction::COMPONENT_IDLE_TIMEOUT_SECS);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    res = rx.changed() => if res.is_ok() { continue } else { return },
                    _ = sleep(IDLE_TIMEOUT) => {
                        let pagination_active = ctx.paginations.lock(&msg).await.remove().is_some();

                        if pagination_active {
                            let builder = MessageBuilder::new().components(Vec::new());

                            if let Err(err) = (msg, channel).update(&ctx, &builder).await {
                                warn!("failed to remove components: {err:?}");
                            }
                        }

                        return;
                    },
                }
            }
        });
    }
}

pub struct PaginationBuilder {
    kind: PaginationKind,
    pages: Pages,
}

impl PaginationBuilder {
    pub(crate) fn new(kind: PaginationKind, pages: Pages) -> Self {
        Self { kind, pages }
    }

    /// Start the pagination
    pub async fn start(self, ctx: Arc<Context>, command: InteractionCommand) -> Result<()> {
        Pagination::start(ctx, command, self).await
    }
}

#[derive(Clone, Debug)]
pub struct Pages {
    pub index: usize,
    last_index: usize,
    pub per_page: usize,
}

impl Pages {
    /// `per_page`: How many entries per page
    ///
    /// `amount`: How many entries in total
    pub fn new(per_page: usize, amount: usize) -> Self {
        Self {
            index: 0,
            per_page,
            last_index: last_multiple(per_page, amount),
        }
    }

    pub fn curr_page(&self) -> usize {
        self.index / self.per_page + 1
    }

    pub fn last_page(&self) -> usize {
        self.last_index / self.per_page + 1
    }

    fn components(&self) -> Vec<Component> {
        if self.last_index == 0 {
            return Vec::new();
        }

        let nav = |id, emoji: &str, disabled| {
            emoji_button(
                id,
                EmojiReactionType::Unicode {
                    name: emoji.to_owned(),
                },
                ButtonStyle::Secondary,
                disabled,
            )
        };

        let at_start = self.index == 0;
        let at_end = self.index == self.last_index;

        vec![action_row(vec![
            nav("pagination_start", "⏮️", at_start),
            nav("pagination_back", "⏪", at_start),
            nav("pagination_custom", "*️⃣", false),
            nav("pagination_step", "⏩", at_end),
            nav("pagination_end", "⏭️", at_end),
        ])]
    }
}
