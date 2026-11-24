use std::sync::Arc;

use eyre::{ContextCompat, Result};

use crate::{
    core::Context,
    util::{
        builder::{MessageBuilder, ModalBuilder},
        interaction::{InteractionComponent, InteractionModal},
        Authored, ComponentExt, ModalExt,
    },
};

use super::Pages;

pub(super) async fn remove_components(
    ctx: &Context,
    component: &InteractionComponent,
) -> Result<()> {
    let builder = MessageBuilder::new()
        .components(Vec::new())
        .content(&component.message.content);

    component.callback(ctx, builder).await?;

    Ok(())
}

async fn respond_not_author(ctx: &Context, component: &InteractionComponent) -> Result<()> {
    component
        .ephemeral_text(
            ctx,
            "Only the command author can use these pagination buttons.",
        )
        .await
}

pub async fn handle_pagination_component(
    ctx: Arc<Context>,
    component: InteractionComponent,
    page_fn: fn(&mut Pages),
) -> Result<()> {
    let builder = {
        let mut guard = ctx.paginations.lock(&component.message.id).await;

        let Some(pagination) = guard.get_mut() else {
            drop(guard);
            return remove_components(&ctx, &component).await;
        };

        if !pagination.is_author(component.user_id()?) {
            drop(guard);
            return respond_not_author(&ctx, &component).await;
        }

        pagination.reset_timeout();
        page_fn(&mut pagination.pages);
        pagination.build()
    };

    component.callback(&ctx, builder).await
}

pub async fn handle_pagination_start(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let f = |pages: &mut Pages| pages.index = 0;

    handle_pagination_component(ctx, component, f).await
}

pub async fn handle_pagination_back(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let f = |pages: &mut Pages| pages.index = pages.index.saturating_sub(pages.per_page);

    handle_pagination_component(ctx, component, f).await
}

pub async fn handle_pagination_step(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let f = |pages: &mut Pages| pages.index = (pages.index + pages.per_page).min(pages.last_index);

    handle_pagination_component(ctx, component, f).await
}

pub async fn handle_pagination_end(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let f = |pages: &mut Pages| pages.index = pages.last_index;

    handle_pagination_component(ctx, component, f).await
}

pub async fn handle_pagination_custom(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let max_page = {
        let guard = ctx.paginations.lock(&component.message.id).await;

        if let Some(pagination) = guard.get() {
            if !pagination.is_author(component.user_id()?) {
                drop(guard);
                return respond_not_author(&ctx, &component).await;
            }

            pagination.reset_timeout();

            pagination.pages.last_page()
        } else {
            drop(guard);
            return remove_components(&ctx, &component).await;
        }
    };

    let placeholder = format!("Number between 1 and {max_page}");

    let modal = ModalBuilder::new("page_input", "Page number")
        .modal_id("pagination_page")
        .min_len(1)
        .max_len(5)
        .placeholder(placeholder)
        .title("Jump to a page");

    component.modal(&ctx, modal).await?;

    Ok(())
}

pub async fn handle_pagination_modal(ctx: Arc<Context>, modal: InteractionModal) -> Result<()> {
    let input = modal
        .data
        .components
        .first()
        .and_then(|component| component.components.first())
        .context("missing input for page number")?;

    let page: usize = if let Some(Ok(n)) = input.value.as_deref().map(|s| s.trim().parse()) {
        n
    } else {
        debug!("failed to parse page input `{:?}` as usize", input.value);
        let _ = modal
            .callback(
                &ctx,
                MessageBuilder::new().embed("Please enter a valid positive page number."),
            )
            .await;

        return Ok(());
    };

    let builder = if let Some(ref msg) = modal.message {
        let mut guard = ctx.paginations.lock(&msg.id).await;

        let Some(pagination) = guard.get_mut() else {
            let _ = modal
                .callback(
                    &ctx,
                    MessageBuilder::new().embed("This pagination session has expired."),
                )
                .await;
            return Ok(());
        };

        if !pagination.is_author(modal.user_id()?) {
            let _ = modal
                .callback(
                    &ctx,
                    MessageBuilder::new().embed("Only the command author can change the page."),
                )
                .await;
            return Ok(());
        }

        let max_page = pagination.pages.last_page();

        if !(1..=max_page).contains(&page) {
            debug!("page {page} is not between 1 and {max_page}");
            let _ = modal
                .callback(
                    &ctx,
                    MessageBuilder::new().embed(format!("Page must be between 1 and {max_page}.")),
                )
                .await;

            return Ok(());
        }

        pagination.reset_timeout();
        pagination.pages.index = (page - 1) * pagination.pages.per_page;

        pagination.build()
    } else {
        warn!(
            "received unexpected modal (has msg: {})",
            modal.message.is_some()
        );
        let _ = modal
            .callback(
                &ctx,
                MessageBuilder::new().embed("Cannot determine which message to paginate."),
            )
            .await;

        return Ok(());
    };

    modal.update_callback(&ctx, builder).await?;

    Ok(())
}
