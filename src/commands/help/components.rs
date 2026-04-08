use std::{fmt::Write, mem};

use eyre::{ContextCompat, Result};
use twilight_interactions::command::{ApplicationCommandData, CommandOptionExtended};


use crate::{
    core::{
        commands::slash::{Command, Commands, SlashCommand},
        Context,
    },
    util::{
        builder::{EmbedBuilder, FooterBuilder, MessageBuilder},
        interaction::InteractionComponent,
        Authored, ComponentExt,
    },
};

use super::{generate_menus, option_fields};

const AUTHORITY_STATUS: &str = "Requires authority status";

type PartResult = Result<(Parts, bool)>;

struct Parts {
    name: String,
    help: String,
    options: Vec<CommandOptionExtended>,
}

impl From<&'static SlashCommand> for Parts {
    fn from(command: &'static SlashCommand) -> Self {
        let command = (command.create)();

        Self {
            name: command.name,
            help: command.help.map(|s| s.to_string()).unwrap_or(command.description),
            options: command.options,
        }
    }
}

impl From<CommandOptionExtended> for Parts {
    fn from(option: CommandOptionExtended) -> Self {
        use twilight_model::application::command::CommandOptionType;

        let options = match option.kind {
            CommandOptionType::SubCommand
            | CommandOptionType::SubCommandGroup => option.options.unwrap_or_default(),
            _ => Vec::new(),
        };


        Self {
            name: option.name.clone(),
            help: option.help.map(|s| s.to_string()).unwrap_or_else(|| option.description.clone()),
            options,
        }
    }
}


impl From<EitherCommand> for Parts {
    fn from(either: EitherCommand) -> Self {
        match either {
            EitherCommand::Base(command) => command.into(),
            EitherCommand::Option(option) => (*option).into(),
        }
    }
}

impl From<CommandIter> for Parts {
    fn from(iter: CommandIter) -> Self {
        match iter.next {
            Some(option) => option.into(),
            None => iter.curr.into(),
        }
    }
}

enum EitherCommand {
    Base(&'static SlashCommand),
    Option(Box<CommandOptionExtended>),
}

struct CommandIter {
    curr: EitherCommand,
    next: Option<CommandOptionExtended>,
}

impl From<&'static SlashCommand> for CommandIter {
    fn from(command: &'static SlashCommand) -> Self {
        Self {
            curr: EitherCommand::Base(command),
            next: None,
        }
    }
}

impl CommandIter {
    fn next(&mut self, name: &str) -> bool {
        use twilight_model::application::command::CommandOptionType;

        let options = match &mut self.next {
            Some(option) => match option.kind {
                CommandOptionType::SubCommand
                | CommandOptionType::SubCommandGroup => {
                    mem::take(&mut option.options).unwrap_or_default()
                }
                _ => return true,
            },
            None => match &mut self.curr {
                EitherCommand::Base(command) => (command.create)().options,
                EitherCommand::Option(option) => match option.kind {
                    CommandOptionType::SubCommand
                    | CommandOptionType::SubCommandGroup => {
                        mem::take(&mut option.options).unwrap_or_default()
                    }
                    _ => return true,
                },
            },
        };

        let next = match options.into_iter().find(|o| o.name == name) {
            Some(option) => option,
            None => return true,
        };

        if let Some(curr) = self.next.replace(next) {
            self.curr = EitherCommand::Option(Box::new(curr));
        }

        false
    }
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

    let ApplicationCommandData {
        name,
        description,
        help,
        options,
        ..
    } = (cmd.create)();

    let description = help.map(|s| s.to_string()).unwrap_or_else(|| description.clone());


    let mut embed = EmbedBuilder::new()
        .title(name)
        .description(description)
        .fields(option_fields(&options));

    if cmd.flags.authority() {
        let footer = FooterBuilder::new(AUTHORITY_STATUS);
        embed = embed.footer(footer);
    }

    let menus = generate_menus(component.user_id()?, &options);
    let builder = MessageBuilder::new().embed(embed).components(menus);

    component.callback(ctx, builder).await?;

    Ok(())
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

    let name = component
        .data
        .values
        .first()
        .with_context(|| format!("missing subcommand for `{title}`"))?;

    let (command, authority) = continue_subcommand(&mut title, name)?;

    // Prepare embed and components
    let mut embed_builder = EmbedBuilder::new()
        .title(title)
        .description(command.help)
        .fields(option_fields(&command.options));

    if authority {
        embed_builder = embed_builder.footer(FooterBuilder::new(AUTHORITY_STATUS));
    }

    let components = generate_menus(component.user_id()?, &command.options);

    let builder = MessageBuilder::new()
        .embed(embed_builder)
        .components(components);

    component.callback(ctx, builder).await?;

    Ok(())
}

fn continue_subcommand(title: &mut String, name: &str) -> PartResult {
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
    let mut iter = CommandIter::from(command);

    for name in names {
        if iter.next(name) {
            bail!("unknown command");
        }
    }

    if iter.next(name) {
        bail!("unknown command");
    }

    let command = Parts::from(iter);
    let _ = write!(title, " {}", command.name);

    Ok((command, authority))
}
