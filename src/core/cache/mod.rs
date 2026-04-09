use std::{collections::HashMap, iter::FromIterator};

use eyre::{ContextCompat, Result};
use twilight_cache_inmemory::{
    model::{CachedGuild, CachedMember},
    InMemoryCache, InMemoryCacheStats, ResourceType,
};
use twilight_gateway::Event;
use twilight_model::{
    channel::Channel,
    guild::Role,
    id::{
        marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
        Id,
    },
    user::{CurrentUser, User},
};


mod permissions;

pub struct Cache {
    inner: InMemoryCache,
    current_user: Option<CurrentUser>,
}

impl Cache {
    pub async fn new() -> (Self, ResumeData) {
        let resource_types = ResourceType::CHANNEL
            | ResourceType::GUILD
            | ResourceType::MEMBER
            | ResourceType::ROLE
            | ResourceType::USER_CURRENT;

        let inner = InMemoryCache::builder()
            .message_cache_size(0)
            .resource_types(resource_types)
            .build();

        let cache = Self {
            inner,
            current_user: None,
        };

        (cache, ResumeData::default())
    }

    pub fn update(&self, event: &Event) {
        self.inner.update(event)
    }

    pub fn stats(&self) -> InMemoryCacheStats<'_> {
        self.inner.stats()
    }

    pub fn channel<F, T>(&self, channel: Id<ChannelMarker>, f: F) -> Result<T>
    where
        F: FnOnce(&Channel) -> T,
    {
        let channel = self
            .inner
            .channel(channel)
            .with_context(|| format!("missing channel {channel}"))?;

        Ok(f(&channel))
    }

    pub fn current_user<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&CurrentUser) -> T,
    {
        let user = self
            .inner
            .current_user()
            .context("missing current user")?;

        Ok(f(&user))
    }

    pub fn guild<F, T>(&self, guild: Id<GuildMarker>, f: F) -> Result<T>
    where
        F: FnOnce(&CachedGuild) -> T,
    {
        let guild = self
            .inner
            .guild(guild)
            .with_context(|| format!("missing guild {guild}"))?;

        Ok(f(&guild))
    }

    pub fn member<F, T>(&self, guild: Id<GuildMarker>, user: Id<UserMarker>, f: F) -> Result<T>
    where
        F: FnOnce(&CachedMember) -> T,
    {
        let member = self
            .inner
            .member(guild, user)
            .with_context(|| format!("missing member {user} in guild {guild}"))?;

        Ok(f(&member))
    }

    #[allow(unused)]
    pub fn members<F, T, C>(&self, guild: Id<GuildMarker>, f: F) -> C
    where
        C: Default + FromIterator<T>,
        F: Fn(&Id<UserMarker>) -> T,
    {
        self.inner
            .guild_members(guild)
            .map_or_else(C::default, |entry| entry.iter().map(f).collect())
    }

    pub fn role<F, T>(&self, role: Id<RoleMarker>, f: F) -> Result<T>
    where
        F: FnOnce(&Role) -> T,
    {
        let role = self
            .inner
            .role(role)
            .with_context(|| format!("missing role {role}"))?;

        Ok(f(&role))
    }

    #[allow(unused)]
    pub fn user<F, T>(&self, user: Id<UserMarker>, f: F) -> Result<T>
    where
        F: FnOnce(&User) -> T,
    {
        let user = self
            .inner
            .user(user)
            .with_context(|| format!("missing user {user}"))?;

        Ok(f(&user))
    }

    pub fn is_guild_owner(&self, guild: Id<GuildMarker>, user: Id<UserMarker>) -> Result<bool> {
        self.guild(guild, |g| g.owner_id() == user)
    }
}

type ResumeData = HashMap<u64, String>;
