use twilight_model::{
    guild::Permissions,
    id::{
        marker::{ChannelMarker, GuildMarker, UserMarker},
        Id,
    },
};

use crate::core::{BotConfig, Context};

/// Is authority -> None
/// No authority -> Some(message to user)
pub async fn check_authority_with_perms(
    ctx: &Context,
    author: Id<UserMarker>,
    channel: Id<ChannelMarker>,
    guild: Option<Id<GuildMarker>>,
    interaction_perms: Option<Permissions>,
) -> Option<&'static str> {
    if BotConfig::get().owners.contains(&author) {
        return None;
    }

    if guild.is_none() {
        return Some("This command can only be used in a server.");
    }

    let permissions = interaction_perms
        .unwrap_or_else(|| ctx.cache.get_channel_permissions(author, channel, guild));

    if permissions.intersects(Permissions::ADMINISTRATOR | Permissions::MANAGE_CHANNELS) {
        None
    } else {
        let content =
            "You need admin permission or manage channels permission to use this command.";

        Some(content)
    }
}

/// Is authority -> None
/// No authority -> Some(message to user)
#[allow(dead_code)]
pub async fn check_authority(
    ctx: &Context,
    author: Id<UserMarker>,
    channel: Id<ChannelMarker>,
    guild: Option<Id<GuildMarker>>,
) -> Option<&'static str> {
    check_authority_with_perms(ctx, author, channel, guild, None).await
}
