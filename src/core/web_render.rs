use std::{
    collections::{hash_map::RandomState, HashMap, VecDeque},
    hash::{BuildHasher, Hasher},
    net::IpAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::{Duration, Instant},
};

use eyre::Result;
use osu_db::{Mode, Replay};
use rosu_v2::prelude::{GameModsLegacy, Grade};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{RwLock, Semaphore},
};

use crate::{
    commands::danser::settings::{
        current_skin_basename, default_settings_path, resolve_skin_name, rewrite_current_skin_path,
    },
    core::{
        replay_queue::{
            is_lazer_replay, RenderOptions, ReplayData, ReplaySlim, ReplayStatus, TimePoints,
            WebJobHandle, WebJobState,
        },
        settings::DanserSettings,
        BotConfig, Context,
    },
    util::fs::write_json_atomic,
};

const DEFAULT_BIND: &str = "127.0.0.1:5556";
const MAX_CONCURRENT_HTTP_CONNS: usize = 64;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_SMALL_BODY_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_OSR_BYTES: usize = 5 * 1024 * 1024;
const MAX_RAW_SETTINGS_BYTES: usize = 256 * 1024;
const MAX_WEB_JOBS_PER_IP: usize = 2;
const MAX_TOTAL_QUEUE_LEN: usize = 15;
const RATE_WINDOW: Duration = Duration::from_secs(60);
const MAX_RATE_TRACKED_KEYS: usize = 4096;

static TOKEN_SEQ: AtomicU64 = AtomicU64::new(1);

struct IpRateLimiter {
    buckets: StdMutex<HashMap<(String, &'static str), VecDeque<Instant>>>,
}

impl IpRateLimiter {
    fn new() -> Self {
        Self {
            buckets: StdMutex::new(HashMap::new()),
        }
    }

    fn check_and_record(&self, client_ip: &str, scope: &'static str, max_hits: usize) -> bool {
        let now = Instant::now();
        let mut map = self.buckets.lock().unwrap_or_else(|p| p.into_inner());
        let key = (client_ip.to_owned(), scope);

        if map.len() >= MAX_RATE_TRACKED_KEYS && !map.contains_key(&key) {
            map.retain(|_, q| {
                while q.front().is_some_and(|t| now.saturating_duration_since(*t) >= RATE_WINDOW) {
                    q.pop_front();
                }
                !q.is_empty()
            });
            if map.len() >= MAX_RATE_TRACKED_KEYS {
                return false;
            }
        }

        let q = map.entry(key).or_default();
        while q.front().is_some_and(|t| now.saturating_duration_since(*t) >= RATE_WINDOW) {
            q.pop_front();
        }
        if q.len() >= max_hits {
            return false;
        }
        q.push_back(now);
        true
    }
}

fn is_trusted_proxy_peer(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_loopback() || v4.is_private())
                || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

pub fn spawn(ctx: Arc<Context>) {
    let bind = std::env::var("WEB_RENDER_BIND")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_BIND.to_owned());

    tokio::spawn(async move {
        let mut bound_listener = None;
        for attempt in 1..=10u8 {
            match TcpListener::bind(&bind).await {
                Ok(l) => {
                    bound_listener = Some(l);
                    break;
                }
                Err(err) if attempt < 10 => {
                    warn!(%bind, attempt, ?err, "Web render HTTP bind busy; retrying in 500ms");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(err) => {
                    error!(%bind, ?err, "Failed to bind web render HTTP API after 10 attempts; exiting");
                    std::process::exit(1);
                }
            }
        }
        let Some(listener) = bound_listener else {
            std::process::exit(1);
        };
        info!(%bind, "Web render HTTP API listening");

        let conn_sem = Arc::new(Semaphore::new(MAX_CONCURRENT_HTTP_CONNS));
        let limiter = Arc::new(IpRateLimiter::new());

        loop {
            match listener.accept().await {
                Ok((mut stream, peer)) => {
                    let permit = match conn_sem.clone().try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            let busy = HttpResponse::json(
                                429,
                                &serde_json::json!({ "ok": false, "error": "Server is busy. Please try again in a moment." }),
                            );
                            tokio::spawn(async move {
                                let _ = write_http_response(&mut stream, &busy).await;
                            });
                            continue;
                        }
                    };
                    let ctx_clone = Arc::clone(&ctx);
                    let limiter_clone = Arc::clone(&limiter);
                    let peer_ip = peer.ip();
                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Err(err) =
                            handle_connection(ctx_clone, limiter_clone, stream, peer_ip).await
                        {
                            tracing::debug!(?err, "Web render HTTP connection closed with error");
                        }
                    });
                }
                Err(err) => {
                    warn!(?err, "Failed to accept web render HTTP connection");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
}

pub(crate) struct HttpRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub client_ip: String,
    pub via_proxy: bool,
    pub cookie: Option<String>,
    pub origin: Option<String>,
    pub host: Option<String>,
    pub forwarded_proto: Option<String>,
    pub sec_fetch_site: Option<String>,
    pub body: Vec<u8>,
}

pub(crate) struct HttpResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub location: Option<String>,
    pub set_cookies: Vec<String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            location: None,
            set_cookies: Vec::new(),
            body,
        }
    }

    pub fn redirect(location: String, set_cookies: Vec<String>) -> Self {
        Self {
            status: 302,
            content_type: "text/plain; charset=utf-8",
            location: Some(location),
            set_cookies,
            body: Vec::new(),
        }
    }
}

async fn read_http_request(
    stream: &mut TcpStream,
    peer_ip: IpAddr,
) -> std::result::Result<HttpRequest, (u16, &'static str)> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];

    let header_end = loop {
        let n = tokio::time::timeout(Duration::from_secs(15), stream.read(&mut tmp))
            .await
            .map_err(|_| (408, "Request timeout"))?
            .map_err(|_| (400, "Failed reading request"))?;
        if n == 0 {
            return Err((400, "Empty request"));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err((431, "Request headers too large"));
        }
    };

    let header_str = std::str::from_utf8(&buf[..header_end]).map_err(|_| (400, "Invalid UTF-8 headers"))?;
    let mut lines = header_str.split("\r\n");
    let request_line = lines.next().ok_or((400, "Missing request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or((400, "Missing method"))?.to_owned();
    let raw_target = parts.next().ok_or((400, "Missing target"))?;
    let (path, query) = match raw_target.split_once('?') {
        Some((p, q)) => (p.to_owned(), q.to_owned()),
        None => (raw_target.to_owned(), String::new()),
    };

    let peer_trusted = is_trusted_proxy_peer(peer_ip);
    let mut content_length: usize = 0;
    let mut client_ip = peer_ip.to_string();
    let mut via_proxy = false;
    let mut cookies: Vec<String> = Vec::new();
    let mut origin: Option<String> = None;
    let mut host: Option<String> = None;
    let mut forwarded_proto: Option<String> = None;
    let mut sec_fetch_site: Option<String> = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            let val = v.trim();
            if key.eq_ignore_ascii_case("content-length") {
                content_length = val
                    .parse::<usize>()
                    .map_err(|_| (400, "Invalid Content-Length"))?;
            } else if key.eq_ignore_ascii_case("x-real-ip") && peer_trusted && !val.is_empty() {
                client_ip = val.chars().take(64).collect();
                via_proxy = true;
            } else if key.eq_ignore_ascii_case("cookie") && !val.is_empty() {
                cookies.push(val.to_owned());
            } else if key.eq_ignore_ascii_case("origin") && !val.is_empty() {
                origin = Some(val.chars().take(256).collect());
            } else if key.eq_ignore_ascii_case("host") && !val.is_empty() {
                host = Some(val.chars().take(256).collect());
            } else if key.eq_ignore_ascii_case("x-forwarded-proto") && peer_trusted && !val.is_empty() {
                forwarded_proto = Some(val.chars().take(16).collect());
                via_proxy = true;
            } else if key.eq_ignore_ascii_case("sec-fetch-site") && !val.is_empty() {
                sec_fetch_site = Some(val.chars().take(32).collect());
            }
        }
    }

    let trimmed_path = path.trim_end_matches('/');
    let max_body = if method == "POST"
        && matches!(
            trimmed_path,
            "/api/web-render/inspect" | "/api/web-render/submit"
        ) {
        MAX_BODY_BYTES
    } else {
        MAX_SMALL_BODY_BYTES
    };

    if content_length > max_body {
        return Err((413, "Payload too large"));
    }

    let body_start = header_end + 4;
    let mut body = Vec::with_capacity(content_length.min(64 * 1024));
    if buf.len() > body_start {
        let take = (buf.len() - body_start).min(content_length);
        body.extend_from_slice(&buf[body_start..body_start + take]);
    }
    while body.len() < content_length {
        let remaining = content_length - body.len();
        let to_read = remaining.min(tmp.len());
        let n = tokio::time::timeout(Duration::from_secs(30), stream.read(&mut tmp[..to_read]))
            .await
            .map_err(|_| (408, "Body read timeout"))?
            .map_err(|_| (400, "Failed reading request body"))?;
        if n == 0 {
            return Err((400, "Truncated request body"));
        }
        body.extend_from_slice(&tmp[..n]);
    }

    let cookie = if cookies.is_empty() {
        None
    } else {
        Some(cookies.join("; "))
    };

    Ok(HttpRequest {
        method,
        path,
        query,
        client_ip,
        via_proxy,
        cookie,
        origin,
        host,
        forwarded_proto,
        sec_fetch_site,
        body,
    })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

async fn write_http_response(stream: &mut TcpStream, resp: &HttpResponse) -> Result<()> {
    let reason = match resp.status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    };
    let mut header = format!(
        "HTTP/1.1 {} {reason}\r\n\
         Content-Type: {}\r\n\
         Cache-Control: private, no-store, no-cache, must-revalidate\r\n\
         Pragma: no-cache\r\n\
         CDN-Cache-Control: no-store\r\n\
         Cloudflare-CDN-Cache-Control: no-store\r\n\
         Content-Length: {}\r\n",
        resp.status,
        resp.content_type,
        resp.body.len()
    );
    if let Some(ref loc) = resp.location {
        let safe_loc = loc.replace(['\r', '\n'], "");
        header.push_str("Location: ");
        header.push_str(&safe_loc);
        header.push_str("\r\n");
    }
    for cookie in &resp.set_cookies {
        let safe_cookie = cookie.replace(['\r', '\n'], "");
        header.push_str("Set-Cookie: ");
        header.push_str(&safe_cookie);
        header.push_str("\r\n");
    }
    header.push_str("Connection: close\r\n\r\n");
    stream.write_all(header.as_bytes()).await?;
    if !resp.body.is_empty() {
        stream.write_all(&resp.body).await?;
    }
    stream.shutdown().await?;
    Ok(())
}

async fn handle_connection(
    ctx: Arc<Context>,
    limiter: Arc<IpRateLimiter>,
    mut stream: TcpStream,
    peer_ip: IpAddr,
) -> Result<()> {
    let req = match read_http_request(&mut stream, peer_ip).await {
        Ok(r) => r,
        Err((code, msg)) => {
            let resp = HttpResponse::json(code, &serde_json::json!({ "ok": false, "error": msg }));
            let _ = write_http_response(&mut stream, &resp).await;
            return Ok(());
        }
    };

    let resp = route_request(ctx, &limiter, req).await;
    write_http_response(&mut stream, &resp).await
}

async fn route_request(
    ctx: Arc<Context>,
    limiter: &IpRateLimiter,
    req: HttpRequest,
) -> HttpResponse {
    let path = req.path.trim_end_matches('/');
    match (req.method.as_str(), path) {
        ("GET", "/api/auth/osu/login") => {
            if !limiter.check_and_record(&req.client_ip, "auth_oauth", 15) {
                return HttpResponse::redirect("/config?auth_error=rate_limited".to_owned(), Vec::new());
            }
            crate::core::web_auth::handle_osu_login(&req).await
        }
        ("GET", "/api/auth/osu/discord-link") => {
            if !limiter.check_and_record(&req.client_ip, "auth_oauth", 15) {
                return HttpResponse::redirect("/config?auth_error=rate_limited".to_owned(), Vec::new());
            }
            crate::core::web_auth::handle_discord_link_start(&ctx, &req).await
        }
        ("GET", "/api/auth/osu/callback") => {
            if !limiter.check_and_record(&req.client_ip, "auth_oauth", 15) {
                return HttpResponse::redirect("/config?auth_error=rate_limited".to_owned(), Vec::new());
            }
            crate::core::web_auth::handle_osu_callback(Arc::clone(&ctx), &req).await
        }
        ("GET", "/api/auth/me") => crate::core::web_auth::handle_auth_me(&ctx, &req).await,
        ("POST", "/api/auth/theme") => {
            if !limiter.check_and_record(&req.client_ip, "auth_theme", 30) {
                return HttpResponse::json(
                    429,
                    &serde_json::json!({ "ok": false, "error": "Too many theme updates. Please wait a moment." }),
                );
            }
            crate::core::web_auth::handle_set_theme(&ctx, &req).await
        }
        ("POST", "/api/auth/logout") => crate::core::web_auth::handle_logout(&ctx, &req).await,
        ("GET", "/api/web-render/health") => {
            let idle_secs = crate::core::events::seconds_since_gateway_event();
            let healthy = idle_secs < 360;
            HttpResponse::json(
                if healthy { 200 } else { 503 },
                &serde_json::json!({
                    "ok": healthy,
                    "gateway_idle_secs": idle_secs,
                }),
            )
        }
        ("GET", "/api/web-render/config") => {
            let (s, v) = handle_get_config(&ctx, &req).await;
            HttpResponse::json(s, &v)
        }
        ("GET", "/api/web-render/queue") => {
            let (s, v) = handle_get_queue(&ctx).await;
            HttpResponse::json(s, &v)
        }
        ("POST", "/api/web-render/inspect") => {
            if !crate::core::web_auth::passes_same_origin_check(&req) {
                return HttpResponse::json(
                    403,
                    &serde_json::json!({ "ok": false, "error": "Cross-origin request rejected." }),
                );
            }
            if !limiter.check_and_record(&req.client_ip, "render_inspect", 15) {
                return HttpResponse::json(
                    429,
                    &serde_json::json!({ "ok": false, "error": "Too many replay inspections. Please wait a moment and try again." }),
                );
            }
            let (s, v) = handle_inspect(&ctx, &req.body).await;
            HttpResponse::json(s, &v)
        }
        ("POST", "/api/web-render/submit") => {
            if !crate::core::web_auth::passes_same_origin_check(&req) {
                return HttpResponse::json(
                    403,
                    &serde_json::json!({ "ok": false, "error": "Cross-origin request rejected." }),
                );
            }
            if !limiter.check_and_record(&req.client_ip, "render_submit", 8) {
                return HttpResponse::json(
                    429,
                    &serde_json::json!({ "ok": false, "error": "Too many render submissions from your IP. Please wait a minute before submitting again." }),
                );
            }
            let (s, v) = handle_submit(&ctx, &req).await;
            HttpResponse::json(s, &v)
        }
        ("GET", p) if p.starts_with("/api/web-render/jobs/") => {
            let token = &p["/api/web-render/jobs/".len()..];
            let (s, v) = if token.contains('/') {
                (404, serde_json::json!({ "ok": false, "error": "Not found" }))
            } else {
                handle_get_job(&ctx, token).await
            };
            HttpResponse::json(s, &v)
        }
        ("POST", p) if p.starts_with("/api/web-render/jobs/") && p.ends_with("/cancel") => {
            if !crate::core::web_auth::passes_same_origin_check(&req) {
                return HttpResponse::json(
                    403,
                    &serde_json::json!({ "ok": false, "error": "Cross-origin request rejected." }),
                );
            }
            if !limiter.check_and_record(&req.client_ip, "render_cancel", 20) {
                return HttpResponse::json(
                    429,
                    &serde_json::json!({ "ok": false, "error": "Too many cancel requests. Please wait a moment." }),
                );
            }
            let inner = &p["/api/web-render/jobs/".len()..p.len() - "/cancel".len()];
            let (s, v) = if inner.contains('/') {
                (404, serde_json::json!({ "ok": false, "error": "Not found" }))
            } else {
                handle_cancel_job(&ctx, inner).await
            };
            HttpResponse::json(s, &v)
        }
        _ => HttpResponse::json(404, &serde_json::json!({ "ok": false, "error": "Not found" })),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WebDanserSettings {
    pub skin: String,
    pub dim: u8,
    pub cursor_scale: f64,
    pub cursor_ripples: bool,
    pub storyboard: bool,
    pub video: bool,
    pub motion_blur: bool,
    pub pp: bool,
    pub pp_decimals: u32,
    pub hit_error_meter: bool,
    pub hit_error_decimals: u32,
    pub aim_error_meter: bool,
    pub aim_error_decimals: u32,
    pub hit_counter: bool,
    pub sliderbreaks: bool,
    pub strain_graph: bool,
    pub strain_graph_outline: bool,
    pub leaderboard: bool,
    pub key_overlay: bool,
    pub music_volume: u8,
    pub hitsound_volume: u8,
    pub beatmap_hitsounds: bool,
    pub skip_intro: bool,
    pub results_screen: bool,
    pub resolution: String,
}

impl Default for WebDanserSettings {
    fn default() -> Self {
        Self {
            skin: String::new(),
            dim: 75,
            cursor_scale: 1.0,
            cursor_ripples: false,
            storyboard: true,
            video: true,
            motion_blur: false,
            pp: true,
            pp_decimals: 0,
            hit_error_meter: true,
            hit_error_decimals: 0,
            aim_error_meter: false,
            aim_error_decimals: 0,
            hit_counter: true,
            sliderbreaks: true,
            strain_graph: true,
            strain_graph_outline: true,
            leaderboard: true,
            key_overlay: true,
            music_volume: 70,
            hitsound_volume: 50,
            beatmap_hitsounds: true,
            skip_intro: false,
            results_screen: true,
            resolution: "720p".to_owned(),
        }
    }
}

impl WebDanserSettings {
    fn from_danser(s: &DanserSettings) -> Self {
        Self {
            skin: current_skin_basename(s),
            dim: (s.playfield.background.dim.normal * 100.0).round().clamp(0.0, 100.0) as u8,
            cursor_scale: (s.skin.cursor.scale * 100.0).round() / 100.0,
            cursor_ripples: s.cursor.cursor_ripples,
            storyboard: s.playfield.background.load_storyboards,
            video: s.playfield.background.load_videos,
            motion_blur: s.recording.motion_blur.enabled,
            pp: s.gameplay.pp_counter.show,
            pp_decimals: s.gameplay.pp_counter.decimals.min(3),
            hit_error_meter: s.gameplay.hit_error_meter.show,
            hit_error_decimals: s.gameplay.hit_error_meter.unstable_rate_decimals.min(3),
            aim_error_meter: s.gameplay.aim_error_meter.show,
            aim_error_decimals: s.gameplay.aim_error_meter.unstable_rate_decimals.min(3),
            hit_counter: s.gameplay.hit_counter.show,
            sliderbreaks: s.gameplay.hit_counter.show_sliderbreaks,
            strain_graph: s.gameplay.strain_graph.show,
            strain_graph_outline: s.gameplay.strain_graph.outline.show,
            leaderboard: s.gameplay.score_board.show,
            key_overlay: s.gameplay.key_overlay.show,
            music_volume: (s.audio.music_volume * 100.0).round().clamp(0.0, 100.0) as u8,
            hitsound_volume: (s.audio.sample_volume * 100.0).round().clamp(0.0, 100.0) as u8,
            beatmap_hitsounds: !s.audio.ignore_beatmap_samples,
            skip_intro: s.gameplay.always_skip_intro,
            results_screen: s.gameplay.show_results_screen,
            resolution: s.current_render_quality().code().to_owned(),
        }
    }

    fn apply_to_danser(
        &self,
        ctx: &Context,
        target: &mut DanserSettings,
        can_use_900p: bool,
    ) -> std::result::Result<(), String> {
        if !self.skin.trim().is_empty() {
            match resolve_skin_name(ctx, &self.skin) {
                Ok(Some(resolved)) => rewrite_current_skin_path(target, &resolved),
                Ok(None) => return Err(format!("Unknown skin `{}`.", self.skin)),
                Err(err) => {
                    warn!(?err, "Failed resolving skin list");
                    return Err("Failed to load available skins.".to_owned());
                }
            }
        }

        target.playfield.background.dim.normal = f64::from(self.dim.min(100)) / 100.0;
        let cs = if self.cursor_scale.is_finite() {
            self.cursor_scale.clamp(0.1, 2.0)
        } else {
            1.0
        };
        target.skin.cursor.scale = cs;
        target.cursor.cursor_ripples = self.cursor_ripples;
        target.playfield.background.load_storyboards = self.storyboard;
        target.playfield.background.load_videos = self.video;
        target.recording.motion_blur.enabled = self.motion_blur;

        target.gameplay.pp_counter.show = self.pp;
        target.gameplay.pp_counter.decimals = self.pp_decimals.min(3);
        target.gameplay.hit_error_meter.show = self.hit_error_meter;
        target.gameplay.hit_error_meter.unstable_rate_decimals = self.hit_error_decimals.min(3);
        target.gameplay.aim_error_meter.show = self.aim_error_meter;
        target.gameplay.aim_error_meter.unstable_rate_decimals = self.aim_error_decimals.min(3);
        target.gameplay.hit_counter.show = self.hit_counter;
        target.gameplay.hit_counter.show_sliderbreaks = self.sliderbreaks;
        target.gameplay.strain_graph.show = self.strain_graph;
        target.gameplay.strain_graph.outline.show = self.strain_graph_outline;
        target.gameplay.score_board.show = self.leaderboard;
        target.gameplay.key_overlay.show = self.key_overlay;

        target.audio.music_volume = f64::from(self.music_volume.min(100)) / 100.0;
        target.audio.sample_volume = f64::from(self.hitsound_volume.min(100)) / 100.0;
        target.audio.ignore_beatmap_samples = !self.beatmap_hitsounds;
        target.gameplay.always_skip_intro = self.skip_intro;
        target.gameplay.show_results_screen = self.results_screen;

        let requested = crate::core::settings::RenderQuality::from_code(&self.resolution)
            .unwrap_or(if can_use_900p {
                crate::core::settings::RenderQuality::HdPlus900p60
            } else {
                crate::core::settings::RenderQuality::Hd720p60
            });
        target.apply_render_quality(requested.cap_for_entitlement(can_use_900p));
        Ok(())
    }
}

async fn load_default_danser_settings() -> Result<DanserSettings> {
    let bytes = tokio::fs::read(default_settings_path()).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn handle_get_config(ctx: &Context, req: &HttpRequest) -> (u16, serde_json::Value) {
    let can_use_900p = crate::core::web_auth::resolve_web_session_osu_id(ctx, req)
        .await
        .is_some();
    let default_quality = if can_use_900p {
        crate::core::settings::RenderQuality::HdPlus900p60
    } else {
        crate::core::settings::RenderQuality::Hd720p60
    };

    let skins: Vec<serde_json::Value> = {
        let mut guard = ctx.skin_list();
        match guard.get() {
            Ok(list) => list
                .iter()
                .filter_map(|os| {
                    let raw = os.to_string_lossy().into_owned();
                    if raw.is_empty() || raw.len() > 120 {
                        return None;
                    }
                    let label = url_decode_display(&raw).replace('_', " ");
                    Some(serde_json::json!({ "value": raw, "label": label }))
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    };

    let mut defaults = match load_default_danser_settings().await {
        Ok(d) => WebDanserSettings::from_danser(&d),
        Err(_) => WebDanserSettings::default(),
    };
    defaults.resolution = default_quality.code().to_owned();

    let queue_length = ctx.replay_queue.queue.lock().await.len();

    let presets = serde_json::json!([
        {
            "id": "standard",
            "label": "Default Profile",
            "patch": {
                "motion_blur": false,
                "storyboard": true,
                "video": true,
                "results_screen": true,
                "leaderboard": true,
                "hit_error_meter": true,
                "strain_graph": true,
                "key_overlay": true
            }
        },
        {
            "id": "clean",
            "label": "Clean Showcase",
            "patch": {
                "results_screen": false,
                "leaderboard": false,
                "hit_error_meter": false,
                "aim_error_meter": false,
                "strain_graph": false,
                "key_overlay": false
            }
        },
        {
            "id": "fast720p",
            "label": "Fast Render (720p60)",
            "patch": {
                "resolution": "720p",
                "motion_blur": false,
                "storyboard": false,
                "video": false
            }
        },
        {
            "id": "blur",
            "label": "Cinematic Blur",
            "patch": {
                "motion_blur": true,
                "results_screen": true
            }
        }
    ]);

    let (dw, dh, dfps) = default_quality.dimensions();
    let profile = serde_json::json!({
        "id": default_quality.code(),
        "name": crate::core::settings::DEFAULT_PROFILE_NAME,
        "label": default_quality.display_label(),
        "resolution": default_quality.code(),
        "width": dw,
        "height": dh,
        "fps": dfps,
        "locked": !can_use_900p,
    });
    let profiles = serde_json::json!([
        {
            "value": "1080p",
            "label": "Full HD (1080p 60fps — 1920×1080 · Slower)",
            "width": crate::core::settings::RECORDING_1080P_WIDTH,
            "height": crate::core::settings::RECORDING_1080P_HEIGHT,
            "fps": crate::core::settings::LOCKED_RECORDING_FPS,
            "unlocked": can_use_900p,
            "requires_osu_login": true,
            "slow_warning": true,
        },
        {
            "value": "900p",
            "label": "High Quality (900p 60fps — 1600×900)",
            "width": crate::core::settings::RECORDING_900P_WIDTH,
            "height": crate::core::settings::RECORDING_900P_HEIGHT,
            "fps": crate::core::settings::LOCKED_RECORDING_FPS,
            "unlocked": can_use_900p,
            "requires_osu_login": true,
            "slow_warning": false,
        },
        {
            "value": "720p",
            "label": "Standard (720p 60fps — 1280×720)",
            "width": crate::core::settings::RECORDING_720P_WIDTH,
            "height": crate::core::settings::RECORDING_720P_HEIGHT,
            "fps": crate::core::settings::LOCKED_RECORDING_FPS,
            "unlocked": true,
            "requires_osu_login": false,
            "slow_warning": false,
        }
    ]);

    (
        200,
        serde_json::json!({
            "ok": true,
            "can_use_900p": can_use_900p,
            "can_use_1080p": can_use_900p,
            "default_quality": default_quality.code(),
            "skins": skins,
            "defaults": defaults,
            "profile": profile,
            "profiles": profiles,
            "presets": presets,
            "queue_length": queue_length,
            "max_osr_bytes": MAX_OSR_BYTES,
        }),
    )
}

async fn handle_get_queue(ctx: &Context) -> (u16, serde_json::Value) {
    let queue_length = ctx.replay_queue.queue.lock().await.len();
    let st = *ctx.replay_queue.status.lock().await;
    let (status, progress) = match st {
        ReplayStatus::Waiting => ("idle", 0u8),
        ReplayStatus::Downloading => ("downloading", 0),
        ReplayStatus::MapFound => ("map_found", 0),
        ReplayStatus::Rendering(p) => ("rendering", p),
        ReplayStatus::Uploading => ("uploading", 100),
    };
    (
        200,
        serde_json::json!({
            "ok": true,
            "queue_length": queue_length,
            "status": status,
            "progress": progress,
        }),
    )
}

#[derive(Deserialize)]
struct InspectPayload {
    #[serde(default)]
    filename: String,
    osr_base64: String,
}

async fn handle_inspect(ctx: &Context, body: &[u8]) -> (u16, serde_json::Value) {
    let req: InspectPayload = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (400, serde_json::json!({ "ok": false, "error": "Invalid JSON body" })),
    };

    let osr_bytes = match decode_base64_limited(&req.osr_base64, MAX_OSR_BYTES) {
        Ok(b) => b,
        Err(msg) => return (400, serde_json::json!({ "ok": false, "error": msg })),
    };

    let raw_replay = match Replay::from_bytes(&osr_bytes) {
        Ok(r) => r,
        Err(_) => {
            return (
                400,
                serde_json::json!({ "ok": false, "error": "Could not parse .osr replay file. Make sure it is a valid osu! replay." }),
            )
        }
    };

    if raw_replay.mode != Mode::Standard {
        return (
            400,
            serde_json::json!({ "ok": false, "error": "Only osu!standard replays are supported." }),
        );
    }

    let is_lazer = is_lazer_replay(raw_replay.version);
    let slim = ReplaySlim::from(raw_replay);
    let legacy_mods = GameModsLegacy::from_bits(slim.mods);
    let mods_str = if legacy_mods.is_empty() {
        "NM".to_owned()
    } else {
        legacy_mods.to_string()
    };
    let grade_str = match slim.grade {
        Grade::XH => "SSH",
        Grade::X => "SS",
        Grade::SH => "SH",
        Grade::S => "S",
        Grade::A => "A",
        Grade::B => "B",
        Grade::C => "C",
        Grade::D => "D",
        Grade::F => "F",
    };

    let mut map_title: Option<String> = None;
    let mut stars: Option<f64> = None;
    let mut beatmap_id: Option<u32> = None;
    let mut mapset_id: Option<u32> = None;
    let mut map_max_combo: Option<u32> = None;
    let mut beatmap_url: Option<String> = None;

    if let Some(ref hash) = slim.beatmap_hash {
        if let Ok(map) = crate::osu_call!(ctx, |osu| osu.beatmap().checksum(hash)) {
            beatmap_id = Some(map.map_id);
            mapset_id = Some(map.mapset_id);
            stars = Some((f64::from(map.stars) * 100.0).round() / 100.0);
            map_max_combo = map.max_combo;
            beatmap_url = Some(format!("https://osu.ppy.sh/b/{}", map.map_id));
            if let Some(ref ms) = map.mapset {
                map_title = Some(format!("{} - {} [{}]", ms.artist, ms.title, map.version));
            } else {
                map_title = Some(map.version);
            }
        }
    }

    (
        200,
        serde_json::json!({
            "ok": true,
            "replay": {
                "filename": req.filename,
                "player": slim.player_name.as_deref().unwrap_or("Unknown"),
                "map_title": map_title,
                "stars": stars,
                "mods": mods_str,
                "accuracy": slim.accuracy(),
                "grade": grade_str,
                "score": slim.score,
                "max_combo": slim.max_combo,
                "map_max_combo": map_max_combo,
                "count_300": slim.count_300,
                "count_100": slim.count_100,
                "count_50": slim.count_50,
                "count_miss": slim.count_miss,
                "is_lazer": is_lazer,
                "beatmap_id": beatmap_id,
                "mapset_id": mapset_id,
                "beatmap_url": beatmap_url,
                "has_beatmap_hash": slim.beatmap_hash.as_deref().is_some_and(|h| !h.is_empty()),
            }
        }),
    )
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WebRenderOptionsInput {
    start: Option<String>,
    end: Option<String>,
    pitch: Option<f64>,
    skin: Option<String>,
    skip_intro: bool,
    offset_ms: Option<i32>,
    force_rerender: bool,
}

#[derive(Deserialize)]
struct SubmitPayload {
    #[serde(default)]
    filename: String,
    osr_base64: String,
    #[serde(default)]
    options: WebRenderOptionsInput,
    #[serde(default)]
    settings: Option<WebDanserSettings>,
    #[serde(default)]
    raw_settings_json: Option<String>,
}

async fn handle_submit(ctx: &Context, http_req: &HttpRequest) -> (u16, serde_json::Value) {
    let client_ip = http_req.client_ip.as_str();
    let body = http_req.body.as_slice();
    let can_use_900p = crate::core::web_auth::resolve_web_session_osu_id(ctx, http_req)
        .await
        .is_some();

    let req: SubmitPayload = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(err) => {
            return (
                400,
                serde_json::json!({ "ok": false, "error": format!("Invalid JSON payload: {err}") }),
            )
        }
    };

    let osr_bytes = match decode_base64_limited(&req.osr_base64, MAX_OSR_BYTES) {
        Ok(b) => b,
        Err(msg) => return (400, serde_json::json!({ "ok": false, "error": msg })),
    };

    let raw_replay = match Replay::from_bytes(&osr_bytes) {
        Ok(r) => r,
        Err(_) => {
            return (
                400,
                serde_json::json!({ "ok": false, "error": "Could not parse .osr replay file." }),
            )
        }
    };
    if raw_replay.mode != Mode::Standard {
        return (
            400,
            serde_json::json!({ "ok": false, "error": "Only osu!standard replays are supported." }),
        );
    }

    let is_lazer = is_lazer_replay(raw_replay.version);
    let slim = ReplaySlim::from(raw_replay);
    if slim.beatmap_hash.as_deref().is_none_or(str::is_empty) {
        return (
            400,
            serde_json::json!({ "ok": false, "error": "Replay file is missing a beatmap hash." }),
        );
    }

    // Parse per-render options (matching Discord /render validation)
    let start_secs = match req.options.start.as_deref().map(str::trim).filter(|s| !s.is_empty() && *s != "0") {
        Some(s) => match TimePoints::parse_single(s) {
            Ok(v) => v,
            Err(msg) => return (400, serde_json::json!({ "ok": false, "error": format!("Start time: {msg}") })),
        },
        None => 0,
    };
    let end_secs = match req.options.end.as_deref().map(str::trim).filter(|s| !s.is_empty() && *s != "0") {
        Some(s) => match TimePoints::parse_single(s) {
            Ok(v) => v,
            Err(msg) => return (400, serde_json::json!({ "ok": false, "error": format!("End time: {msg}") })),
        },
        None => 0,
    };
    if end_secs > 0 && end_secs <= start_secs {
        return (
            400,
            serde_json::json!({ "ok": false, "error": "End timestamp must be greater than start timestamp." }),
        );
    }
    let pitch = match req.options.pitch {
        Some(p) if !p.is_finite() || !(0.5..=5.0).contains(&p) => {
            return (
                400,
                serde_json::json!({ "ok": false, "error": "Pitch must be between 0.5 and 5.0." }),
            )
        }
        other => other,
    };
    let offset_ms = req.options.offset_ms.map(|v| v.clamp(-5000, 5000));
    let override_skin = match req.options.skin.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => match resolve_skin_name(ctx, s) {
            Ok(Some(resolved)) => Some(resolved),
            Ok(None) => {
                return (
                    400,
                    serde_json::json!({ "ok": false, "error": format!("Skin `{s}` not found on server.") }),
                )
            }
            Err(_) => {
                return (
                    500,
                    serde_json::json!({ "ok": false, "error": "Failed to read installed skins." }),
                )
            }
        },
        None => None,
    };

    // Build sanitized DanserSettings
    let mut danser_settings = match load_default_danser_settings().await {
        Ok(s) => s,
        Err(err) => {
            warn!(?err, "Failed loading default.json for web render");
            return (
                500,
                serde_json::json!({ "ok": false, "error": "Server default settings are unavailable." }),
            );
        }
    };

    if let Some(ref raw_json) = req.raw_settings_json {
        if raw_json.len() > MAX_RAW_SETTINGS_BYTES {
            return (
                400,
                serde_json::json!({ "ok": false, "error": "Imported settings JSON exceeds 256 KB limit." }),
            );
        }
        if !raw_json.trim().is_empty() {
            match serde_json::from_str::<DanserSettings>(raw_json) {
                Ok(mut uploaded) => {
                    uploaded.sanitize_user_upload();
                    danser_settings.audio = uploaded.audio;
                    danser_settings.input = uploaded.input;
                    danser_settings.gameplay = uploaded.gameplay;
                    danser_settings.cursor = uploaded.cursor;
                    danser_settings.objects = uploaded.objects;
                    danser_settings.playfield = uploaded.playfield;
                    danser_settings.skin.use_colors_from_skin = uploaded.skin.use_colors_from_skin;
                    danser_settings.skin.use_beatmap_colors = uploaded.skin.use_beatmap_colors;
                    danser_settings.skin.cursor = uploaded.skin.cursor;
                }
                Err(err) => {
                    return (
                        400,
                        serde_json::json!({ "ok": false, "error": format!("Invalid imported Danser JSON: {err}") }),
                    );
                }
            }
        }
    }

    if let Some(ref ws) = req.settings {
        if let Err(msg) = ws.apply_to_danser(ctx, &mut danser_settings, can_use_900p) {
            return (400, serde_json::json!({ "ok": false, "error": msg }));
        }
    }
    danser_settings.sanitize_user_upload();
    danser_settings.clamp_render_quality(can_use_900p);

    let token = generate_token();
    let config = BotConfig::get();
    let settings_name = format!("web_{token}");
    let settings_path = config
        .paths
        .danser()
        .join(format!("settings/{settings_name}.json"));

    if let Err(err) = write_json_atomic(&settings_path, &danser_settings, true).await {
        warn!(?err, "Failed writing temporary web render settings");
        return (
            500,
            serde_json::json!({ "ok": false, "error": "Failed preparing render settings." }),
        );
    }

    let safe_base = sanitize_filename(&req.filename);
    let osr_path: PathBuf = config
        .paths
        .downloads()
        .join(format!("web_{token}_{safe_base}.osr"));
    if let Err(err) = tokio::fs::write(&osr_path, &osr_bytes).await {
        let _ = tokio::fs::remove_file(&settings_path).await;
        warn!(?err, "Failed writing web render .osr file");
        return (
            500,
            serde_json::json!({ "ok": false, "error": "Failed saving uploaded replay." }),
        );
    }

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let handle = Arc::new(WebJobHandle {
        token: token.clone(),
        client_ip: client_ip.to_owned(),
        settings_name,
        settings_path: settings_path.clone(),
        state: RwLock::new(WebJobState::new_queued(None, now)),
    });

    let opts = RenderOptions {
        start: start_secs,
        end: end_secs,
        pitch,
        skin: override_skin,
        skip_intro: req.options.skip_intro,
        offset_ms,
        force_rerender: req.options.force_rerender,
        skip_info_embed: true,
    };

    let replay_data = ReplayData::for_web(osr_path.clone(), slim, is_lazer, opts, Arc::clone(&handle));
    match ctx
        .replay_queue
        .push_web(replay_data, handle, MAX_WEB_JOBS_PER_IP, MAX_TOTAL_QUEUE_LEN)
        .await
    {
        Ok((position, queue_length)) => {
            info!(token = %token, position, queue_length, "Enqueued web replay render");
            (
                200,
                serde_json::json!({
                    "ok": true,
                    "job_id": token,
                    "position": position,
                    "queue_length": queue_length,
                }),
            )
        }
        Err(msg) => {
            let _ = tokio::fs::remove_file(&osr_path).await;
            let _ = tokio::fs::remove_file(&settings_path).await;
            (429, serde_json::json!({ "ok": false, "error": msg }))
        }
    }
}

async fn handle_get_job(ctx: &Context, token: &str) -> (u16, serde_json::Value) {
    if !is_valid_token(token) {
        return (400, serde_json::json!({ "ok": false, "error": "Invalid job ID" }));
    }
    let Some(handle) = ctx.replay_queue.get_web_job(token).await else {
        return (404, serde_json::json!({ "ok": false, "error": "Render job not found or expired" }));
    };
    let (position, queue_length) = ctx.replay_queue.web_job_queue_info(token).await;
    let st = handle.state.read().await.clone();
    (
        200,
        serde_json::json!({
            "ok": true,
            "job_id": handle.token,
            "position": position,
            "queue_length": queue_length,
            "status": st.status,
            "progress": st.progress,
            "title": st.title,
            "watch_url": st.watch_url,
            "watch_path": st.watch_path,
            "video_url": st.video_url,
            "thumbnail_url": st.thumbnail_url,
            "error": st.error,
            "created_at": st.created_at,
            "finished_at": st.finished_at,
        }),
    )
}

async fn handle_cancel_job(ctx: &Context, token: &str) -> (u16, serde_json::Value) {
    if !is_valid_token(token) {
        return (400, serde_json::json!({ "ok": false, "error": "Invalid job ID" }));
    }
    let cancelled = ctx.replay_queue.cancel_web_job(token).await;
    (200, serde_json::json!({ "ok": true, "cancelled": cancelled }))
}

fn is_valid_token(s: &str) -> bool {
    (16..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn generate_token() -> String {
    if let Ok(csprng_hex) = crate::core::web_auth::generate_random_hex_32() {
        return csprng_hex;
    }
    let seq = TOKEN_SEQ.fetch_add(1, Ordering::Relaxed);
    let now = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    let mut h1 = RandomState::new().build_hasher();
    h1.write_u64(seq);
    h1.write_i128(now);
    let a = h1.finish();
    let mut h2 = RandomState::new().build_hasher();
    h2.write_u64(a);
    h2.write_u64(seq.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let b = h2.finish();
    format!("{a:016x}{b:016x}")
}

fn sanitize_filename(name: &str) -> String {
    let stem = name.rsplit(['/', '\\']).next().unwrap_or("replay");
    let stem = stem
        .strip_suffix(".osr")
        .or_else(|| stem.strip_suffix(".OSR"))
        .unwrap_or(stem);
    let mut out = String::with_capacity(stem.len().min(48));
    for c in stem.chars() {
        if out.len() >= 48 {
            break;
        }
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "replay".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn url_decode_display(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn decode_base64_limited(input: &str, max_bytes: usize) -> std::result::Result<Vec<u8>, &'static str> {
    let clean: Vec<u8> = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if clean.is_empty() || !clean.len().is_multiple_of(4) {
        return Err("Invalid base64 replay data.");
    }
    let estimated = (clean.len() / 4) * 3;
    if estimated > max_bytes + 3 {
        return Err("Replay file exceeds 5 MB limit.");
    }

    let mut out = Vec::with_capacity(estimated);
    for chunk in clean.chunks_exact(4) {
        let v0 = b64_val(chunk[0]).ok_or("Invalid base64 character.")?;
        let v1 = b64_val(chunk[1]).ok_or("Invalid base64 character.")?;
        let pad2 = chunk[2] == b'=';
        let pad3 = chunk[3] == b'=';
        let v2 = if pad2 { 0 } else { b64_val(chunk[2]).ok_or("Invalid base64 character.")? };
        let v3 = if pad3 { 0 } else { b64_val(chunk[3]).ok_or("Invalid base64 character.")? };

        out.push((v0 << 2) | (v1 >> 4));
        if !pad2 {
            out.push((v1 << 4) | (v2 >> 2));
        }
        if !pad3 {
            out.push((v2 << 6) | v3);
        }
        if out.len() > max_bytes {
            return Err("Replay file exceeds 5 MB limit.");
        }
    }
    Ok(out)
}

fn b64_val(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_base64_limited() {
        assert_eq!(decode_base64_limited("aGVsbG8=", 16).unwrap(), b"hello");
        assert_eq!(decode_base64_limited("b3N1IQ==", 16).unwrap(), b"osu!");
        assert!(decode_base64_limited("aGVsbG8=", 3).is_err());
        assert!(decode_base64_limited("not!base64", 32).is_err());
    }

    #[test]
    fn test_sanitize_filename_and_token() {
        assert_eq!(sanitize_filename("../weird name (1).osr"), "weird_name__1_".trim_matches('_'));
        let tok = generate_token();
        assert!(is_valid_token(&tok));
        assert_eq!(tok.len(), 64);

        let lim = IpRateLimiter::new();
        assert!(lim.check_and_record("203.0.113.5", "render_inspect", 2));
        assert!(lim.check_and_record("203.0.113.5", "render_inspect", 2));
        assert!(!lim.check_and_record("203.0.113.5", "render_inspect", 2));
        assert!(lim.check_and_record("203.0.113.6", "render_inspect", 2));

        let mut ds = DanserSettings::default();
        ds.skin.current_skin = "..".to_owned();
        ds.normalize_current_skin();
        assert_eq!(ds.skin.current_skin, "default");
        assert!(ds.has_normalized_skin());
    }
}
