use std::{hash::Hash, path::Path};

use crate::core::BotConfig;
use bytes::Bytes;
use eyre::{Context as _, ContextCompat, Result};
use http::{header::CONTENT_LENGTH, Response};
use hyper::{
    client::{connect::dns::GaiResolver, Client as HyperClient, HttpConnector},
    header::{CONTENT_TYPE, USER_AGENT},
    Body, Method, Request,
};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use leaky_bucket_lite::LeakyBucket;
use serde::Deserialize;
use tokio::time::Duration;
use twilight_model::{
    channel::Attachment,
    id::{marker::UserMarker, Id},
};

use self::multipart::Multipart;

mod multipart;

static MY_USER_AGENT: &str = env!("CARGO_PKG_NAME");

#[derive(Copy, Clone, Eq, Hash, PartialEq)]
#[repr(u8)]
enum Site {
    DiscordAttachment,
    DownloadChimu,
    DownloadKitsu,
    DownloadNerinyan,
    DownloadCatboy,
    OsuReplay,
    ShishaMezo,
}

type Client = HyperClient<HttpsConnector<HttpConnector<GaiResolver>>, Body>;

pub struct CustomClient {
    client: Client,
    ratelimiters: [LeakyBucket; 7],
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
        let connector = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();

        let client = HyperClient::builder().build(connector);

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
        let mut url = url.as_ref().to_owned();
        self.ratelimit(site).await;
        let mut redirects = 0;

        loop {
            trace!("GET request to url {url}");
            warn!("Sending GET request to: {url}");

            let req = Request::builder()
                .uri(&url)
                .method(Method::GET)
                .header(USER_AGENT, MY_USER_AGENT)
                .body(Body::empty())
                .context("failed to build GET request")?;

            let response = self
                .client
                .request(req)
                .await
                .context("failed to receive GET response")?;

            let status = response.status();
            warn!("Response status: {status} for url: {url}");

            if status.is_redirection() {
                redirects += 1;
                ensure!(redirects < 10, "too many redirects for {url}");

                let location = response
                    .headers()
                    .get(hyper::header::LOCATION)
                    .context("redirect with no Location header")?
                    .to_str()
                    .context("Location header was not valid UTF-8")?
                    .to_owned();

                warn!("Redirect #{redirects} -> {location}");

                url = if location.starts_with("http") {
                    location
                } else {
                    format!("https://{}{}", url.split('/').nth(2).unwrap_or(""), location)
                };

                continue;
            }

            let bytes = Self::error_for_status(response, &url).await?;
            warn!(
                "Got {} bytes from {url}, starts_with PK: {}",
                bytes.len(),
                bytes.starts_with(b"PK")
            );

            return Ok(bytes);
        }
    }



    async fn make_post_request(
        &self,
        url: impl AsRef<str>,
        site: Site,
        form: Multipart,
    ) -> Result<Bytes> {
        let url = url.as_ref();
        trace!("POST request to url {url}");

        let content_type = format!("multipart/form-data; boundary={}", form.boundary());
        let form = form.finish();

        let req = Request::builder()
            .method(Method::POST)
            .uri(url)
            .header(USER_AGENT, MY_USER_AGENT)
            .header(CONTENT_TYPE, content_type)
            .header(CONTENT_LENGTH, form.len())
            .body(Body::from(form))
            .context("failed to build POST request")?;

        self.ratelimit(site).await;

        let response = self
            .client
            .request(req)
            .await
            .context("failed to receive POST response")?;

        Self::error_for_status(response, url).await
    }

    async fn error_for_status(response: Response<Body>, url: &str) -> Result<Bytes> {
        let status = response.status();

        if status.is_client_error() || status.is_server_error() {
            bail!("failed with status code {status} when requesting {url}")
        } else {
            let bytes = hyper::body::to_bytes(response.into_body())
                .await
                .context("failed to extract response bytes")?;

            Ok(bytes)
        }
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
            eyre::bail!("osu replay api returned error for score {score_id}: {}", err.error);
        }

        let RawReplay { content } = serde_json::from_slice::<RawReplay>(&bytes)
            .with_context(|| format!("failed to deserialize raw replay response: {text}"))?;

        base64::decode(content).context("failed to decode replay through base64")
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

        base64::decode(content).context("failed to decode replay through base64")
    }


    pub async fn get_discord_attachment(&self, attachment: &Attachment) -> Result<Bytes> {
        self.make_get_request(&attachment.url, Site::DiscordAttachment)
            .await
    }
    
    pub async fn get_skin_from_url(&self, url: &str) -> Result<Bytes> {
        self.make_get_request(url, Site::DownloadCatboy).await
    }
    
    pub async fn download_chimu_mapset(&self, mapset_id: u32) -> Result<Bytes> {
        let url = format!("https://osu.direct/api/d/{mapset_id}");
        let bytes = self.make_get_request(url, Site::DownloadChimu).await?;
        ensure!(bytes.starts_with(b"PK"), "catboy returned invalid data");
        Ok(bytes)
    }

    pub async fn download_kitsu_mapset(&self, mapset_id: u32) -> Result<Bytes> {
        let url = format!("https://osu.direct/api/d/{mapset_id}");
        let bytes = self.make_get_request(url, Site::DownloadKitsu).await?;
        ensure!(bytes.starts_with(b"PK"), "osu.direct returned invalid data");
        Ok(bytes)
    }

    pub async fn download_nerinyan_mapset(&self, mapset_id: u32) -> Result<Bytes> {
        let url = format!("https://osu.direct/api/d/{mapset_id}");
        let bytes = self.make_get_request(url, Site::DownloadNerinyan).await?;
        ensure!(bytes.starts_with(b"PK"), "nerinyan returned invalid data");
        Ok(bytes)
    }
    pub async fn download_catboy_mapset(&self, mapset_id: u32) -> Result<Bytes> {
        let url = format!("https://osu.direct/api/d/{mapset_id}");
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
        let form = Multipart::new()
            .push_file("video", path)
            .await
            .context("failed to create multipart form")?
            .push_text("title", title)
            .push_text("author", author)
            .push_text("secret", self.upload.secret)
            .push_text("hash", hash)
            .push_text("beatmap", beatmap);

        let bytes = self
            .make_post_request(self.upload.url, Site::ShishaMezo, form)
            .await?;

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

