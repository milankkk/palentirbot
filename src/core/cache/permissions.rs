use twilight_model::{
    channel::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    guild::Permissions,
    id::{
        marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
        Id,
    },
};

use crate::core::BotConfig;

use super::Cache;

impl Cache {
    fn get_guild_permissions(
        &self,
        user: Id<UserMarker>,
        guild: Id<GuildMarker>,
    ) -> (Permissions, Option<Vec<Id<RoleMarker>>>) {
        if BotConfig::get().owners.contains(&user) {
            return (Permissions::all(), None);
        }

        match self.is_guild_owner(guild, user) {
            Ok(false) => {}
            Ok(true) => return (Permissions::all(), None),
            Err(_) => return (Permissions::empty(), None),
        }

        let member_roles = match self.member(guild, user, |m| m.roles().to_owned()) {
            Ok(roles) => roles,
            Err(_) => return (Permissions::empty(), None),
        };

        // Discord excludes the @everyone role (id == guild_id) from member.roles,
        // so seed base permissions from it before merging explicit roles.
        let mut permissions = self
            .role(guild.cast(), |r| r.permissions)
            .unwrap_or_else(|_| Permissions::empty());

        if permissions.contains(Permissions::ADMINISTRATOR) {
            return (Permissions::all(), Some(member_roles));
        }

        for &role in &member_roles {
            if let Ok(role_permissions) = self.role(role, |r| r.permissions) {
                if role_permissions.contains(Permissions::ADMINISTRATOR) {
                    return (Permissions::all(), Some(member_roles));
                }

                permissions |= role_permissions;
            }
        }

        (permissions, Some(member_roles))
    }

    pub fn get_channel_permissions(
        &self,
        user: Id<UserMarker>,
        channel: Id<ChannelMarker>,
        guild: Option<Id<GuildMarker>>,
    ) -> Permissions {
        let guild = match guild {
            Some(guild) => guild,
            None => return Permissions::all(),
        };

        let (mut permissions, roles) = self.get_guild_permissions(user, guild);

        if permissions.contains(Permissions::ADMINISTRATOR) {
            return Permissions::all();
        }

        match self.channel(channel, |c| {
            if let (Some(overwrites), Some(roles)) =
                (c.permission_overwrites.as_deref(), roles.as_deref())
            {
                Self::text_channel_permissions(&mut permissions, user, guild, overwrites, roles);
                None
            } else if c.permission_overwrites.is_none() {
                c.parent_id
            } else {
                None
            }
        }) {
            Ok(Some(parent_id)) => {
                if let Some(roles) = roles.as_deref() {
                    let _ = self.channel(parent_id, |c| {
                        if let Some(overwrites) = c.permission_overwrites.as_deref() {
                            Self::text_channel_permissions(
                                &mut permissions,
                                user,
                                guild,
                                overwrites,
                                roles,
                            );
                        }
                    });
                }
            }
            Ok(None) => {}
            Err(_) => return Permissions::empty(),
        }

        if !permissions.contains(Permissions::VIEW_CHANNEL) {
            return Permissions::empty();
        }
        if !permissions.contains(Permissions::SEND_MESSAGES) {
            permissions.remove(
                Permissions::EMBED_LINKS
                    | Permissions::ATTACH_FILES
                    | Permissions::MENTION_EVERYONE
                    | Permissions::SEND_TTS_MESSAGES,
            );
        }

        permissions
    }

    fn text_channel_permissions(
        permissions: &mut Permissions,
        user: Id<UserMarker>,
        guild: Id<GuildMarker>,
        permission_overwrites: &[PermissionOverwrite],
        roles: &[Id<RoleMarker>],
    ) {
        let mut everyone_allowed = Permissions::empty();
        let mut everyone_denied = Permissions::empty();
        let mut user_allowed = Permissions::empty();
        let mut user_denied = Permissions::empty();
        let mut role_allowed = Permissions::empty();
        let mut role_denied = Permissions::empty();

        for overwrite in permission_overwrites {
            match overwrite.kind {
                PermissionOverwriteType::Member => {
                    if overwrite.id.cast() == user {
                        user_allowed |= overwrite.allow;
                        user_denied |= overwrite.deny;
                    }
                }
                PermissionOverwriteType::Role => {
                    if overwrite.id.cast() == guild {
                        everyone_allowed |= overwrite.allow;
                        everyone_denied |= overwrite.deny
                    } else if roles.contains(&overwrite.id.cast()) {
                        role_allowed |= overwrite.allow;
                        role_denied |= overwrite.deny;
                    }
                }
                _ => {}
            }
        }

        *permissions &= !everyone_denied;
        *permissions |= everyone_allowed;

        *permissions &= !role_denied;
        *permissions |= role_allowed;

        *permissions &= !user_denied;
        *permissions |= user_allowed;
    }
}
