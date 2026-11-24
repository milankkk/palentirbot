use std::pin::Pin;

use eyre::Result;
use futures::Future;
use twilight_interactions::command::CreateCommand;

use crate::commands::{danser::*, owner::*, utility::*};

pub use self::command::{Command, MessageCommand, SlashCommand};
use crate::commands::help::{Help, HELP_SLASH};

mod command;

macro_rules! command_table {
    (
        $(msg: $msg_cmd:ident,)*
        $(slash: $slash_cmd:ident => $chat_fun:ident,)*
    ) => {
        &[
            $(($msg_cmd.name, Command::Message(&$msg_cmd)),)*
            $(($slash_cmd::NAME, Command::Slash(&$chat_fun)),)*
        ]
    };
}

// Keep entries strictly sorted in ascending ASCII order for binary search and `/help` ordering.
static COMMANDS_LIST: &[(&str, Command)] = command_table! {
    msg: RENDER_FROM_MSG,
    slash: AiClear => AICLEAR_SLASH,
    slash: AiContext => AICONTEXT_SLASH,
    slash: Ask => ASK_SLASH,
    slash: Blacklist => BLACKLIST_SLASH,
    slash: Help => HELP_SLASH,
    slash: Invite => INVITE_SLASH,
    slash: Link => LINK_SLASH,
    slash: Owner => OWNER_SLASH,
    slash: Ping => PING_SLASH,
    slash: Queue => QUEUE_SLASH,
    slash: Render => RENDER_SLASH,
    slash: Rr => RR_SLASH,
    slash: Rrn => RRN_SLASH,
    slash: Rtop => RTOP_SLASH,
    slash: Screenshot => SCREENSHOT_SLASH,
    slash: Settings => SETTINGS_SLASH,
    slash: Setup => SETUP_SLASH,
    slash: Skin => SKIN_SLASH,
};

static COMMANDS: Commands = Commands(COMMANDS_LIST);

pub struct Commands(&'static [(&'static str, Command)]);

pub type CommandResult = Pin<Box<dyn Future<Output = Result<()>> + 'static + Send>>;

impl Commands {
    pub fn get() -> &'static Self {
        &COMMANDS
    }

    pub fn command(&self, command: &str) -> Option<Command> {
        self.0
            .binary_search_by_key(&command, |&(name, _)| name)
            .ok()
            .map(|idx| self.0[idx].1)
    }

    pub fn collect<F, O>(&self, mut f: F) -> Vec<O>
    where
        F: FnMut(&Command) -> O,
    {
        self.0.iter().map(|(_, cmd)| f(cmd)).collect()
    }

    pub fn filter_collect<F, O>(&self, f: F) -> Vec<O>
    where
        F: FnMut(Command) -> Option<O>,
    {
        self.0.iter().map(|&(_, cmd)| cmd).filter_map(f).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_list_is_sorted_and_complete() {
        assert_eq!(COMMANDS_LIST.len(), 19);
        for pair in COMMANDS_LIST.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "COMMANDS_LIST out of order: {:?} >= {:?}",
                pair[0].0,
                pair[1].0
            );
        }
        assert!(Commands::get().command("ask").is_some());
        assert!(Commands::get().command("Render score").is_some());
        assert!(Commands::get().command("nonexistent").is_none());
    }
}
