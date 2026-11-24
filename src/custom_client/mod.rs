use std::ffi::OsStr;
use std::path::Path;
use tokio::time::Duration;

use crate::core::BotConfig;
use bytes::Bytes;
use eyre::{bail, ensure, Context as _, Result};
use leaky_bucket_lite::LeakyBucket;
use reqwest::Client;
use serde::Deserialize;
use twilight_model::{
    channel::Attachment,
    id::{marker::UserMarker, Id},
};

static MY_USER_AGENT: &str = env!("CARGO_PKG_NAME");

#[derive(Copy, Clone, Eq, Hash, PartialEq)]
#[repr(u8)]
enum Site {
    DiscordAttachment,
    DownloadKitsu,
    DownloadNerinyan,
    DownloadCatboy,
    ShishaMezo,
    OsuOsuFile,
}

pub struct CustomClient {
    client: Client,
    ratelimiters: [LeakyBucket; 6],
    upload: UploadData,
}

struct UploadData {
    secret: &'static str,
    url: &'static str,
}

impl From<&'static BotConfig> for UploadData {
    #[inline]
    fn from(config: &'static BotConfig) -> Self {
        Self {
            secret: &config.tokens.upload_secret,
            url: &config.upload_url,
        }
    }
}

const MAX_DOWNLOAD_BYTES: usize = 250 * 1024 * 1024;
pub const MAX_SKIN_BYTES: usize = 150 * 1024 * 1024;

impl CustomClient {
    pub fn new() -> Self {
        let client = Client::builder()
            .user_agent(MY_USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .expect("failed to create reqwest CustomClient");

        let ratelimiter = |per_second| {
            LeakyBucket::builder()
                .max(per_second)
                .tokens(per_second)
                .refill_interval(Duration::from_millis(1000 / per_second as u64))
                .refill_amount(1)
                .build()
        };

        let ratelimiters = [
            ratelimiter(2), // DiscordAttachment
            ratelimiter(1), // DownloadKitsu
            ratelimiter(1), // DownloadNerinyan
            ratelimiter(1), // DownloadCatboy
            ratelimiter(1), // ShishaMezo
            ratelimiter(2), // OsuOsuFile
        ];

        Self {
            client,
            ratelimiters,
            upload: UploadData::from(BotConfig::get()),
        }
    }

    async fn ratelimit(&self, site: Site) {
        self.ratelimiters[site as usize].acquire_one().await
    }

    async fn read_response_bytes_limited(
        mut response: reqwest::Response,
        url: &str,
        max_bytes: usize,
    ) -> Result<Bytes> {
        let content_len = response.content_length();
        if let Some(len) = content_len {
            ensure!(
                len <= max_bytes as u64,
                "response from {url} exceeds {} MB size limit",
                max_bytes / (1024 * 1024)
            );
        }

        let initial_cap = content_len
            .and_then(|l| usize::try_from(l).ok())
            .map_or(0, |l| l.min(max_bytes).min(8 * 1024 * 1024));
        let mut buf = Vec::with_capacity(initial_cap);

        while let Some(chunk) = response
            .chunk()
            .await
            .with_context(|| format!("failed to read response chunk from {url}"))?
        {
            if buf.len().saturating_add(chunk.len()) > max_bytes {
                bail!(
                    "response from {url} exceeds {} MB size limit",
                    max_bytes / (1024 * 1024)
                );
            }
            buf.extend_from_slice(&chunk);
        }

        Ok(Bytes::from(buf))
    }

    async fn make_get_request(&self, url: impl AsRef<str>, site: Site) -> Result<Bytes> {
        self.make_get_request_limited(url, site, MAX_DOWNLOAD_BYTES)
            .await
    }

    async fn make_get_request_limited(
        &self,
        url: impl AsRef<str>,
        site: Site,
        max_bytes: usize,
    ) -> Result<Bytes> {
        let url = url.as_ref();
        let redacted_url = url.split('?').next().unwrap_or(url);
        self.ratelimit(site).await;
        tracing::debug!("Sending GET request to: {redacted_url}");

        let response = self
            .client
            .get(url)
            .timeout(Duration::from_secs(90))
            .send()
            .await
            .with_context(|| format!("failed to receive GET response from {redacted_url}"))?;

        let status = response.status();
        tracing::debug!("Response status: {status} for url: {redacted_url}");

        if !status.is_success() {
            bail!("failed with status code {status} when requesting {redacted_url}");
        }

        let bytes = Self::read_response_bytes_limited(response, redacted_url, max_bytes).await?;

        tracing::debug!(
            "Got {} bytes from {redacted_url}, starts_with PK: {}",
            bytes.len(),
            bytes.starts_with(b"PK")
        );

        Ok(bytes)
    }

    pub async fn get_discord_attachment_limited(
        &self,
        attachment: &Attachment,
        max_bytes: usize,
    ) -> Result<Bytes> {
        self.make_get_request_limited(&attachment.url, Site::DiscordAttachment, max_bytes)
            .await
    }

    pub async fn get_skin_from_url(&self, url: &str) -> Result<Bytes> {
        const MAX_REDIRECTS: usize = 5;
        self.ratelimit(Site::DownloadCatboy).await;

        let mut current_url = url.to_owned();
        for _ in 0..=MAX_REDIRECTS {
            let target = crate::core::ai::search::validate_and_resolve_url(&current_url).await?;
            let mut client_builder = Client::builder()
                .user_agent(MY_USER_AGENT)
                .no_proxy()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(120))
                .redirect(reqwest::redirect::Policy::none());
            if !target.pinned_addrs.is_empty() && target.host.parse::<std::net::IpAddr>().is_err() {
                client_builder = client_builder.resolve_to_addrs(&target.host, &target.pinned_addrs);
            }
            let client = client_builder
                .build()
                .context("failed to build SSRF-safe download client")?;

            let response = client
                .get(target.url.clone())
                .send()
                .await
                .with_context(|| format!("failed to receive GET response from {current_url}"))?;

            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| eyre::eyre!("redirect response missing Location header"))?;
                let next_url = target
                    .url
                    .join(location)
                    .context("invalid redirect Location URL")?;
                current_url = next_url.to_string();
                continue;
            }

            if !response.status().is_success() {
                bail!(
                    "failed with status code {} when requesting {current_url}",
                    response.status()
                );
            }

            return Self::read_response_bytes_limited(response, &current_url, MAX_SKIN_BYTES)
                .await;
        }

        bail!("too many redirects while downloading skin")
    }

    async fn download_mapset(&self, url: String, site: Site, source: &str) -> Result<Bytes> {
        let bytes = self.make_get_request(url, site).await?;
        ensure!(bytes.starts_with(b"PK"), "{source} returned invalid data");
        Ok(bytes)
    }

    pub async fn download_kitsu_mapset(&self, mapset_id: u32) -> Result<Bytes> {
        self.download_mapset(
            format!("https://osu.direct/api/d/{mapset_id}"),
            Site::DownloadKitsu,
            "osu.direct",
        )
        .await
    }

    pub async fn download_nerinyan_mapset(&self, mapset_id: u32) -> Result<Bytes> {
        self.download_mapset(
            format!("https://nerinyan.moe/d/{mapset_id}"),
            Site::DownloadNerinyan,
            "nerinyan",
        )
        .await
    }

    pub async fn download_catboy_mapset(&self, mapset_id: u32) -> Result<Bytes> {
        self.download_mapset(
            format!("https://mirror.nekoha.moe/api4/download/{mapset_id}"),
            Site::DownloadCatboy,
            "catboy",
        )
        .await
    }

    /// Fetch the official `.osu` difficulty file directly from osu.ppy.sh (used when a mirror `.osz` is stale).
    pub async fn download_official_osu_file(&self, map_id: u32) -> Result<Bytes> {
        const MAX_OSU_FILE_BYTES: usize = 16 * 1024 * 1024;
        let bytes = self
            .make_get_request_limited(
                format!("https://osu.ppy.sh/osu/{map_id}"),
                Site::OsuOsuFile,
                MAX_OSU_FILE_BYTES,
            )
            .await?;
        let head = &bytes[..bytes.len().min(64)];
        ensure!(
            head.windows(b"osu file format".len())
                .any(|w| w == b"osu file format"),
            "osu.ppy.sh/osu/{map_id} did not return a valid .osu file",
        );
        Ok(bytes)
    }

    pub async fn upload_video(
        &self,
        title: &str,
        author: Id<UserMarker>,
        path: impl AsRef<Path>,
        beatmap: &str,
        hash: &str,
        extra_meta_json: Option<&str>,
    ) -> Result<UploadResponse> {
        let path = path.as_ref();
        let extension = path.extension();
        ensure!(
            matches!(extension.and_then(OsStr::to_str), Some("mp4")),
            "unexpected file extension {extension:?} while creating multipart, expected mp4",
        );

        let file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("failed to open video file at {path:?} for upload"))?;
        let video_len = file
            .metadata()
            .await
            .with_context(|| format!("failed to get metadata for video file at {path:?}"))?
            .len();

        let filename = path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("video.mp4")
            .to_string();

        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
        let file_part = reqwest::multipart::Part::stream_with_length(body, video_len)
            .file_name(filename)
            .mime_str("video/mp4")
            .context("invalid mime type for video")?;

        let mut form = reqwest::multipart::Form::new()
            .part("video", file_part)
            .text("title", title.to_string())
            .text("author", author.to_string())
            .text("secret", self.upload.secret.to_string())
            .text("hash", hash.to_string())
            .text("beatmap", beatmap.to_string());
        if let Some(meta) = extra_meta_json {
            form = form.text("meta", meta.to_owned());
        }

        self.ratelimit(Site::ShishaMezo).await;

        let response = self
            .client
            .post(self.upload.url)
            .header("X-Upload-Secret", self.upload.secret)
            .timeout(Duration::from_secs(600))
            .multipart(form)
            .send()
            .await
            .context("failed to receive POST response for video upload")?;

        let status = response.status();
        let bytes = Self::read_response_bytes_limited(response, self.upload.url, 1024 * 1024)
            .await
            .context("failed to extract upload response bytes")?;
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes);
            bail!("upload failed with status code {status}: {body}");
        }

        serde_json::from_slice(&bytes).with_context(|| {
            let text = String::from_utf8_lossy(&bytes);
            format!("failed to deserialize upload response: {text}")
        })
    }

    /// Verify with a quick HEAD request whether a cached replay URL is still reachable.
    pub async fn check_url_exists(&self, url: &str) -> bool {
        let Ok(resp) = self
            .client
            .head(url)
            .timeout(Duration::from_secs(3))
            .send()
            .await
        else {
            return false;
        };
        resp.status().is_success()
    }
}

#[derive(Deserialize)]
pub struct UploadResponse {
    pub error: u16,
    pub text: String,
    #[serde(default)]
    pub video_url: Option<String>,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
}
