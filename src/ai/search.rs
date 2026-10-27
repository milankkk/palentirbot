use std::time::Duration;

use eyre::{Context as _, Result};
use reqwest::Client;
use serde::Deserialize;

#[derive(Deserialize, Debug, Clone, Default)]
pub struct SearxResultItem {
    pub title: Option<String>,
    pub url: Option<String>,
    pub content: Option<String>,
    pub img_src: Option<String>,
    pub thumbnail_src: Option<String>,
    pub audio_src: Option<String>,
    pub iframe_src: Option<String>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct SearxResponse {
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub results: Vec<SearxResultItem>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum AiSearchTag {
    Web(String),
    Image(String),
    Audio(String),
}

pub struct SearxClient {
    client: Client,
    base_url: Option<String>,
}

impl SearxClient {
    pub fn new() -> Self {
        let base_url = std::env::var("SEARXNG_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_owned())
            .filter(|s| !s.is_empty())
            .or_else(|| Some("http://127.0.0.1:8080".to_owned()));

        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build SearXNG HTTP client");

        Self { client, base_url }
    }
}

impl Default for SearxClient {
    fn default() -> Self {
        Self::new()
    }
}

impl SearxClient {
    pub fn is_enabled(&self) -> bool {
        self.base_url.is_some()
    }

    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    /// Query SearXNG for results across categories (`general`, `images`, `music,files,videos`).
    pub async fn search(&self, query: &str, category: &str, safesearch: u8) -> Result<Vec<SearxResultItem>> {
        let base_url = self
            .base_url
            .as_deref()
            .ok_or_else(|| eyre::eyre!("SearXNG is not configured (SEARXNG_URL is empty)"))?;

        let url = format!("{base_url}/search");
        let res = self
            .client
            .get(&url)
            .query(&[
                ("q", query),
                ("categories", category),
                ("format", "json"),
                ("safesearch", &safesearch.to_string()),
            ])
            .send()
            .await
            .context("failed to query SearXNG")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            eyre::bail!("SearXNG returned HTTP {status}: {body}");
        }

        let parsed: SearxResponse = res.json().await.context("failed to parse SearXNG JSON")?;
        Ok(parsed.results)
    }
}

/// Check if an IP address is private, loopback, link-local, unspecified, broadcast, or documentation.
pub fn is_private_or_local_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
        }
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
    }
}

/// Validate a URL against SSRF attacks (rejecting non-http/https, localhost, and private/local IPs).
pub async fn validate_url_for_ssrf(url_str: &str) -> Result<reqwest::Url> {
    let parsed = reqwest::Url::parse(url_str).context("invalid URL")?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        eyre::bail!("URL scheme must be http or https");
    }

    let host_str = parsed
        .host_str()
        .ok_or_else(|| eyre::eyre!("URL has no host"))?;
    let host_lower = host_str.to_ascii_lowercase();

    if host_lower == "localhost" {
        eyre::bail!("Localhost is not allowed");
    }

    if let Ok(ip) = host_lower.parse::<std::net::IpAddr>() {
        if is_private_or_local_ip(ip) {
            eyre::bail!("Private or local IP addresses are not allowed");
        }
    } else {
        let port = parsed.port_or_known_default().unwrap_or(80);
        if let Ok(addrs) = tokio::net::lookup_host(format!("{host_lower}:{port}")).await {
            for addr in addrs {
                if is_private_or_local_ip(addr.ip()) {
                    eyre::bail!("Host resolves to private or local IP address");
                }
            }
        }
    }

    Ok(parsed)
}

const MAX_MEDIA_SIZE: usize = 15 * 1024 * 1024; // 15 MB

/// Safely download a media file (image, audio, or video) up to 15MB with SSRF protection.
pub async fn download_media(client: &Client, url_str: &str) -> Result<(String, Vec<u8>)> {
    let validated = validate_url_for_ssrf(url_str).await?;

    let res = client
        .get(validated.as_str())
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("failed to request media file")?;

    let status = res.status();
    if !status.is_success() {
        eyre::bail!("HTTP status {status} when downloading media");
    }

    let content_type = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let bytes = res.bytes().await.context("failed to read media bytes")?;
    if bytes.len() > MAX_MEDIA_SIZE {
        eyre::bail!("Media file exceeded 15MB limit ({} bytes)", bytes.len());
    }

    let ext = infer_media_extension(&content_type, &bytes, validated.path())
        .ok_or_else(|| eyre::eyre!("Unsupported or invalid media content-type: {content_type}"))?;

    let filename = format!("attachment.{ext}");
    Ok((filename, bytes.to_vec()))
}

/// Determine a valid extension (.jpg, .png, .webp, .gif, .mp3, .ogg, .wav, .mp4, .webm)
/// from Content-Type, magic bytes, or URL path.
pub fn infer_media_extension(content_type: &str, bytes: &[u8], path: &str) -> Option<&'static str> {
    // 1. Content-Type inspection
    if content_type.contains("image/jpeg") || content_type.contains("image/jpg") {
        return Some("jpg");
    }
    if content_type.contains("image/png") {
        return Some("png");
    }
    if content_type.contains("image/webp") {
        return Some("webp");
    }
    if content_type.contains("image/gif") {
        return Some("gif");
    }
    if content_type.contains("audio/mpeg") || content_type.contains("audio/mp3") {
        return Some("mp3");
    }
    if content_type.contains("audio/ogg") {
        return Some("ogg");
    }
    if content_type.contains("audio/wav") || content_type.contains("audio/x-wav") {
        return Some("wav");
    }
    if content_type.contains("video/mp4") {
        return Some("mp4");
    }
    if content_type.contains("video/webm") {
        return Some("webm");
    }

    // 2. Magic bytes inspection
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Some("jpg");
    }
    if bytes.len() >= 8 && bytes[0..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some("png");
    }
    if bytes.len() >= 6 && (&bytes[0..6] == b"GIF87a" || &bytes[0..6] == b"GIF89a") {
        return Some("gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    if bytes.len() >= 3 && &bytes[0..3] == b"ID3" {
        return Some("mp3");
    }
    if bytes.len() >= 4 && &bytes[0..4] == b"OggS" {
        return Some("ogg");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        return Some("wav");
    }
    if bytes.len() >= 8 && &bytes[4..8] == b"ftyp" {
        return Some("mp4");
    }
    if bytes.len() >= 4 && bytes[0..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Some("webm");
    }

    // 3. Fallback to path extension if content-type was generic
    let lower_path = path.to_ascii_lowercase();
    if lower_path.ends_with(".jpg") || lower_path.ends_with(".jpeg") {
        return Some("jpg");
    }
    if lower_path.ends_with(".png") {
        return Some("png");
    }
    if lower_path.ends_with(".webp") {
        return Some("webp");
    }
    if lower_path.ends_with(".gif") {
        return Some("gif");
    }
    if lower_path.ends_with(".mp3") {
        return Some("mp3");
    }
    if lower_path.ends_with(".ogg") {
        return Some("ogg");
    }
    if lower_path.ends_with(".wav") {
        return Some("wav");
    }
    if lower_path.ends_with(".mp4") {
        return Some("mp4");
    }
    if lower_path.ends_with(".webm") {
        return Some("webm");
    }

    None
}

/// Find the first closed search tag (`<search>...</search>`, `<search_image>...</search_image>`,
/// or `<search_audio>...</search_audio>`) in text. Also handles `</search>` closing variant
/// and `<web_search>`, `<image_search>`, `<audio_search>` aliases.
pub fn find_closed_search_tag(text: &str) -> Option<AiSearchTag> {
    let mut remaining = text;

    while let Some(open_idx) = remaining.find('<') {
        let after_open = &remaining[open_idx..];

        if let Some(rest) = after_open.strip_prefix("<search>").or_else(|| after_open.strip_prefix("<web_search>")) {
            if let Some(close_idx) = rest.find("</search>").or_else(|| rest.find("</web_search>")) {
                let query = rest[..close_idx].trim().to_string();
                if !query.is_empty() {
                    return Some(AiSearchTag::Web(query));
                }
            }
        } else if let Some(rest) = after_open.strip_prefix("<search_image>").or_else(|| after_open.strip_prefix("<image_search>")) {
            if let Some(close_idx) = rest.find("</search_image>").or_else(|| rest.find("</image_search>")).or_else(|| rest.find("</search>")).or_else(|| rest.find("</web_search>")) {
                let query = rest[..close_idx].trim().to_string();
                if !query.is_empty() {
                    return Some(AiSearchTag::Image(query));
                }
            }
        } else if let Some(rest) = after_open.strip_prefix("<search_audio>").or_else(|| after_open.strip_prefix("<audio_search>")) {
            if let Some(close_idx) = rest.find("</search_audio>").or_else(|| rest.find("</audio_search>")).or_else(|| rest.find("</search>")).or_else(|| rest.find("</web_search>")) {
                let query = rest[..close_idx].trim().to_string();
                if !query.is_empty() {
                    return Some(AiSearchTag::Audio(query));
                }
            }
        }

        remaining = &after_open[1..];
    }

    None
}

/// Extract `<search*>` and `<attach>` tags. Supports aliases like `<web_search>`, `<image_search>`.
pub fn extract_search_tags(text: &str) -> Vec<AiSearchTag> {
    let mut tags = Vec::new();
    let mut remaining = text;

    while let Some(open_idx) = remaining.find('<') {
        let after_open = &remaining[open_idx..];
        if let Some(rest) = after_open.strip_prefix("<search>").or_else(|| after_open.strip_prefix("<web_search>")) {
            let close_tag = if rest.contains("</search>") { "</search>" } else { "</web_search>" };
            if let Some(close_idx) = rest.find(close_tag) {
                let query = rest[..close_idx].trim().to_string();
                if !query.is_empty() {
                    tags.push(AiSearchTag::Web(query));
                }
                remaining = &rest[close_idx + close_tag.len()..];
                continue;
            }
        } else if let Some(rest) = after_open.strip_prefix("<search_image>").or_else(|| after_open.strip_prefix("<image_search>")) {
            let close_tag = if rest.contains("</search_image>") {
                "</search_image>"
            } else if rest.contains("</image_search>") {
                "</image_search>"
            } else if rest.contains("</web_search>") {
                "</web_search>"
            } else {
                "</search>"
            };
            if let Some(close_idx) = rest.find(close_tag) {
                let query = rest[..close_idx].trim().to_string();
                if !query.is_empty() {
                    tags.push(AiSearchTag::Image(query));
                }
                remaining = &rest[close_idx + close_tag.len()..];
                continue;
            }
        } else if let Some(rest) = after_open.strip_prefix("<search_audio>").or_else(|| after_open.strip_prefix("<audio_search>")) {
            let close_tag = if rest.contains("</search_audio>") {
                "</search_audio>"
            } else if rest.contains("</audio_search>") {
                "</audio_search>"
            } else if rest.contains("</web_search>") {
                "</web_search>"
            } else {
                "</search>"
            };
            if let Some(close_idx) = rest.find(close_tag) {
                let query = rest[..close_idx].trim().to_string();
                if !query.is_empty() {
                    tags.push(AiSearchTag::Audio(query));
                }
                remaining = &rest[close_idx + close_tag.len()..];
                continue;
            }
        }
        remaining = &after_open[1..];
    }

    tags
}

/// Extract `<attach>URL</attach>` or `<attachment>URL</attachment>` tags from the text.
pub fn extract_attach_tags(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut remaining = text;

    while let Some(open_idx) = remaining.find('<') {
        let after_open = &remaining[open_idx..];
        let (prefix, close_tag) = if let Some(rest) = after_open.strip_prefix("<attach>") {
            (rest, "</attach>")
        } else if let Some(rest) = after_open.strip_prefix("<attachment>") {
            (rest, "</attachment>")
        } else {
            remaining = &after_open[1..];
            continue;
        };

        if let Some(end) = prefix.find(close_tag) {
            let url = prefix[..end].trim().to_string();
            if !url.is_empty() {
                urls.push(url);
            }
            remaining = &prefix[end + close_tag.len()..];
        } else {
            break;
        }
    }

    urls
}

/// Strip `<search*>` and `<attach>` tags from text.
pub fn strip_tool_tags(text: &str) -> String {
    let mut result = text.to_string();

    for tag in &[
        "search",
        "web_search",
        "search_image",
        "image_search",
        "search_audio",
        "audio_search",
        "attach",
        "attachment",
    ] {
        let open_tag = format!("<{tag}>");
        let close_tag = format!("</{tag}>");

        while let Some(start) = result.find(&open_tag) {
            if let Some(end) = result[start..].find(&close_tag) {
                let end_idx = start + end + close_tag.len();
                result.drain(start..end_idx);
            } else if let Some(end) = result[start..].find("</search>") {
                let end_idx = start + end + "</search>".len();
                result.drain(start..end_idx);
            } else if let Some(end) = result[start..].find("</web_search>") {
                let end_idx = start + end + "</web_search>".len();
                result.drain(start..end_idx);
            } else {
                result.truncate(start);
                break;
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_closed_search_tag() {
        // Closed web search
        let text1 = "Thinking... <search>osu world cup 2026</search> continuing thought";
        assert_eq!(
            find_closed_search_tag(text1),
            Some(AiSearchTag::Web("osu world cup 2026".to_string()))
        );

        // Closed image search with standard tag
        let text2 = "Let me get an image <search_image>cute cat</search_image>";
        assert_eq!(
            find_closed_search_tag(text2),
            Some(AiSearchTag::Image("cute cat".to_string()))
        );

        // Closed image search with short </search> closing tag
        let text3 = "Let me get an image <search_image>orange tabby</search> more text";
        assert_eq!(
            find_closed_search_tag(text3),
            Some(AiSearchTag::Image("orange tabby".to_string()))
        );

        // Closed audio search
        let text4 = "<search_audio>osu theme song</search_audio>";
        assert_eq!(
            find_closed_search_tag(text4),
            Some(AiSearchTag::Audio("osu theme song".to_string()))
        );

        // Alias tags: <web_search>, <image_search>
        assert_eq!(
            find_closed_search_tag("<web_search>peppy</web_search>"),
            Some(AiSearchTag::Web("peppy".to_string()))
        );
        assert_eq!(
            find_closed_search_tag("<image_search>red rose</image_search>"),
            Some(AiSearchTag::Image("red rose".to_string()))
        );

        // Incomplete / unclosed tag (still streaming)
        let text5 = "Let me find <search_image>cute cat";
        assert_eq!(find_closed_search_tag(text5), None);

        // No search tag
        let text6 = "Normal conversation without any search tags.";
        assert_eq!(find_closed_search_tag(text6), None);
    }

    #[test]
    fn test_extract_search_tags() {
        let text = "Let me look that up: <search>osu! world cup 2026</search> and also <search_image>peppy osu</search_image>";
        let tags = extract_search_tags(text);
        assert_eq!(
            tags,
            vec![
                AiSearchTag::Web("osu! world cup 2026".to_string()),
                AiSearchTag::Image("peppy osu".to_string())
            ]
        );

        let audio_text = "Here is a song: <search_audio>osu theme song</search_audio>";
        let tags2 = extract_search_tags(audio_text);
        assert_eq!(tags2, vec![AiSearchTag::Audio("osu theme song".to_string())]);
    }

    #[test]
    fn test_extract_attach_tags() {
        let text = "Here is the picture: <attach>https://example.com/pic.png</attach> Enjoy!";
        let urls = extract_attach_tags(text);
        assert_eq!(urls, vec!["https://example.com/pic.png".to_string()]);
    }

    #[test]
    fn test_strip_tool_tags() {
        let text = "Answer here <search>query</search> more words <attach>http://example.com/a.jpg</attach> done.";
        let stripped = strip_tool_tags(text);
        assert_eq!(stripped, "Answer here  more words  done.");
    }

    #[test]
    fn test_ssrf_ip_rejection() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

        assert!(is_private_or_local_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_private_or_local_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))));
        assert!(is_private_or_local_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(is_private_or_local_ip(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(is_private_or_local_ip(IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))));
        assert!(is_private_or_local_ip(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))));
        assert!(is_private_or_local_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_private_or_local_ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED)));

        // Public IP should not be rejected
        assert!(!is_private_or_local_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_private_or_local_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[tokio::test]
    async fn test_validate_url_ssrf() {
        assert!(validate_url_for_ssrf("http://127.0.0.1/test").await.is_err());
        assert!(validate_url_for_ssrf("http://localhost:8080/evil").await.is_err());
        assert!(validate_url_for_ssrf("file:///etc/passwd").await.is_err());
        assert!(validate_url_for_ssrf("ftp://example.com/test").await.is_err());
    }

    #[test]
    fn test_infer_media_extension() {
        assert_eq!(infer_media_extension("image/png", &[], "/foo"), Some("png"));
        assert_eq!(infer_media_extension("image/jpeg", &[], "/foo"), Some("jpg"));
        assert_eq!(infer_media_extension("audio/mpeg", &[], "/foo"), Some("mp3"));
        assert_eq!(infer_media_extension("video/mp4", &[], "/foo"), Some("mp4"));

        // Magic bytes
        let png_bytes = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00];
        assert_eq!(infer_media_extension("", &png_bytes, "/bar"), Some("png"));

        let jpg_bytes = [0xFF, 0xD8, 0xFF, 0xE0];
        assert_eq!(infer_media_extension("", &jpg_bytes, "/bar"), Some("jpg"));
    }
}
