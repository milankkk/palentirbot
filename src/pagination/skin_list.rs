use std::fmt::Write;

use twilight_model::channel::message::embed::Embed;

use crate::util::builder::EmbedBuilder;

use super::{Pages, PaginationBuilder, PaginationKind};

pub struct SkinListPagination {
    skins: Vec<String>,
}

impl SkinListPagination {
    pub fn builder(skins: Vec<String>) -> PaginationBuilder {
        let pages = Pages::new(15, skins.len());
        let kind = PaginationKind::SkinList(Box::new(Self { skins }));

        PaginationBuilder::new(kind, pages)
    }

    pub fn build_page(&mut self, pages: &Pages) -> Embed {
        let mut description = String::with_capacity(256);

        let skins = self
            .skins
            .iter()
            .skip(pages.index)
            .take(pages.per_page)
            .zip(pages.index + 1..);

        for (skin, idx) in skins {
            let _ = writeln!(description, "{idx}) {skin}");
        }

        let page = pages.curr_page();
        let pages = pages.last_page();

        let footer_text = format!("Page {page}/{pages}");

        EmbedBuilder::new()
            .description(description)
            .footer(footer_text)
            .build()
    }
}
