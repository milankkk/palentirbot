use std::fmt::Write;

use eyre::{ContextCompat, Result};
use twilight_interactions::command::{ApplicationCommandData, CommandOptionExtended};
use twilight_model::{
    application::command::CommandOptionType,
    id::{marker::UserMarker, Id},
};

use crate::{
    core::{
        commands::slash::{Command, Commands},
        BotConfig, Context,
    },
    util::{
        builder::{EmbedBuilder, MessageBuilder},
        constants::NOT_OWNER,
        interaction::InteractionComponent,
        Authored, ComponentExt,
    },
};

use super::{generate_menus, option_fields};

const AUTHORITY_STATUS: &str = "Requires authority status";

async fn send_help_view(
    ctx: &Context,
    component: &InteractionComponent,
    user_id: Id<UserMarker>,
    title: String,
    description: String,
    options: Vec<CommandOptionExtended>,
    authority: bool,
) -> Result<()> {
    let mut embed = EmbedBuilder::new()
        .title(title)
        .description(description)
        .fields(option_fields(&options));

    if authority {
        embed = embed.footer(AUTHORITY_STATUS);
    }

    let components = generate_menus(user_id, &options);
    let builder = MessageBuilder::new().embed(embed).components(components);
    component.callback(ctx, builder).await
}

pub async fn handle_help_basecommand(ctx: &Context, component: InteractionComponent) -> Result<()> {
    let name = component
        .data
        .values
        .first()
        .context("no menu option was selected")?;

    let cmd = Commands::get()
        .command(name)
        .and_then(|cmd| match cmd {
            Command::Slash(cmd) => Some(cmd),
            Command::Message(_) => None,
        })
        .with_context(|| format!("missing slash command `{name}`"))?;

    let user_id = component.user_id()?;
    if cmd.flags.only_owner() && !BotConfig::get().owners.contains(&user_id) {
        let builder = MessageBuilder::new().embed(NOT_OWNER);
        return component.callback(ctx, builder).await;
    }

    let ApplicationCommandData {
        name,
        description,
        help,
        options,
        ..
    } = (cmd.create)();

    let description = help.map_or(description, |s| s.to_string());
    send_help_view(
        ctx,
        &component,
        user_id,
        name,
        description,
        options,
        cmd.flags.authority(),
    )
    .await
}

pub async fn handle_help_subcommand(
    ctx: &Context,
    mut component: InteractionComponent,
) -> Result<()> {
    let mut title = component
        .message
        .embeds
        .pop()
        .context("missing embed")?
        .title
        .context("missing embed title")?;

    let selected = component
        .data
        .values
        .first()
        .with_context(|| format!("missing subcommand for `{title}`"))?;

    let (help, options, authority) = resolve_subcommand(&mut title, selected)?;
    let user_id = component.user_id()?;
    send_help_view(ctx, &component, user_id, title, help, options, authority).await
}

fn resolve_subcommand(
    title: &mut String,
    selected: &str,
) -> Result<(String, Vec<CommandOptionExtended>, bool)> {
    let mut names = title.split(' ');
    let base = names.next().context("missing embed title")?;

    let command = Commands::get()
        .command(base)
        .and_then(|cmd| match cmd {
            Command::Slash(cmd) => Some(cmd),
            Command::Message(_) => None,
        })
        .context("unknown command")?;

    let authority = command.flags.authority();
    let mut options = (command.create)().options;
    let mut found = None;

    for segment in names.chain(std::iter::once(selected)) {
        let opt = options
            .into_iter()
            .find(|o| o.name == segment)
            .context("unknown command")?;

        options = match opt.kind {
            CommandOptionType::SubCommand | CommandOptionType::SubCommandGroup => {
                opt.options.unwrap_or_default()
            }
            _ => Vec::new(),
        };
        found = Some((opt.name, opt.help.map_or(opt.description, |s| s.to_string())));
    }

    let (leaf_name, help) = found.context("unknown command")?;
    let _ = write!(title, " {leaf_name}");

    Ok((help, options, authority))
}
