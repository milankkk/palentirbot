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
    Browse(String),
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
        true
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    /// Direct DuckDuckGo HTML search for fast, reliable web results without CAPTCHA.
    pub async fn search_duckduckgo(&self, query: &str) -> Result<Vec<SearxResultItem>> {
        let url = "https://html.duckduckgo.com/html/";
        let res = self
            .client
            .post(url)
            .header(reqwest::header::USER_AGENT, "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36")
            .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(reqwest::header::REFERER, "https://html.duckduckgo.com/")
            .form(&[("q", query), ("b", "")])
            .send()
            .await
            .context("failed to query DuckDuckGo")?;

        let status = res.status();
        if !status.is_success() {
            eyre::bail!("DuckDuckGo returned HTTP {status}");
        }

        let body = res.text().await.context("failed to read DuckDuckGo response")?;
        let results = parse_duckduckgo_html(&body);
        Ok(results)
    }

    /// Query web search (DuckDuckGo first, with SearXNG fallback) or SearXNG media categories (`images`, `music,files,videos`).
    pub async fn search(&self, query: &str, category: &str, safesearch: u8) -> Result<Vec<SearxResultItem>> {
        if category == "general" {
            match self.search_duckduckgo(query).await {
                Ok(results) if !results.is_empty() => return Ok(results),
                Ok(_) => {
                    tracing::debug!(query, "DuckDuckGo returned 0 results, attempting SearXNG fallback");
                }
                Err(err) => {
                    tracing::debug!(?err, query, "DuckDuckGo search error, attempting SearXNG fallback");
                }
            }
        }

        let base_url = match self.base_url.as_deref() {
            Some(u) => u,
            None => eyre::bail!("SearXNG is not configured (SEARXNG_URL is empty)"),
        };

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

/// Check if an IPv4 address is private, loopback, link-local, unspecified, broadcast, documentation, or reserved.
pub fn is_private_or_local_ip_v4(v4: std::net::Ipv4Addr) -> bool {
    let octets = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_documentation()
        // 0.0.0.0/8 (Current network)
        || octets[0] == 0
        // 100.64.0.0/10 (Carrier-Grade NAT / Shared address space)
        || (octets[0] == 100 && (octets[1] & 0xc0) == 64)
        // 192.0.0.0/24 (IETF Protocol Assignments)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        // 198.18.0.0/15 (Network benchmark testing)
        || (octets[0] == 198 && (octets[1] & 0xfe) == 18)
        // 240.0.0.0/4 (Reserved for future use)
        || (octets[0] >= 240)
}

/// Check if an IP address is private, loopback, link-local, unspecified, broadcast, documentation, or reserved.
pub fn is_private_or_local_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => is_private_or_local_ip_v4(v4),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // IPv4-mapped IPv6 (::ffff:x.x.x.x)
                || v6.to_ipv4_mapped().map(is_private_or_local_ip_v4).unwrap_or(false)
                // IPv6 Unique Local Address (ULA) fc00::/7 (fc00:: to fdff::)
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // IPv6 Link-Local Unicast fe80::/10
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv6 Documentation 2001:db8::/32
                || (v6.segments()[0] == 0x2001 && v6.segments()[1] == 0x0db8)
        }
    }
}

/// Validate a URL against SSRF attacks (rejecting non-http/https, localhost, and private/local IPs/domains).
pub async fn validate_url_for_ssrf(url_str: &str) -> Result<reqwest::Url> {
    let raw = url_str
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`' || c == '<' || c == '>')
        .trim();
    let url_to_parse = if !raw.contains("://") {
        format!("https://{raw}")
    } else {
        raw.to_string()
    };
    let parsed = reqwest::Url::parse(&url_to_parse).context("invalid URL")?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        eyre::bail!("URL scheme must be http or https");
    }

    let host_str = parsed
        .host_str()
        .ok_or_else(|| eyre::eyre!("URL has no host"))?;
    let host_lower = host_str.to_ascii_lowercase();

    // Block localhost and common local/internal domain extensions
    if host_lower == "localhost"
        || host_lower.ends_with(".localhost")
        || host_lower.ends_with(".local")
        || host_lower.ends_with(".internal")
        || host_lower.ends_with(".lan")
        || host_lower.ends_with(".home")
        || host_lower.ends_with(".localdomain")
        || host_lower.ends_with(".corp")
        || host_lower.ends_with(".onion")
    {
        eyre::bail!("Local and internal hostnames are not allowed");
    }

    if let Ok(ip) = host_lower.parse::<std::net::IpAddr>() {
        if is_private_or_local_ip(ip) {
            eyre::bail!("Private or local IP addresses are not allowed: {ip}");
        }
    } else {
        let port = parsed.port_or_known_default().unwrap_or(80);
        let addrs = tokio::net::lookup_host(format!("{host_lower}:{port}"))
            .await
            .context("failed to resolve host for security check")?;

        let mut has_addrs = false;
        for addr in addrs {
            has_addrs = true;
            if is_private_or_local_ip(addr.ip()) {
                eyre::bail!("Host resolves to a private or local IP address: {}", addr.ip());
            }
        }
        if !has_addrs {
            eyre::bail!("Host did not resolve to any IP address");
        }
    }

    Ok(parsed)
}

/// Decode common HTML entities into readable text characters.
pub fn decode_html_entities(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&#160;", " ")
        .replace("&#8211;", "–")
        .replace("&#8212;", "—")
        .replace("&#8216;", "‘")
        .replace("&#8217;", "’")
        .replace("&#8220;", "“")
        .replace("&#8221;", "”")
}

/// Strip all HTML `<...>` tags and decode HTML entities.
pub fn strip_html_tags(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut inside = false;
    for c in input.chars() {
        if c == '<' {
            inside = true;
        } else if c == '>' {
            inside = false;
        } else if !inside {
            output.push(c);
        }
    }
    decode_html_entities(&output)
}

/// Parse DuckDuckGo HTML search results into structured items.
pub fn parse_duckduckgo_html(html: &str) -> Vec<SearxResultItem> {
    let mut results = Vec::new();
    let mut remaining = html;

    while let Some(pos) = remaining.find("class=\"result__title\"") {
        let block = &remaining[pos..];
        let next_pos = block["class=\"result__title\"".len()..].find("class=\"result__title\"");
        let chunk = match next_pos {
            Some(n) => &block[..n + "class=\"result__title\"".len()],
            None => block,
        };

        let mut title = None;
        let mut url = None;
        let mut snippet = None;

        // Find <a class="result__a" href="...">Title</a>
        if let Some(a_pos) = chunk.find("class=\"result__a\"") {
            let a_start = &chunk[..a_pos];
            if let Some(tag_open) = a_start.rfind("<a ") {
                let a_tag_full = &chunk[tag_open..];
                if let Some(href_pos) = a_tag_full.find("href=\"") {
                    let href_start = &a_tag_full[href_pos + 6..];
                    if let Some(href_end) = href_start.find('"') {
                        let extracted_url = &href_start[..href_end];
                        if extracted_url.starts_with("http") {
                            url = Some(decode_html_entities(extracted_url));
                        }
                    }
                }
                if let Some(content_start) = a_tag_full.find('>') {
                    let tag_inner = &a_tag_full[content_start + 1..];
                    if let Some(content_end) = tag_inner.find("</a>") {
                        let raw_title = &tag_inner[..content_end];
                        let clean = strip_html_tags(raw_title).trim().to_string();
                        if !clean.is_empty() {
                            title = Some(clean);
                        }
                    }
                }
            }
        }

        // Find <a class="result__snippet" ...>Snippet</a>
        if let Some(s_pos) = chunk.find("class=\"result__snippet\"") {
            let s_tag = &chunk[s_pos..];
            if let Some(content_start) = s_tag.find('>') {
                let tag_inner = &s_tag[content_start + 1..];
                if let Some(content_end) = tag_inner.find("</a>") {
                    let raw_snippet = &tag_inner[..content_end];
                    let clean = strip_html_tags(raw_snippet).trim().to_string();
                    if !clean.is_empty() {
                        snippet = Some(clean);
                    }
                }
            }
        }

        if let (Some(u), Some(t)) = (url, title) {
            results.push(SearxResultItem {
                title: Some(t),
                url: Some(u),
                content: snippet,
                ..Default::default()
            });
        }

        remaining = match next_pos {
            Some(n) => &block[n + "class=\"result__title\"".len()..],
            None => break,
        };
    }

    results
}

/// Fetch a webpage and extract clean, readable text (up to 3,500 characters).
pub async fn fetch_webpage_clean_text(client: &Client, url_str: &str) -> Result<String> {
    let validated = validate_url_for_ssrf(url_str).await?;

    let res = client
        .get(validated.as_str())
        .header(reqwest::header::USER_AGENT, "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36")
        .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.8")
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("failed to request webpage")?;

    let status = res.status();
    if !status.is_success() {
        eyre::bail!("Webpage returned HTTP {status}");
    }

    let bytes = res.bytes().await.context("failed to read webpage bytes")?;
    let raw_html = String::from_utf8_lossy(&bytes[..bytes.len().min(5 * 1024 * 1024)]);

    let clean = extract_article_text(&raw_html);
    if clean.is_empty() {
        eyre::bail!("No readable text found on webpage");
    }

    let truncated: String = clean.chars().take(3500).collect();
    Ok(truncated)
}

/// Clean HTML markup into human-readable article text.
pub fn extract_article_text(html: &str) -> String {
    let mut text = html.to_string();

    // Strip scripts, styles, noscript, nav, footer, header
    for tag in &["script", "style", "noscript", "nav", "footer", "header"] {
        let open_pattern = format!("<{tag}");
        let close_pattern = format!("</{tag}>");
        while let Some(start) = text.to_ascii_lowercase().find(&open_pattern) {
            if let Some(end) = text[start..].to_ascii_lowercase().find(&close_pattern) {
                let end_idx = start + end + close_pattern.len();
                text.drain(start..end_idx);
            } else {
                break;
            }
        }
    }

    // Replace line-break tags with newlines
    let mut with_breaks = String::with_capacity(text.len());
    let mut remaining = text.as_str();

    while let Some(open) = remaining.find('<') {
        with_breaks.push_str(&remaining[..open]);
        let after_open = &remaining[open..];
        if let Some(close) = after_open.find('>') {
            let tag_content = &after_open[1..close].to_ascii_lowercase();
            if tag_content.starts_with("br")
                || tag_content.starts_with("/p")
                || tag_content.starts_with("/div")
                || tag_content.starts_with("/li")
                || tag_content.starts_with("/h1")
                || tag_content.starts_with("/h2")
                || tag_content.starts_with("/h3")
                || tag_content.starts_with("/h4")
                || tag_content.starts_with("p")
                || tag_content.starts_with("div")
            {
                with_breaks.push('\n');
            }
            remaining = &after_open[close + 1..];
        } else {
            break;
        }
    }
    with_breaks.push_str(remaining);

    // Decode entities and collapse lines
    let decoded = decode_html_entities(&with_breaks);
    let mut lines = Vec::new();
    for line in decoded.lines() {
        let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.len() > 1 {
            lines.push(collapsed);
        }
    }

    lines.join("\n")
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

/// Find the first closed search tag (`<search>...</search>`, `<browse>...</browse>`,
/// `<search_image>...</search_image>`, or `<search_audio>...</search_audio>`) in text.
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
        } else if let Some(rest) = after_open.strip_prefix("<browse>").or_else(|| after_open.strip_prefix("<fetch>")) {
            if let Some(close_idx) = rest.find("</browse>").or_else(|| rest.find("</fetch>")).or_else(|| rest.find("</search>")) {
                let url = rest[..close_idx]
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'' || c == '`' || c == '<' || c == '>')
                    .trim()
                    .to_string();
                if !url.is_empty() {
                    return Some(AiSearchTag::Browse(url));
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

/// Extract `<search*>`, `<browse>`, and `<attach>` tags.
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
        } else if let Some(rest) = after_open.strip_prefix("<browse>").or_else(|| after_open.strip_prefix("<fetch>")) {
            let close_tag = if rest.contains("</browse>") { "</browse>" } else if rest.contains("</fetch>") { "</fetch>" } else { "</search>" };
            if let Some(close_idx) = rest.find(close_tag) {
                let url = rest[..close_idx]
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'' || c == '`' || c == '<' || c == '>')
                    .trim()
                    .to_string();
                if !url.is_empty() {
                    tags.push(AiSearchTag::Browse(url));
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
        "browse",
        "fetch",
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
        // CGNAT (100.64.0.0/10)
        assert!(is_private_or_local_ip(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_private_or_local_ip(IpAddr::V4(Ipv4Addr::new(100, 127, 255, 254))));
        // IETF protocol assignments (192.0.0.0/24)
        assert!(is_private_or_local_ip(IpAddr::V4(Ipv4Addr::new(192, 0, 0, 1))));
        // Network benchmark (198.18.0.0/15)
        assert!(is_private_or_local_ip(IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1))));
        assert!(is_private_or_local_ip(IpAddr::V4(Ipv4Addr::new(198, 19, 255, 254))));
        // Reserved (240.0.0.0/4)
        assert!(is_private_or_local_ip(IpAddr::V4(Ipv4Addr::new(240, 0, 0, 1))));

        assert!(is_private_or_local_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_private_or_local_ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED)));

        // IPv6 ULA (fc00::/7)
        let ula: Ipv6Addr = "fc00::1".parse().unwrap();
        assert!(is_private_or_local_ip(IpAddr::V6(ula)));
        let ula2: Ipv6Addr = "fd12:3456:789a::1".parse().unwrap();
        assert!(is_private_or_local_ip(IpAddr::V6(ula2)));

        // IPv6 Link-Local (fe80::/10)
        let link_local: Ipv6Addr = "fe80::1".parse().unwrap();
        assert!(is_private_or_local_ip(IpAddr::V6(link_local)));

        // IPv6 Documentation (2001:db8::/32)
        let doc: Ipv6Addr = "2001:db8::1".parse().unwrap();
        assert!(is_private_or_local_ip(IpAddr::V6(doc)));

        // IPv4-mapped IPv6
        let mapped_loopback: Ipv6Addr = "::ffff:127.0.0.1".parse().unwrap();
        assert!(is_private_or_local_ip(IpAddr::V6(mapped_loopback)));
        let mapped_private: Ipv6Addr = "::ffff:192.168.1.1".parse().unwrap();
        assert!(is_private_or_local_ip(IpAddr::V6(mapped_private)));
        let mapped_public: Ipv6Addr = "::ffff:8.8.8.8".parse().unwrap();
        assert!(!is_private_or_local_ip(IpAddr::V6(mapped_public)));

        // Public IP should not be rejected
        assert!(!is_private_or_local_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_private_or_local_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        let public_v6: Ipv6Addr = "2606:4700:4700::1111".parse().unwrap();
        assert!(!is_private_or_local_ip(IpAddr::V6(public_v6)));
    }

    #[tokio::test]
    async fn test_validate_url_ssrf() {
        assert!(validate_url_for_ssrf("http://127.0.0.1/test").await.is_err());
        assert!(validate_url_for_ssrf("http://localhost:8080/evil").await.is_err());
        assert!(validate_url_for_ssrf("http://router.local/admin").await.is_err());
        assert!(validate_url_for_ssrf("http://internal.lan/").await.is_err());
        assert!(validate_url_for_ssrf("http://service.internal/").await.is_err());
        assert!(validate_url_for_ssrf("http://myhost.home/").await.is_err());
        assert!(validate_url_for_ssrf("http://server.localdomain/").await.is_err());
        assert!(validate_url_for_ssrf("http://100.64.0.1:8080/").await.is_err());
        assert!(validate_url_for_ssrf("http://[fe80::1]/").await.is_err());
        assert!(validate_url_for_ssrf("http://[fc00::1]/").await.is_err());
        assert!(validate_url_for_ssrf("http://[::ffff:127.0.0.1]/").await.is_err());
        assert!(validate_url_for_ssrf("file:///etc/passwd").await.is_err());
        assert!(validate_url_for_ssrf("ftp://example.com/test").await.is_err());
        assert!(validate_url_for_ssrf("duckduckgo.com").await.is_ok());
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

    #[test]
    fn test_parse_duckduckgo_html() {
        let sample_html = r#"
            <div class="result results_links results_links_deep web-result ">
              <div class="links_main links_deep result__body">
                <h2 class="result__title">
                  <a rel="nofollow" class="result__a" href="https://osu.ppy.sh/home/download">download | osu!</a>
                </h2>
                <a class="result__snippet" href="https://osu.ppy.sh/home/download">Rhythm is just a *click* away! Taiko and original modes.</a>
              </div>
            </div>
        "#;
        let results = parse_duckduckgo_html(sample_html);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title.as_deref(), Some("download | osu!"));
        assert_eq!(results[0].url.as_deref(), Some("https://osu.ppy.sh/home/download"));
        assert!(results[0].content.as_deref().unwrap().contains("Rhythm is just a"));
    }

    #[test]
    fn test_extract_article_text() {
        let html = r#"
            <html>
                <head><script>alert('bad');</script><style>body { color: red; }</style></head>
                <body>
                    <header><nav>Home | About</nav></header>
                    <h1>Article Title</h1>
                    <p>First paragraph with <b>bold</b> text &amp; info.</p>
                    <p>Second paragraph.</p>
                    <footer>Copyright 2026</footer>
                </body>
            </html>
        "#;
        let text = extract_article_text(html);
        assert!(!text.contains("alert"));
        assert!(!text.contains("body {"));
        assert!(!text.contains("Home | About"));
        assert!(!text.contains("Copyright 2026"));
        assert!(text.contains("Article Title"));
        assert!(text.contains("First paragraph with bold text & info."));
        assert!(text.contains("Second paragraph."));
    }

    #[test]
    fn test_find_closed_browse_tag() {
        let text = "I should check the website <browse>https://example.com/info</browse> let me see";
        assert_eq!(
            find_closed_search_tag(text),
            Some(AiSearchTag::Browse("https://example.com/info".to_string()))
        );
    }
}
