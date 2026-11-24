use std::{io::Read as _, sync::Arc, time::Duration};

use eyre::Result;
use serde::Deserialize;
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker, UserMarker},
    Id,
};

use crate::{
    core::{
        web_render::{HttpRequest, HttpResponse},
        BotConfig, Context,
    },
    database::{DISCORD_LINK_TTL_SECS, WEB_SESSION_TTL_SECS},
};

const HOST_SESSION_COOKIE_NAME: &str = "__Host-osu_session";
const DEV_SESSION_COOKIE_NAME: &str = "osu_session";
const HOST_STATE_COOKIE_NAME: &str = "__Host-osu_oauth_state";
const DEV_STATE_COOKIE_NAME: &str = "osu_oauth_state";
const HOST_DISCORD_LINK_COOKIE_NAME: &str = "__Host-osu_discord_link";
const DEV_DISCORD_LINK_COOKIE_NAME: &str = "osu_discord_link";
const STATE_TTL_SECS: i64 = 600;
const DEFAULT_REDIRECT_URI: &str = "https://replays.insertdomainname.be/api/auth/osu/callback";
const OSU_AUTHORIZE_URL: &str = "https://osu.ppy.sh/oauth/authorize";
const OSU_TOKEN_URL: &str = "https://osu.ppy.sh/oauth/token";
const OSU_ME_URL: &str = "https://osu.ppy.sh/api/v2/me/osu";

const fn session_cookie_name(secure: bool) -> &'static str {
    if secure {
        HOST_SESSION_COOKIE_NAME
    } else {
        DEV_SESSION_COOKIE_NAME
    }
}

const fn state_cookie_name(secure: bool) -> &'static str {
    if secure {
        HOST_STATE_COOKIE_NAME
    } else {
        DEV_STATE_COOKIE_NAME
    }
}

const fn discord_link_cookie_name(secure: bool) -> &'static str {
    if secure {
        HOST_DISCORD_LINK_COOKIE_NAME
    } else {
        DEV_DISCORD_LINK_COOKIE_NAME
    }
}

struct OAuthConfig {
    client_id: u64,
    client_secret: String,
    redirect_uri: String,
}

fn load_oauth_config() -> OAuthConfig {
    let bot_cfg = BotConfig::get();
    let client_id = std::env::var("OSU_WEB_OAUTH_CLIENT_ID")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(bot_cfg.tokens.osu_client_id);
    let client_secret = std::env::var("OSU_WEB_OAUTH_CLIENT_SECRET")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| bot_cfg.tokens.osu_client_secret.clone());
    let redirect_uri = std::env::var("OSU_WEB_OAUTH_REDIRECT_URI")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_REDIRECT_URI.to_owned());

    OAuthConfig {
        client_id,
        client_secret,
        redirect_uri,
    }
}

fn should_set_secure_cookie(req: &HttpRequest, redirect_uri: &str) -> bool {
    if req
        .forwarded_proto
        .as_deref()
        .is_some_and(|p| p.eq_ignore_ascii_case("https"))
    {
        return true;
    }
    let is_local_host = req.host.as_deref().is_some_and(|h| {
        let bare = h.split(':').next().unwrap_or(h);
        bare.eq_ignore_ascii_case("localhost") || bare == "127.0.0.1"
    });
    if is_local_host {
        return false;
    }
    redirect_uri.starts_with("https://")
}

fn build_session_cookie(token: &str, expires_at: i64, secure: bool) -> String {
    let name = session_cookie_name(secure);
    let mut cookie = format!(
        "{name}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={WEB_SESSION_TTL_SECS}"
    );
    if let Some(http_date) = format_http_date(expires_at) {
        cookie.push_str("; Expires=");
        cookie.push_str(&http_date);
    }
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

fn build_clear_cookie(name: &str, path: &str, secure: bool) -> String {
    let mut cookie = format!(
        "{name}=; Path={path}; HttpOnly; SameSite=Lax; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

fn build_state_cookie(state: &str, return_to: &str, secure: bool) -> String {
    // `__Host-` prefix requires `Secure`, `Path=/`, and no `Domain` attribute so browsers
    // reject sibling-subdomain cookie tossing across `*.insertdomainname.be`.
    let name = state_cookie_name(secure);
    let encoded_ret = percent_encode_component(return_to);
    let mut cookie = format!(
        "{name}={state}.{encoded_ret}; Path=/; HttpOnly; SameSite=Lax; Max-Age={STATE_TTL_SECS}"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

fn build_discord_link_cookie(token: &str, secure: bool) -> String {
    let name = discord_link_cookie_name(secure);
    let mut cookie =
        format!("{name}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={STATE_TTL_SECS}");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

fn public_site_origin(redirect_uri: &str) -> String {
    if let Some(stripped) = redirect_uri.strip_suffix("/api/auth/osu/callback") {
        let trimmed = stripped.trim_end_matches('/');
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    if let Some(scheme_end) = redirect_uri.find("://") {
        let rest = &redirect_uri[scheme_end + 3..];
        let host = rest.split('/').next().unwrap_or("");
        if !host.is_empty() {
            return format!("{}{host}", &redirect_uri[..scheme_end + 3]);
        }
    }
    "https://replays.insertdomainname.be".to_owned()
}

/// Generate a single-use 10-minute verification URL for linking a Discord account via osu! OAuth.
pub(crate) async fn create_discord_link_url(
    ctx: &Context,
    discord_id: Id<UserMarker>,
    channel_id: Id<ChannelMarker>,
    guild_id: Option<Id<GuildMarker>>,
) -> Result<(String, [u8; 32])> {
    let token = generate_random_hex_32()?;
    let token_hash = sha256(token.as_bytes());
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let expires_at = now.saturating_add(DISCORD_LINK_TTL_SECS);

    ctx.psql()
        .create_discord_link_token(
            &token_hash,
            discord_id.get(),
            channel_id.get(),
            guild_id.map(Id::get),
            now,
            expires_at,
        )
        .await?;

    let oauth = load_oauth_config();
    let origin = public_site_origin(&oauth.redirect_uri);
    let url = format!("{origin}/api/auth/osu/discord-link?token={token}");
    Ok((url, token_hash))
}

pub(crate) async fn handle_osu_login(req: &HttpRequest) -> HttpResponse {
    let oauth = load_oauth_config();
    let secure = should_set_secure_cookie(req, &oauth.redirect_uri);
    let return_to = query_param(&req.query, "return_to")
        .as_deref()
        .map(sanitize_return_to)
        .unwrap_or_else(|| "/".to_owned());

    let state = match generate_random_hex_32() {
        Ok(s) => s,
        Err(err) => {
            warn!(?err, "Failed to generate OAuth state token");
            return HttpResponse::redirect(
                append_query_param(&return_to, "auth_error", "rng_failed"),
                Vec::new(),
            );
        }
    };

    let authorize_url = format!(
        "{OSU_AUTHORIZE_URL}?client_id={}&redirect_uri={}&response_type=code&scope=identify%20public&state={state}",
        oauth.client_id,
        percent_encode_component(&oauth.redirect_uri),
    );
    let state_cookie = build_state_cookie(&state, &return_to, secure);
    let clear_link = build_clear_cookie(discord_link_cookie_name(secure), "/", secure);
    HttpResponse::redirect(authorize_url, vec![state_cookie, clear_link])
}

pub(crate) async fn handle_discord_link_start(ctx: &Context, req: &HttpRequest) -> HttpResponse {
    let oauth = load_oauth_config();
    let secure = should_set_secure_cookie(req, &oauth.redirect_uri);

    let Some(token) = query_param(&req.query, "token").filter(|t| is_valid_hex_64(t)) else {
        return HttpResponse::redirect("/config?auth_error=link_expired".to_owned(), Vec::new());
    };

    let token_hash = sha256(token.as_bytes());
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    match ctx.psql().peek_discord_link_token(&token_hash, now).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return HttpResponse::redirect("/config?auth_error=link_expired".to_owned(), Vec::new());
        }
        Err(err) => {
            warn!(?err, "Failed checking pending Discord link token");
            return HttpResponse::redirect("/config?auth_error=db_error".to_owned(), Vec::new());
        }
    }

    let state = match generate_random_hex_32() {
        Ok(s) => s,
        Err(err) => {
            warn!(?err, "Failed generating OAuth state for Discord link");
            return HttpResponse::redirect("/config?auth_error=rng_failed".to_owned(), Vec::new());
        }
    };

    let authorize_url = format!(
        "{OSU_AUTHORIZE_URL}?client_id={}&redirect_uri={}&response_type=code&scope=identify%20public&state={state}",
        oauth.client_id,
        percent_encode_component(&oauth.redirect_uri),
    );
    let state_cookie = build_state_cookie(&state, "/config", secure);
    let link_cookie = build_discord_link_cookie(&token, secure);
    HttpResponse::redirect(authorize_url, vec![state_cookie, link_cookie])
}

#[derive(Deserialize)]
struct OsuTokenResponse {
    access_token: String,
    #[serde(default)]
    #[allow(dead_code)]
    expires_in: u64,
}

#[derive(Deserialize)]
struct OsuMeResponse {
    id: u32,
    username: String,
    #[serde(default)]
    avatar_url: String,
    #[serde(default)]
    country_code: String,
}

pub(crate) async fn handle_osu_callback(ctx: Arc<Context>, req: &HttpRequest) -> HttpResponse {
    let oauth = load_oauth_config();
    let secure = should_set_secure_cookie(req, &oauth.redirect_uri);
    let clear_state = build_clear_cookie(state_cookie_name(secure), "/", secure);
    let clear_link = build_clear_cookie(discord_link_cookie_name(secure), "/", secure);
    let pending_discord_link = req
        .cookie
        .as_deref()
        .and_then(|c| get_cookie_value(c, discord_link_cookie_name(secure)))
        .filter(|t| is_valid_hex_64(t));

    let (expected_state, return_to) = match req
        .cookie
        .as_deref()
        .and_then(|c| get_cookie_value(c, state_cookie_name(secure)))
        .and_then(|raw| {
            raw.split_once('.')
                .map(|(st, enc_ret)| (st.to_owned(), enc_ret.to_owned()))
        }) {
        Some((st, enc_ret)) => {
            let decoded_ret = percent_decode_form(&enc_ret);
            (Some(st), sanitize_return_to(&decoded_ret))
        }
        None => (None, "/".to_owned()),
    };

    if let Some(err_code) = query_param(&req.query, "error") {
        let code = if err_code == "access_denied" {
            "cancelled"
        } else {
            "oauth_denied"
        };
        return HttpResponse::redirect(
            append_query_param(&return_to, "auth_error", code),
            vec![clear_state, clear_link],
        );
    }

    let Some(expected_state) = expected_state else {
        warn!("osu! OAuth callback missing state cookie");
        return HttpResponse::redirect(
            append_query_param(&return_to, "auth_error", "invalid_state"),
            vec![clear_state, clear_link],
        );
    };

    let received_state = query_param(&req.query, "state").unwrap_or_default();
    if !is_valid_hex_64(&received_state)
        || !constant_time_eq(expected_state.as_bytes(), received_state.as_bytes())
    {
        warn!("osu! OAuth callback state mismatch");
        return HttpResponse::redirect(
            append_query_param(&return_to, "auth_error", "invalid_state"),
            vec![clear_state, clear_link],
        );
    }

    let Some(code) = query_param(&req.query, "code").filter(|c| !c.is_empty() && c.len() <= 2048)
    else {
        return HttpResponse::redirect(
            append_query_param(&return_to, "auth_error", "missing_code"),
            vec![clear_state, clear_link],
        );
    };

    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "Failed building HTTP client for osu! OAuth");
            return HttpResponse::redirect(
                append_query_param(&return_to, "auth_error", "server_error"),
                vec![clear_state, clear_link],
            );
        }
    };

    let client_id_str = oauth.client_id.to_string();
    let token_res = http
        .post(OSU_TOKEN_URL)
        .header("Accept", "application/json")
        .form(&[
            ("client_id", client_id_str.as_str()),
            ("client_secret", oauth.client_secret.as_str()),
            ("code", code.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", oauth.redirect_uri.as_str()),
        ])
        .send()
        .await;

    let token_resp = match token_res {
        Ok(r) => r,
        Err(err) => {
            warn!(?err, "osu! OAuth token exchange request failed");
            return HttpResponse::redirect(
                append_query_param(&return_to, "auth_error", "token_exchange"),
                vec![clear_state, clear_link],
            );
        }
    };

    if !token_resp.status().is_success() {
        let status = token_resp.status();
        let err_text = token_resp.text().await.unwrap_or_default();
        let preview: String = err_text.chars().take(240).collect();
        warn!(%status, body = %preview, redirect_uri = %oauth.redirect_uri, "osu! OAuth token exchange rejected");
        return HttpResponse::redirect(
            append_query_param(&return_to, "auth_error", "token_exchange"),
            vec![clear_state, clear_link],
        );
    }

    let token_body = match token_resp.json::<OsuTokenResponse>().await {
        Ok(t) if !t.access_token.is_empty() => t,
        Ok(_) | Err(_) => {
            warn!("osu! OAuth token response missing access_token");
            return HttpResponse::redirect(
                append_query_param(&return_to, "auth_error", "token_exchange"),
                vec![clear_state, clear_link],
            );
        }
    };

    let me_res = http
        .get(OSU_ME_URL)
        .bearer_auth(&token_body.access_token)
        .header("Accept", "application/json")
        .send()
        .await;

    let me_resp = match me_res {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            warn!(status = %r.status(), "osu! /api/v2/me/osu returned non-success");
            return HttpResponse::redirect(
                append_query_param(&return_to, "auth_error", "profile_fetch"),
                vec![clear_state, clear_link],
            );
        }
        Err(err) => {
            warn!(?err, "osu! /api/v2/me/osu request failed");
            return HttpResponse::redirect(
                append_query_param(&return_to, "auth_error", "profile_fetch"),
                vec![clear_state, clear_link],
            );
        }
    };

    let me: OsuMeResponse = match me_resp.json().await {
        Ok(m) => m,
        Err(err) => {
            warn!(?err, "Failed decoding osu! /api/v2/me/osu JSON");
            return HttpResponse::redirect(
                append_query_param(&return_to, "auth_error", "profile_fetch"),
                vec![clear_state, clear_link],
            );
        }
    };

    let session_token = match generate_random_hex_32() {
        Ok(t) => t,
        Err(err) => {
            warn!(?err, "Failed generating session token");
            return HttpResponse::redirect(
                append_query_param(&return_to, "auth_error", "server_error"),
                vec![clear_state, clear_link],
            );
        }
    };
    let token_hash = sha256(session_token.as_bytes());
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let expires_at = now.saturating_add(WEB_SESSION_TTL_SECS);

    if let Err(err) = ctx
        .psql()
        .upsert_web_user_profile(me.id, &me.username, &me.avatar_url, &me.country_code, now)
        .await
    {
        warn!(?err, osu_user_id = me.id, "Failed upserting web_user_profiles");
        return HttpResponse::redirect(
            append_query_param(&return_to, "auth_error", "db_error"),
            vec![clear_state, clear_link],
        );
    }

    if let Err(err) = ctx
        .psql()
        .create_web_session(&token_hash, me.id, now, expires_at)
        .await
    {
        warn!(?err, osu_user_id = me.id, "Failed creating web_session");
        return HttpResponse::redirect(
            append_query_param(&return_to, "auth_error", "db_error"),
            vec![clear_state, clear_link],
        );
    }

    let session_cookie = build_session_cookie(&session_token, expires_at, secure);

    // If this OAuth flow was started from Discord /link or !link, verify & link the Discord account!
    if let Some(link_tok) = pending_discord_link {
        let link_hash = sha256(link_tok.as_bytes());
        match ctx.psql().consume_discord_link_token(&link_hash, now).await {
            Ok(Some(pending)) => {
                let discord_id = Id::new(pending.discord_id);
                let channel_id = Id::new(pending.channel_id);
                let guild_id = pending.guild_id.map(Id::new);
                if let Err(err) = ctx
                    .psql()
                    .link_osu_user(discord_id, me.id, &me.username)
                    .await
                {
                    warn!(?err, %discord_id, osu_user_id = me.id, "Failed saving verified Discord osu! link");
                    return HttpResponse::redirect(
                        "/config?auth_error=db_error".to_owned(),
                        vec![clear_state, clear_link, session_cookie],
                    );
                }

                info!(
                    %discord_id,
                    osu_user_id = me.id,
                    username = %me.username,
                    "Verified & linked Discord account to osu! profile via OAuth"
                );

                let ctx_notify = Arc::clone(&ctx);
                let osu_id = me.id;
                let osu_name = me.username.clone();
                let osu_avatar = me.avatar_url.clone();
                tokio::spawn(async move {
                    crate::commands::danser::profile_render::notify_discord_link_verified(
                        ctx_notify, discord_id, channel_id, guild_id, osu_id, osu_name, osu_avatar,
                    )
                    .await;
                });

                return HttpResponse::redirect(
                    "/config?auth=linked".to_owned(),
                    vec![clear_state, clear_link, session_cookie],
                );
            }
            Ok(None) => {
                warn!(osu_user_id = me.id, "Discord link token expired or already consumed during OAuth");
                return HttpResponse::redirect(
                    "/config?auth_error=link_expired".to_owned(),
                    vec![clear_state, clear_link, session_cookie],
                );
            }
            Err(err) => {
                warn!(?err, "Failed consuming Discord link token");
                return HttpResponse::redirect(
                    "/config?auth_error=db_error".to_owned(),
                    vec![clear_state, clear_link, session_cookie],
                );
            }
        }
    }

    info!(osu_user_id = me.id, username = %me.username, "Web user logged in via osu! OAuth");
    HttpResponse::redirect(
        append_query_param(&return_to, "auth", "ok"),
        vec![clear_state, clear_link, session_cookie],
    )
}

/// Resolve the authenticated osu! user ID from the incoming request's session cookie, if valid.
pub(crate) async fn resolve_web_session_osu_id(ctx: &Context, req: &HttpRequest) -> Option<u32> {
    let oauth = load_oauth_config();
    let secure = should_set_secure_cookie(req, &oauth.redirect_uri);
    let sess_name = session_cookie_name(secure);
    let token = req
        .cookie
        .as_deref()
        .and_then(|c| get_cookie_value(c, sess_name))
        .filter(|t| is_valid_hex_64(t))?;
    let token_hash = sha256(token.as_bytes());
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    ctx.psql()
        .get_web_session_user(&token_hash, now)
        .await
        .ok()
        .flatten()
        .map(|p| p.osu_user_id)
}

pub(crate) async fn handle_auth_me(ctx: &Context, req: &HttpRequest) -> HttpResponse {
    let oauth = load_oauth_config();
    let secure = should_set_secure_cookie(req, &oauth.redirect_uri);
    let sess_name = session_cookie_name(secure);

    let Some(token) = req
        .cookie
        .as_deref()
        .and_then(|c| get_cookie_value(c, sess_name))
    else {
        let mut resp =
            HttpResponse::json(200, &serde_json::json!({ "ok": true, "logged_in": false }));
        if secure
            && req
                .cookie
                .as_deref()
                .and_then(|c| get_cookie_value(c, DEV_SESSION_COOKIE_NAME))
                .is_some()
        {
            resp.set_cookies
                .push(build_clear_cookie(DEV_SESSION_COOKIE_NAME, "/", true));
        }
        return resp;
    };

    if !is_valid_hex_64(&token) {
        let mut resp =
            HttpResponse::json(200, &serde_json::json!({ "ok": true, "logged_in": false }));
        resp.set_cookies
            .push(build_clear_cookie(sess_name, "/", secure));
        return resp;
    }

    let token_hash = sha256(token.as_bytes());
    let now = time::OffsetDateTime::now_utc().unix_timestamp();

    match ctx.psql().get_web_session_user(&token_hash, now).await {
        Ok(Some(profile)) => {
            let mut resp = HttpResponse::json(
                200,
                &serde_json::json!({
                    "ok": true,
                    "logged_in": true,
                    "user": {
                        "osu_user_id": profile.osu_user_id,
                        "username": profile.username,
                        "avatar_url": profile.avatar_url,
                        "country_code": profile.country_code,
                        "theme": profile.theme,
                        "linked_discord": profile.linked_discord_id.is_some(),
                        "session_expires_at": profile.session_expires_at,
                    }
                }),
            );
            if profile.session_renewed {
                resp.set_cookies.push(build_session_cookie(
                    &token,
                    profile.session_expires_at,
                    secure,
                ));
            }
            resp
        }
        Ok(None) => {
            let mut resp =
                HttpResponse::json(200, &serde_json::json!({ "ok": true, "logged_in": false }));
            resp.set_cookies
                .push(build_clear_cookie(sess_name, "/", secure));
            resp
        }
        Err(err) => {
            warn!(?err, "Database lookup failed in /api/auth/me");
            HttpResponse::json(
                500,
                &serde_json::json!({ "ok": false, "error": "Session lookup temporarily unavailable." }),
            )
        }
    }
}

pub(crate) const ALLOWED_WEB_THEMES: &[&str] = &[
    "nord-dark",
    "nord-light",
    "midnight",
    "osu-pink",
    "catppuccin-mocha",
    "sakura-light",
    "oled-pitch",
];

pub(crate) fn normalize_web_theme(raw: &str) -> Option<&'static str> {
    let trimmed = raw.trim();
    ALLOWED_WEB_THEMES
        .iter()
        .copied()
        .find(|allowed| allowed.eq_ignore_ascii_case(trimmed))
}

#[derive(Deserialize)]
struct SetThemeBody {
    theme: String,
}

pub(crate) async fn handle_set_theme(ctx: &Context, req: &HttpRequest) -> HttpResponse {
    if !passes_same_origin_check(req) {
        return HttpResponse::json(
            403,
            &serde_json::json!({ "ok": false, "error": "Cross-origin theme update rejected." }),
        );
    }

    let Some(osu_user_id) = resolve_web_session_osu_id(ctx, req).await else {
        return HttpResponse::json(
            401,
            &serde_json::json!({ "ok": false, "error": "Log in with osu! to save account themes." }),
        );
    };

    let Ok(parsed) = serde_json::from_slice::<SetThemeBody>(&req.body) else {
        return HttpResponse::json(
            400,
            &serde_json::json!({ "ok": false, "error": "Invalid JSON body." }),
        );
    };

    let Some(theme) = normalize_web_theme(&parsed.theme) else {
        return HttpResponse::json(
            400,
            &serde_json::json!({ "ok": false, "error": "Unknown theme selected." }),
        );
    };

    match ctx.psql().set_web_user_theme(osu_user_id, theme).await {
        Ok(()) => HttpResponse::json(200, &serde_json::json!({ "ok": true, "theme": theme })),
        Err(err) => {
            warn!(?err, osu_user_id, theme, "Failed persisting web user theme");
            HttpResponse::json(
                500,
                &serde_json::json!({ "ok": false, "error": "Failed saving theme preference." }),
            )
        }
    }
}

pub(crate) async fn handle_logout(ctx: &Context, req: &HttpRequest) -> HttpResponse {
    if !passes_same_origin_check(req) {
        return HttpResponse::json(
            403,
            &serde_json::json!({ "ok": false, "error": "Cross-origin logout rejected." }),
        );
    }

    let oauth = load_oauth_config();
    let secure = should_set_secure_cookie(req, &oauth.redirect_uri);
    let sess_name = session_cookie_name(secure);

    if let Some(token) = req
        .cookie
        .as_deref()
        .and_then(|c| get_cookie_value(c, sess_name))
    {
        if is_valid_hex_64(&token) {
            let token_hash = sha256(token.as_bytes());
            if let Err(err) = ctx.psql().delete_web_session(&token_hash).await {
                warn!(?err, "Failed deleting web session during logout");
            }
        }
    }

    let mut resp = HttpResponse::json(200, &serde_json::json!({ "ok": true, "logged_in": false }));
    resp.set_cookies
        .push(build_clear_cookie(sess_name, "/", secure));
    if secure {
        resp.set_cookies
            .push(build_clear_cookie(DEV_SESSION_COOKIE_NAME, "/", true));
    }
    resp
}

pub(crate) fn passes_same_origin_check(req: &HttpRequest) -> bool {
    let mut verified_same_origin = false;

    if let Some(sfs) = req.sec_fetch_site.as_deref() {
        let lower = sfs.trim().to_ascii_lowercase();
        if lower != "same-origin" {
            return false;
        }
        verified_same_origin = true;
    }

    if let Some(origin) = req.origin.as_deref() {
        let Some(host) = req.host.as_deref().map(str::trim).filter(|h| !h.is_empty()) else {
            return false;
        };
        let Some(origin_authority) = origin
            .trim()
            .strip_prefix("https://")
            .or_else(|| origin.trim().strip_prefix("http://"))
        else {
            return false;
        };
        if origin_authority.is_empty()
            || origin_authority
                .bytes()
                .any(|b| matches!(b, b'/' | b'\\' | b'@' | b'?' | b'#') || b.is_ascii_whitespace())
            || !origin_authority.eq_ignore_ascii_case(host)
        {
            return false;
        }
        verified_same_origin = true;
    }

    if req.via_proxy && !verified_same_origin {
        return false;
    }

    true
}

fn sanitize_return_to(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || !trimmed.starts_with('/')
        || trimmed.starts_with("//")
        || trimmed.starts_with("/\\")
        || trimmed.len() > 256
        || trimmed.bytes().any(|b| b < 0x20 || b == 0x7f)
    {
        return "/".to_owned();
    }
    trimmed.to_owned()
}

fn append_query_param(path: &str, key: &str, value: &str) -> String {
    let (base, frag) = match path.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (path, None),
    };
    let sep = if base.contains('?') { '&' } else { '?' };
    let enc_val = percent_encode_component(value);
    match frag {
        Some(f) => format!("{base}{sep}{key}={enc_val}#{f}"),
        None => format!("{base}{sep}{key}={enc_val}"),
    }
}

fn query_param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        if k == name {
            return Some(percent_decode_form(v));
        }
    }
    None
}

fn get_cookie_value(cookie_header: &str, name: &str) -> Option<String> {
    for part in cookie_header.split(';') {
        let trimmed = part.trim();
        if let Some((k, v)) = trimmed.split_once('=') {
            if k.trim() == name {
                return Some(v.trim().to_owned());
            }
        }
    }
    None
}

fn percent_encode_component(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len() + 16);
    for &b in input.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

fn percent_decode_form(input: &str) -> String {
    let replaced = input.replace('+', " ");
    crate::util::str::percent_decode_utf8(&replaced)
}

fn is_valid_hex_64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub(crate) fn generate_random_hex_32() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(&mut bytes)?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    Ok(out)
}

fn format_http_date(unix_ts: i64) -> Option<String> {
    let dt = time::OffsetDateTime::from_unix_timestamp(unix_ts).ok()?;
    let wd = match dt.weekday() {
        time::Weekday::Monday => "Mon",
        time::Weekday::Tuesday => "Tue",
        time::Weekday::Wednesday => "Wed",
        time::Weekday::Thursday => "Thu",
        time::Weekday::Friday => "Fri",
        time::Weekday::Saturday => "Sat",
        time::Weekday::Sunday => "Sun",
    };
    let mon = match dt.month() {
        time::Month::January => "Jan",
        time::Month::February => "Feb",
        time::Month::March => "Mar",
        time::Month::April => "Apr",
        time::Month::May => "May",
        time::Month::June => "Jun",
        time::Month::July => "Jul",
        time::Month::August => "Aug",
        time::Month::September => "Sep",
        time::Month::October => "Oct",
        time::Month::November => "Nov",
        time::Month::December => "Dec",
    };
    Some(format!(
        "{wd}, {:02} {mon} {:04} {:02}:{:02}:{:02} GMT",
        dt.day(),
        dt.year(),
        dt.hour(),
        dt.minute(),
        dt.second()
    ))
}

/// FIPS 180-4 SHA-256 digest used to hash opaque session tokens before DB storage.
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity((data.len() + 9).div_ceil(64) * 64);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn test_sha256_nist_vectors() {
        assert_eq!(
            to_hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            to_hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            to_hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn test_sanitize_return_to_and_cookies() {
        assert_eq!(sanitize_return_to("/config"), "/config");
        assert_eq!(sanitize_return_to("/watch/ad4316be"), "/watch/ad4316be");
        assert_eq!(sanitize_return_to("//evil.com"), "/");
        assert_eq!(sanitize_return_to("https://evil.com"), "/");
        assert_eq!(sanitize_return_to("/foo\r\nSet-Cookie: bad"), "/");

        assert_eq!(
            get_cookie_value("a=1; __Host-osu_session=deadbeef; b=2", "__Host-osu_session"),
            Some("deadbeef".to_owned())
        );
        let prod_sess = build_session_cookie("abcd", 1_700_000_000, true);
        assert!(prod_sess.starts_with("__Host-osu_session=abcd; Path=/; HttpOnly; SameSite=Lax;"));
        assert!(prod_sess.ends_with("; Secure"));
        let prod_state = build_state_cookie("st123", "/config", true);
        assert!(prod_state.starts_with("__Host-osu_oauth_state=st123.%2Fconfig; Path=/; HttpOnly;"));
        assert!(prod_state.ends_with("; Secure"));

        let make_req = |origin: Option<&str>, host: Option<&str>, sfs: Option<&str>, via_proxy: bool| HttpRequest {
            method: "POST".to_owned(),
            path: "/api/auth/logout".to_owned(),
            query: String::new(),
            client_ip: "198.51.100.10".to_owned(),
            via_proxy,
            cookie: None,
            origin: origin.map(str::to_owned),
            host: host.map(str::to_owned),
            forwarded_proto: Some("https".to_owned()),
            sec_fetch_site: sfs.map(str::to_owned),
            body: Vec::new(),
        };
        assert!(passes_same_origin_check(&make_req(
            Some("https://replays.insertdomainname.be"),
            Some("replays.insertdomainname.be"),
            Some("same-origin"),
            true,
        )));
        // Sibling subdomain (`same-site`) MUST be rejected
        assert!(!passes_same_origin_check(&make_req(
            Some("https://other.insertdomainname.be"),
            Some("replays.insertdomainname.be"),
            Some("same-site"),
            true,
        )));
        // Sandboxed iframe (`Origin: null`) MUST be rejected
        assert!(!passes_same_origin_check(&make_req(
            Some("null"),
            Some("replays.insertdomainname.be"),
            None,
            true,
        )));
        // Proxied POST missing both Origin and Sec-Fetch-Site MUST be rejected
        assert!(!passes_same_origin_check(&make_req(
            None,
            Some("replays.insertdomainname.be"),
            None,
            true,
        )));

        assert_eq!(
            query_param("code=abc%20123&state=xyz", "code"),
            Some("abc 123".to_owned())
        );
        assert_eq!(format_http_date(0).as_deref(), Some("Thu, 01 Jan 1970 00:00:00 GMT"));
        assert!(is_valid_hex_64(&generate_random_hex_32().unwrap()));
        assert_eq!(
            public_site_origin("https://replays.insertdomainname.be/api/auth/osu/callback"),
            "https://replays.insertdomainname.be"
        );
        assert_eq!(
            public_site_origin("https://example.com:8443/custom/callback"),
            "https://example.com:8443"
        );
        assert_eq!(normalize_web_theme("nord-dark"), Some("nord-dark"));
        assert_eq!(normalize_web_theme("  OSU-PINK "), Some("osu-pink"));
        assert_eq!(normalize_web_theme("sakura-light"), Some("sakura-light"));
        assert_eq!(normalize_web_theme("oled-pitch"), Some("oled-pitch"));
        assert_eq!(normalize_web_theme("unknown"), None);
        assert_eq!(normalize_web_theme(""), None);
    }
}
