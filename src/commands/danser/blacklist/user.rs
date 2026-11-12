use std::sync::Arc;

use eyre::Result;

use crate::{
    commands::danser::{BlacklistUserAdd, BlacklistUserRemove},
    core::Context,
    util::{builder::MessageBuilder, interaction::InteractionCommand, InteractionCommandExt},
};

/// Parse a raw Discord ID or `<@id>` / `<@!id>` mention.
pub(crate) fn parse_discord_id(input: &str) -> Option<u64> {
    let trimmed = input
        .trim()
        .trim_start_matches("<@!")
        .trim_start_matches("<@")
        .trim_end_matches('>');
    trimmed.parse::<u64>().ok().filter(|&id| id > 0)
}

pub async fn add(
    ctx: Arc<Context>,
    command: InteractionCommand,
    args: BlacklistUserAdd,
) -> Result<()> {
    let Some(user_id) = parse_discord_id(&args.user) else {
        command
            .error_callback(&ctx, "User ID or mention is invalid!", true)
            .await?;
        return Ok(());
    };

    ctx.psql().blacklist_user(user_id, args.reason).await?;
    let builder = MessageBuilder::new().embed(format!("User `{user_id}` successfully blacklisted"));
    command.callback(&ctx, builder, false).await?;
    Ok(())
}

pub async fn remove(
    ctx: Arc<Context>,
    command: InteractionCommand,
    args: BlacklistUserRemove,
) -> Result<()> {
    let Some(user_id) = parse_discord_id(&args.user) else {
        command
            .error_callback(&ctx, "User ID or mention is invalid!", true)
            .await?;
        return Ok(());
    };

    if ctx.psql().whitelist_user(user_id).await? {
        let builder =
            MessageBuilder::new().embed(format!("User `{user_id}` successfully whitelisted"));
        command.callback(&ctx, builder, false).await?;
    } else {
        command
            .error_callback(&ctx, format!("User `{user_id}` not found"), true)
            .await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_discord_id;

    #[test]
    fn test_parse_discord_id() {
        assert_eq!(parse_discord_id("123456789012345678"), Some(123456789012345678));
        assert_eq!(parse_discord_id("<@123456789012345678>"), Some(123456789012345678));
        assert_eq!(parse_discord_id(" <@!123456789012345678> "), Some(123456789012345678));
        assert_eq!(parse_discord_id("0"), None);
        assert_eq!(parse_discord_id("not_an_id"), None);
    }
}
