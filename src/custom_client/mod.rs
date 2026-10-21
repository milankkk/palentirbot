use std::path::Path;

use crate::core::BotConfig;
use bytes::Bytes;
use eyre::{bail, ensure, Context as _, Result};
use leaky_bucket_lite::LeakyBucket;
use reqwest::Client;
use serde::Deserialize;
use std::ffi::OsStr;
use tokio::time::Duration;
use twilight_model::{
    channel::Attachment,
    id::{marker::UserMarker, Id},
};

static MY_USER_AGENT: &str = env!("CARGO_PKG_NAME");

#[derive(Copy, Clone, Eq, Hash, PartialEq)]
#[repr(u8)]
enum Site {
    DiscordAttachment,
    DownloadOfficial,
    DownloadChimu,
    DownloadKitsu,
    DownloadNerinyan,
    DownloadCatboy,
    OsuReplay,
    ShishaMezo,
}

pub struct CustomClient {
    client: Client,
    ratelimiters: [LeakyBucket; 8],
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
            ratelimiter(1), // DownloadOfficial
            ratelimiter(1), // DownloadChimu
            ratelimiter(1), // DownloadKitsu
            ratelimiter(1), // DownloadNerinyan
            ratelimiter(1), // DownloadCatboy
            ratelimiter(1), // OsuReplay
            ratelimiter(1), // ShishaMezo
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

    pub async fn get_raw_replay(&self, score_id: u64) -> eyre::Result<Vec<u8>> {
        let url = format!(
            "https://osu.ppy.sh/api/get_replay?k={apikey}&s={score_id}",
            apikey = BotConfig::get().tokens.osu_api_key,
        );

        let bytes = self.make_get_request(url, Site::OsuReplay).await?;
        let text = String::from_utf8_lossy(&bytes);

        warn!(
            "Got {} bytes from osu replay api for score {}, body: {}",
            bytes.len(),
            score_id,
            text
        );

        #[derive(serde::Deserialize)]
        struct RawReplay {
            content: String,
        }

        #[derive(serde::Deserialize)]
        struct OsuApiError {
            error: String,
        }

        if let Ok(err) = serde_json::from_slice::<OsuApiError>(&bytes) {
            eyre::bail!(
                "osu replay api returned error for score {score_id}: {}",
                err.error
            );
        }

        let RawReplay { content } = serde_json::from_slice::<RawReplay>(&bytes)
            .with_context(|| format!("failed to deserialize raw replay response: {text}"))?;

        use base64::prelude::*;
        BASE64_STANDARD.decode(content).context("failed to decode replay through base64")
    }

    pub async fn getrawreplay_for_user_map(
        &self,
        beatmap_id: u32,
        user_id: u32,
        mode: u8,
    ) -> eyre::Result<Vec<u8>> {
        let url = format!(
            "https://osu.ppy.sh/api/get_replay?k={apikey}&b={beatmap_id}&u={user_id}&m={mode}&type=id",
            apikey = BotConfig::get().tokens.osu_api_key,
        );

        let bytes = self.make_get_request(url, Site::OsuReplay).await?;
        let text = String::from_utf8_lossy(&bytes);

        #[derive(serde::Deserialize)]
        struct RawReplay {
            content: String,
        }

        #[derive(serde::Deserialize)]
        struct OsuApiError {
            error: String,
        }

        if let Ok(err) = serde_json::from_slice::<OsuApiError>(&bytes) {
            eyre::bail!("osu replay api returned error: {}", err.error);
        }

        let RawReplay { content } = serde_json::from_slice::<RawReplay>(&bytes)
            .with_context(|| format!("failed to deserialize raw replay response: {text}"))?;

        use base64::prelude::*;
        BASE64_STANDARD.decode(content).context("failed to decode replay through base64")
    }

    pub async fn download_official_mapset(&self, mapset_id: u32, osu_token: &str) -> Result<Bytes> {
        let url = format!("https://osu.ppy.sh/beatmapsets/{}/download", mapset_id);
        self.ratelimit(Site::DownloadOfficial).await;

        let response = self
            .client
            .get(&url)
            .bearer_auth(osu_token)
            .send()
            .await
            .with_context(|| format!("failed to receive official download response from {url}"))?;

        let status = response.status();
        if !status.is_success() {
            bail!("failed with status code {status} when requesting {url}");
        }

        let bytes = response
            .bytes()
            .await
            .context("failed to extract official download response bytes")?;

        ensure!(
            bytes.starts_with(b"PK"),
            "official download returned invalid data"
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

        let response = self
            .client
            .post(self.upload.url)
            .multipart(form)
            .send()
            .await
            .context("failed to receive POST response for video upload")?;

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

#[derive(Deserialize)]
pub struct OsuReplayResponse {
    pub content: String,
    pub encoding: String,
}
