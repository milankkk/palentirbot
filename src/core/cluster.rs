use std::sync::Arc;

use eyre::{Context as _, Result};
use twilight_gateway::{Config, Intents, MessageSender, Shard, create_recommended};
use twilight_http::Client;
use twilight_model::gateway::{
    payload::outgoing::update_presence::UpdatePresencePayload,
    presence::{ActivityType, MinimalActivity, Status},
};

pub async fn build_shards(
    token: &str,
    http: Arc<Client>,
) -> Result<(Vec<Shard>, Vec<MessageSender>)> {
    let intents = Intents::GUILDS
        | Intents::GUILD_MEMBERS
        | Intents::GUILD_MESSAGES
        | Intents::DIRECT_MESSAGES
        | Intents::MESSAGE_CONTENT;

    let activity = MinimalActivity {
        kind: ActivityType::Playing,
        name: String::new(),
        url: None,
    };

    let _presence =
        UpdatePresencePayload::new([activity.into()], false, None, Status::Online).unwrap();

    let config = Config::new(token.to_owned(), intents);

    let shards: Vec<Shard> = create_recommended(&*http, config, |_, builder| builder.build())
        .await
        .context("failed to build shards")?
        .collect();

    let senders = shards.iter().map(|s| s.sender()).collect();

    Ok((shards, senders))
}
