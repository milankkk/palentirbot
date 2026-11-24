use crate::{
    core::{
        commands::slash::{Command, Commands},
        BotConfig,
    },
    util::builder::text_select_row,
};
use twilight_interactions::command::CommandOptionExtended;
use twilight_model::channel::message::component::{Component, SelectMenuOption};
use twilight_model::channel::message::embed::EmbedField;
use twilight_model::id::{marker::UserMarker, Id};

pub use self::{
    components::{handle_help_basecommand, handle_help_subcommand},
    interaction::{Help, HELP_SLASH},
};

mod components;
mod interaction;

fn generate_menus(user: Id<UserMarker>, options: &[CommandOptionExtended]) -> Vec<Component> {
    let is_owner = BotConfig::get().owners.contains(&user);
    let base_options: Vec<_> = Commands::get().filter_collect(|c| {
        let only_owner = match c {
            Command::Slash(slash) => slash.flags.only_owner(),
            Command::Message(_) => false,
        };
        let cmd = c.create();
        let name = cmd.name;
        let description = cmd.description;

        if description.is_empty() || ((only_owner || name == "owner") && !is_owner) {
            None
        } else {
            Some(SelectMenuOption {
                default: false,
                description: Some(description),
                emoji: None,
                label: name.clone(),
                value: name,
            })
        }
    });

    let base_menu = text_select_row("help_basecommand", "Select a base command", base_options);

    match parse_subcommand_menu(options) {
        Some(sub_menu) => vec![base_menu, sub_menu],
        None => vec![base_menu],
    }
}

fn parse_subcommand_menu(options: &[CommandOptionExtended]) -> Option<Component> {
    if options.is_empty() {
        return None;
    }

    let options: Vec<_> = options
        .iter()
        .filter_map(|option| {
            use twilight_model::application::command::CommandOptionType;
            match option.kind {
                CommandOptionType::SubCommand | CommandOptionType::SubCommandGroup => {
                    Some(SelectMenuOption {
                        default: false,
                        description: Some(option.description.clone()),
                        emoji: None,
                        label: option.name.clone(),
                        value: option.name.clone(),
                    })
                }
                _ => None,
            }
        })
        .collect();

    (!options.is_empty()).then(|| text_select_row("help_subcommand", "Select a subcommand", options))
}

fn option_fields(children: &[CommandOptionExtended]) -> Vec<EmbedField> {
    use twilight_model::application::command::CommandOptionType;

    children
        .iter()
        .filter_map(|child| {
            match child.kind {
                CommandOptionType::SubCommand | CommandOptionType::SubCommandGroup => return None,
                _ => {}
            }

            let mut name: String = child.name.to_owned();
            if child.required.unwrap_or(false) {
                name.push_str(" (required)");
            }

            let value: String = child
                .help
                .as_ref()
                .map(|h| h.to_string())
                .unwrap_or_else(|| child.description.clone());

            Some(EmbedField {
                inline: value.len() <= 37,
                name,
                value,
            })
        })
        .collect()
}
