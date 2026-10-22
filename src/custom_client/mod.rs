use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
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
    DownloadChimu,
    DownloadKitsu,
    DownloadNerinyan,
    DownloadCatboy,
    ShishaMezo,
}

pub struct CustomClient {
    client: Client,
    ratelimiters: [LeakyBucket; 6],
    upload: UploadData,
    upload_speed_bits: AtomicU64,
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

impl CustomClient {
    pub fn new() -> Self {
        let client = Client::builder()
            .user_agent(MY_USER_AGENT)
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
            ratelimiter(1), // DownloadChimu
            ratelimiter(1), // DownloadKitsu
            ratelimiter(1), // DownloadNerinyan
            ratelimiter(1), // DownloadCatboy
            ratelimiter(1), // ShishaMezo
        ];

        let default_speed: f64 = std::env::var("UPLOAD_MBPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(70.0);

        Self {
            client,
            ratelimiters,
            upload: UploadData::from(BotConfig::get()),
            upload_speed_bits: AtomicU64::new(default_speed.to_bits()),
        }
    }

    pub fn upload_speed_mbps(&self) -> f64 {
        f64::from_bits(self.upload_speed_bits.load(Ordering::Relaxed))
    }

    pub fn set_upload_speed_mbps(&self, mbps: f64) {
        let clamped = mbps.clamp(1.0, 10000.0);
        self.upload_speed_bits.store(clamped.to_bits(), Ordering::Relaxed);
    }

    pub fn update_upload_speed(&self, measured_mbps: f64) {
        let current = self.upload_speed_mbps();
        let updated = (current * 0.7) + (measured_mbps * 0.3);
        self.set_upload_speed_mbps(updated);
    }

    pub async fn measure_bandwidth(&self) -> Option<f64> {
        if let Ok(val) = std::env::var("UPLOAD_MBPS") {
            if let Ok(fixed) = val.parse::<f64>() {
                tracing::info!("Using manually configured UPLOAD_MBPS: {:.2} Mbps", fixed);
                self.set_upload_speed_mbps(fixed);
                return Some(fixed);
            }
        }

        tracing::info!("Measuring outbound upload speed for Discord caching...");
        const TEST_PAYLOAD_SIZE: usize = 2 * 1024 * 1024;
        let payload = vec![0u8; TEST_PAYLOAD_SIZE];
        let start = std::time::Instant::now();
        let res = self
            .client
            .post("https://speed.cloudflare.com/__up")
            .body(payload)
            .timeout(Duration::from_secs(8))
            .send()
            .await;

        match res {
            Ok(r) if r.status().is_success() => {
                let elapsed = start.elapsed().as_secs_f64();
                if elapsed > 0.05 {
                    let mbps = (TEST_PAYLOAD_SIZE as f64 * 8.0) / (elapsed * 1_000_000.0);
                    tracing::info!("Measured outbound upload bandwidth: {:.2} Mbps", mbps);
                    self.set_upload_speed_mbps(mbps);
                    return Some(mbps);
                }
            }
            Ok(r) => tracing::warn!("Speedtest endpoint returned HTTP status {}", r.status()),
            Err(e) => tracing::warn!("Could not measure upload bandwidth against benchmark endpoint: {e}"),
        }
        None
    }

    async fn ratelimit(&self, site: Site) {
        self.ratelimiters[site as usize].acquire_one().await
    }

    async fn make_get_request(&self, url: impl AsRef<str>, site: Site) -> Result<Bytes> {
        let url = url.as_ref();
        self.ratelimit(site).await;
        trace!("GET request to url {url}");
        warn!("Sending GET request to: {url}");

        let response = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("failed to receive GET response from {url}"))?;

        let status = response.status();
        warn!("Response status: {status} for url: {url}");

        if !status.is_success() {
            bail!("failed with status code {status} when requesting {url}");
        }

        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("failed to extract response bytes from {url}"))?;

        warn!(
            "Got {} bytes from {url}, starts_with PK: {}",
            bytes.len(),
            bytes.starts_with(b"PK")
        );

        Ok(bytes)
    }

    pub async fn get_discord_attachment(&self, attachment: &Attachment) -> Result<Bytes> {
        self.make_get_request(&attachment.url, Site::DiscordAttachment)
            .await
    }

    pub async fn get_skin_from_url(&self, url: &str) -> Result<Bytes> {
        let bytes = self.make_get_request(url, Site::DownloadCatboy).await?;
        const MAX_SKIN_BYTES: usize = 150 * 1024 * 1024; // 150 MB limit
        ensure!(bytes.len() <= MAX_SKIN_BYTES, "skin file exceeds 150MB limit");
        Ok(bytes)
    }

    pub async fn download_chimu_mapset(&self, mapset_id: u32) -> Result<Bytes> {
        let url = format!("https://api.chimu.moe/v1/download/{mapset_id}");
        let bytes = self.make_get_request(url, Site::DownloadChimu).await?;
        ensure!(bytes.starts_with(b"PK"), "chimu returned invalid data");
        Ok(bytes)
    }

    pub async fn download_kitsu_mapset(&self, mapset_id: u32) -> Result<Bytes> {
        let url = format!("https://osu.direct/api/d/{mapset_id}");
        let bytes = self.make_get_request(url, Site::DownloadKitsu).await?;
        ensure!(bytes.starts_with(b"PK"), "osu.direct returned invalid data");
        Ok(bytes)
    }

    pub async fn download_nerinyan_mapset(&self, mapset_id: u32) -> Result<Bytes> {
        let url = format!("https://nerinyan.moe/d/{mapset_id}");
        let bytes = self.make_get_request(url, Site::DownloadNerinyan).await?;
        ensure!(bytes.starts_with(b"PK"), "nerinyan returned invalid data");
        Ok(bytes)
    }

    pub async fn download_catboy_mapset(&self, mapset_id: u32) -> Result<Bytes> {
        let url = format!("https://mirror.nekoha.moe/api4/download/{mapset_id}");
        let bytes = self.make_get_request(url, Site::DownloadCatboy).await?;
        ensure!(bytes.starts_with(b"PK"), "catboy returned invalid data");
        Ok(bytes)
    }

    pub async fn upload_video(
        &self,
        title: &str,
        author: Id<UserMarker>,
        path: impl AsRef<Path>,
        beatmap: &str,
        hash: &str,
    ) -> Result<UploadResponse> {
        let path = path.as_ref();
        let extension = path.extension();
        ensure!(
            matches!(extension.and_then(OsStr::to_str), Some("mp4")),
            "unexpected file extension {extension:?} while creating multipart, expected mp4",
        );

        let video_bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("failed to read video file at {path:?} for upload"))?;

        let filename = path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("video.mp4")
            .to_string();

        let video_len = video_bytes.len();
        let file_part = reqwest::multipart::Part::bytes(video_bytes)
            .file_name(filename)
            .mime_str("video/mp4")
            .context("invalid mime type for video")?;

        let form = reqwest::multipart::Form::new()
            .part("video", file_part)
            .text("title", title.to_string())
            .text("author", author.to_string())
            .text("secret", self.upload.secret.to_string())
            .text("hash", hash.to_string())
            .text("beatmap", beatmap.to_string());

        self.ratelimit(Site::ShishaMezo).await;

        let upload_start = std::time::Instant::now();
        let response = self
            .client
            .post(self.upload.url)
            .multipart(form)
            .send()
            .await
            .context("failed to receive POST response for video upload")?;
        let upload_duration = upload_start.elapsed();

        if !self.upload.url.contains("localhost")
            && !self.upload.url.contains("127.0.0.1")
            && !self.upload.url.contains("fileserver")
        {
            let elapsed = upload_duration.as_secs_f64();
            if elapsed > 0.2 {
                let mbps = (video_len as f64 * 8.0) / (elapsed * 1_000_000.0);
                tracing::debug!("Measured remote upload throughput: {:.2} Mbps", mbps);
                self.update_upload_speed(mbps);
            }
        }

        let status = response.status();
        if !status.is_success() {
            bail!("upload failed with status code {status}");
        }

        let bytes = response
            .bytes()
            .await
            .context("failed to extract upload response bytes")?;

        serde_json::from_slice(&bytes).with_context(|| {
            let text = String::from_utf8_lossy(&bytes);
            format!("failed to deserialize upload response: {text}")
        })
    }
}

#[derive(Deserialize)]
pub struct UploadResponse {
    pub error: u16,
    pub text: String,
}
