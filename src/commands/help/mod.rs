use twilight_interactions::command::CommandOptionExtended;
use twilight_model::channel::message::component::{
    ActionRow, Component, SelectMenu, SelectMenuOption,
};
use twilight_model::channel::message::embed::EmbedField;
use twilight_model::id::{marker::UserMarker, Id};
use crate::core::{commands::slash::Commands, BotConfig};



pub use self::{
    components::{handle_help_basecommand, handle_help_subcommand},
    interaction::{Help, HELP_SLASH},
};

mod components;
mod interaction;

fn generate_menus(user: Id<UserMarker>, options: &[CommandOptionExtended]) -> Vec<Component> {
    let base_options: Vec<_> = Commands::get().filter_collect(|c| {
        let cmd = c.create();
        let name = cmd.name;
        let description = cmd.description;

        if description.is_empty() || (name == "owner" && !BotConfig::get().owners.contains(&user)) {
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

    let select_menu = SelectMenu {
        custom_id: "help_basecommand".to_owned(),
        disabled: false,
        max_values: None,
        min_values: None,
        options: Some(base_options),
        placeholder: Some("Select a base command".to_owned()),
        channel_types: None,
        default_values: None,
        kind: twilight_model::channel::message::component::SelectMenuType::Text,
    };



    let row = ActionRow {
        components: vec![Component::SelectMenu(select_menu)],
    };

    let base_menu = Component::ActionRow(row);

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
                CommandOptionType::SubCommand
                | CommandOptionType::SubCommandGroup => Some((&option.name, &option.description)),
                _ => None,
            }
        })
        .map(|(name, description): (&String, &String)| SelectMenuOption {
            default: false,
            description: Some(description.to_owned()),
            emoji: None,
            label: name.to_owned(),
            value: name.to_owned(),
        })
        .collect();

    if options.is_empty() {
        return None;
    }

    let select_menu = SelectMenu {
        custom_id: "help_subcommand".to_owned(),
        disabled: false,
        max_values: None,
        min_values: None,
        options: Some(options),
        placeholder: Some("Select a subcommand".to_owned()),
        channel_types: None,
        default_values: None,
        kind: twilight_model::channel::message::component::SelectMenuType::Text,
    };

    let row = ActionRow {
        components: vec![Component::SelectMenu(select_menu)],
    };

    Some(Component::ActionRow(row))
}

fn option_fields(children: &[CommandOptionExtended]) -> Vec<EmbedField> {
    use twilight_model::application::command::CommandOptionType;

    children
        .iter()
        .filter_map(|child| {
            match child.kind {
                CommandOptionType::SubCommand
                | CommandOptionType::SubCommandGroup => return None,
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

