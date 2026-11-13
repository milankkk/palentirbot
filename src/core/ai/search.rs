use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use eyre::{Context as _, Result};
use reqwest::Client;
use serde::Deserialize;

#[derive(Deserialize, Debug, Clone, Default)]
pub struct SearchResultItem {
    pub title: Option<String>,
    pub url: Option<String>,
    pub content: Option<String>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct ImageSearchItem {
    pub title: String,
    pub image_url: String,
    pub source_url: String,
}

#[derive(Debug, Clone)]
pub struct VerifiedImageResult {
    pub title: String,
    pub image_url: String,
    pub source_url: String,
    pub filename: String,
    pub bytes: Vec<u8>,
}

#[derive(Deserialize, Debug, Clone, Default)]
struct SearxResponse {
    #[serde(default)]
    pub results: Vec<SearchResultItem>,
}

#[derive(Deserialize, Debug, Clone, Default)]
struct SearxImageItem {
    pub title: Option<String>,
    pub url: Option<String>,
    pub img_src: Option<String>,
}

#[derive(Deserialize, Debug, Clone, Default)]
struct SearxImageResponse {
    #[serde(default)]
    pub results: Vec<SearxImageItem>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum AiSearchAction {
    Search(String),
    Browse(String),
}

const MAX_BROWSE_BODY_BYTES: usize = 256 * 1024;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36";
const WIKIMEDIA_USER_AGENT: &str = "PalentirBot/1.0 (Discord assistant image lookup)";

pub struct WebSearchClient {
    client: Client,
    searxng_url: Option<String>,
}

impl Default for WebSearchClient {
    fn default() -> Self {
        Self::new()
    }
}

impl WebSearchClient {
    pub fn new() -> Self {
        let searxng_url = std::env::var("SEARXNG_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_owned())
            .filter(|s| !s.is_empty());

        let client = Client::builder()
            .timeout(Duration::from_secs(8))
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build WebSearchClient HTTP client");

        Self {
            client,
            searxng_url,
        }
    }

    /// Search the web using SearXNG (if configured via `SEARXNG_URL`) or direct DuckDuckGo HTML search.
    pub async fn search(&self, query: &str) -> Result<Vec<SearchResultItem>> {
        let clean_query = query.trim();
        if clean_query.is_empty() {
            return Ok(Vec::new());
        }

        if let Some(base_url) = self.searxng_url.as_deref() {
            let url = format!("{base_url}/search");
            if let Ok(resp) = self
                .client
                .get(&url)
                .query(&[
                    ("q", clean_query),
                    ("categories", "general"),
                    ("format", "json"),
                    ("safesearch", "0"),
                ])
                .send()
                .await
            {
                if resp.status().is_success() {
                    if let Ok(parsed) = resp.json::<SearxResponse>().await {
                        if !parsed.results.is_empty() {
                            return Ok(parsed.results);
                        }
                    }
                }
            }
        }

        self.search_duckduckgo(clean_query).await
    }

    /// Direct DuckDuckGo HTML search (no API key or external container required).
    pub async fn search_duckduckgo(&self, query: &str) -> Result<Vec<SearchResultItem>> {
        let url = "https://html.duckduckgo.com/html/";
        let res = self
            .client
            .post(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(reqwest::header::REFERER, "https://html.duckduckgo.com/")
            .form(&[("q", query), ("kl", "wt-wt"), ("kp", "-2")])
            .send()
            .await
            .context("failed to query DuckDuckGo")?;

        let status = res.status();
        if status == reqwest::StatusCode::ACCEPTED || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            eyre::bail!("DuckDuckGo rate-limited / challenged automated search (HTTP {status})");
        }
        if !status.is_success() {
            eyre::bail!("DuckDuckGo returned HTTP {status}");
        }

        let body = res
            .text()
            .await
            .context("failed to read DuckDuckGo response")?;

        let results = parse_duckduckgo_html(&body);
        if results.is_empty() && is_duckduckgo_challenge_html(&body) {
            eyre::bail!("DuckDuckGo CAPTCHA/anomaly challenge prevented search results");
        }

        Ok(results)
    }

    /// Fetch raw bytes from a URL using up to 3 SSRF-validated redirects and DNS-pinned lookup.
    async fn fetch_ssrf_safe_bytes(
        &self,
        raw_url: &str,
        accept: &str,
        max_bytes: usize,
    ) -> Result<(reqwest::Url, Option<String>, Vec<u8>)> {
        let mut current = validate_and_resolve_url(raw_url).await?;

        for _ in 0..4 {
            let client = build_pinned_client(&current.host, &current.pinned_addrs)?;
            let mut res = client
                .get(current.url.clone())
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .header(reqwest::header::ACCEPT, accept)
                .send()
                .await
                .context("failed to fetch remote URL")?;

            if res.status().is_redirection() {
                if let Some(loc) = res.headers().get(reqwest::header::LOCATION) {
                    if let Ok(loc_str) = loc.to_str() {
                        let next_url = current
                            .url
                            .join(loc_str)
                            .context("invalid redirect Location")?;
                        current = validate_and_resolve_url(next_url.as_str()).await?;
                        continue;
                    }
                }
                eyre::bail!("redirect without valid Location header");
            }

            if !res.status().is_success() {
                eyre::bail!("remote server returned HTTP {}", res.status());
            }

            if let Some(len) = res.content_length() {
                if len as usize > max_bytes {
                    eyre::bail!("remote file is too large ({len} bytes > {max_bytes} byte limit)");
                }
            }

            let content_type = res
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_ascii_lowercase());

            let mut buf = Vec::new();
            while let Some(chunk) = res.chunk().await.context("failed to read response chunk")? {
                if buf.len().saturating_add(chunk.len()) > max_bytes {
                    if accept.starts_with("image/") {
                        eyre::bail!("image exceeds {max_bytes} byte limit");
                    }
                    let remaining = max_bytes.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..remaining]);
                    break;
                }
                buf.extend_from_slice(&chunk);
            }

            return Ok((current.url, content_type, buf));
        }

        eyre::bail!("too many redirects")
    }

    /// Fetch and extract readable plain text (plus notable page images) from a URL safely.
    pub async fn browse_url(&self, raw_url: &str, max_chars: usize) -> Result<String> {
        let (final_url, _, buf) = self
            .fetch_ssrf_safe_bytes(
                raw_url,
                "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5",
                MAX_BROWSE_BODY_BYTES,
            )
            .await?;

        let html = String::from_utf8_lossy(&buf);
        let extracted = extract_article_text(&html);
        let mut out: String = extracted.chars().take(max_chars).collect();
        let page_images = extract_page_image_urls(&html, &final_url);
        if !page_images.is_empty() {
            out.push_str("\n\n[Page images found:\n");
            for img in page_images {
                out.push_str("- ");
                out.push_str(&img);
                out.push('\n');
            }
            out.push(']');
        }
        Ok(out)
    }

    /// Download a remote PNG/JPEG/GIF/WebP image with SSRF protection and magic-byte verification.
    pub async fn download_image(&self, raw_url: &str, max_bytes: usize) -> Result<(String, Vec<u8>)> {
        let (final_url, content_type, bytes) = self
            .fetch_ssrf_safe_bytes(
                raw_url,
                "image/png,image/jpeg,image/webp,image/gif,image/*;q=0.8,*/*;q=0.1",
                max_bytes,
            )
            .await?;

        if let Some(ref ct) = content_type {
            let main_type = ct.split(';').next().unwrap_or("").trim();
            if main_type.starts_with("text/")
                || main_type.contains("json")
                || main_type.contains("svg")
            {
                eyre::bail!("URL returned `{main_type}` instead of a raster image");
            }
        }

        let Some(ext) = detect_image_extension(&bytes) else {
            eyre::bail!("URL bytes are not a valid PNG, JPEG, GIF, or WebP image");
        };

        let filename = derive_image_filename(&final_url, ext);
        Ok((filename, bytes))
    }

    /// Search the web for relevant images using SearXNG (if configured), Bing Images, and Wikimedia Commons.
    pub async fn search_images(&self, query: &str) -> Result<Vec<ImageSearchItem>> {
        let clean_query = query.trim();
        if clean_query.is_empty() {
            return Ok(Vec::new());
        }

        let mut combined = Vec::new();
        let mut seen = HashSet::new();

        if let Some(base_url) = self.searxng_url.as_deref() {
            let url = format!("{base_url}/search");
            if let Ok(resp) = self
                .client
                .get(&url)
                .query(&[
                    ("q", clean_query),
                    ("categories", "images"),
                    ("format", "json"),
                    ("safesearch", "1"),
                ])
                .send()
                .await
            {
                if resp.status().is_success() {
                    if let Ok(parsed) = resp.json::<SearxImageResponse>().await {
                        for r in parsed.results {
                            if let Some(img) = r.img_src.filter(|u| is_usable_candidate_image_url(u))
                            {
                                if seen.insert(img.clone()) {
                                    combined.push(ImageSearchItem {
                                        title: r.title.unwrap_or_else(|| "Image".to_owned()),
                                        image_url: img,
                                        source_url: r.url.unwrap_or_default(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Ok(bing) = self.search_bing_images(clean_query).await {
            for item in bing {
                if seen.insert(item.image_url.clone()) {
                    combined.push(item);
                }
            }
        }

        if combined.len() < 4 {
            if let Ok(wiki) = self.search_wikimedia_images(clean_query).await {
                for item in wiki {
                    if seen.insert(item.image_url.clone()) {
                        combined.push(item);
                    }
                }
            } else if combined.is_empty() {
                return self.search_wikimedia_images(clean_query).await;
            }
        }

        combined.truncate(10);
        Ok(combined)
    }

    /// Search for images and download-verify candidates (HTTP 200 + raster magic bytes + >= 1 KB)
    /// so broken/hotlink-blocked URLs are discarded before the model sees them.
    pub async fn search_verified_images(
        &self,
        query: &str,
        max_bytes: usize,
        max_verified: usize,
    ) -> Result<Vec<VerifiedImageResult>> {
        let candidates = self.search_images(query).await?;
        let mut verified = Vec::new();
        for item in candidates.into_iter().take(8) {
            match self.download_image(&item.image_url, max_bytes).await {
                Ok((filename, bytes)) if bytes.len() >= 1024 => {
                    verified.push(VerifiedImageResult {
                        title: item.title,
                        image_url: item.image_url,
                        source_url: item.source_url,
                        filename,
                        bytes,
                    });
                    if verified.len() >= max_verified {
                        break;
                    }
                }
                _ => continue,
            }
        }
        Ok(verified)
    }

    async fn search_bing_images(&self, query: &str) -> Result<Vec<ImageSearchItem>> {
        let res = self
            .client
            .get("https://www.bing.com/images/search")
            .query(&[("q", query), ("form", "HDRSC3"), ("adlt", "moderate")])
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .send()
            .await
            .context("failed to query Bing Images")?;

        if !res.status().is_success() {
            eyre::bail!("Bing Images returned HTTP {}", res.status());
        }

        let body = res.text().await.context("failed to read Bing Images HTML")?;
        Ok(parse_bing_image_results(&body))
    }

    async fn search_wikimedia_images(&self, query: &str) -> Result<Vec<ImageSearchItem>> {
        #[derive(Deserialize, Default)]
        struct WikiResp {
            query: Option<WikiQuery>,
        }
        #[derive(Deserialize, Default)]
        struct WikiQuery {
            #[serde(default)]
            pages: std::collections::HashMap<String, WikiPage>,
        }
        #[derive(Deserialize, Default)]
        struct WikiPage {
            title: Option<String>,
            #[serde(default)]
            imageinfo: Vec<WikiImageInfo>,
        }
        #[derive(Deserialize, Default)]
        struct WikiImageInfo {
            url: Option<String>,
            thumburl: Option<String>,
            mime: Option<String>,
        }

        let res = self
            .client
            .get("https://commons.wikimedia.org/w/api.php")
            .query(&[
                ("action", "query"),
                ("generator", "search"),
                ("gsrsearch", query),
                ("gsrnamespace", "6"),
                ("gsrlimit", "6"),
                ("prop", "imageinfo"),
                ("iiprop", "url|mime|size"),
                ("iiurlwidth", "1200"),
                ("format", "json"),
            ])
            .header(reqwest::header::USER_AGENT, WIKIMEDIA_USER_AGENT)
            .send()
            .await
            .context("failed to query Wikimedia Commons")?;

        if !res.status().is_success() {
            eyre::bail!("Wikimedia Commons returned HTTP {}", res.status());
        }

        let parsed: WikiResp = res
            .json()
            .await
            .context("failed to parse Wikimedia Commons response")?;

        let mut items = Vec::new();
        if let Some(q) = parsed.query {
            for page in q.pages.into_values() {
                let Some(info) = page.imageinfo.into_iter().next() else {
                    continue;
                };
                let mime = info.mime.as_deref().unwrap_or("");
                if !matches!(mime, "image/png" | "image/jpeg" | "image/webp" | "image/gif") {
                    continue;
                }
                let Some(img_url) = info.thumburl.or(info.url).filter(|u| is_http_url(u)) else {
                    continue;
                };
                let title = page
                    .title
                    .unwrap_or_else(|| "Wikimedia image".to_owned())
                    .trim_start_matches("File:")
                    .to_owned();
                items.push(ImageSearchItem {
                    title,
                    image_url: img_url,
                    source_url: "https://commons.wikimedia.org".to_owned(),
                });
                if items.len() >= 6 {
                    break;
                }
            }
        }
        Ok(items)
    }
}

fn is_http_url(raw: &str) -> bool {
    raw.starts_with("https://") || raw.starts_with("http://")
}

fn is_usable_candidate_image_url(raw: &str) -> bool {
    if !is_http_url(raw) {
        return false;
    }
    let lower = raw.to_ascii_lowercase();
    !lower.contains(".svg")
        && !lower.contains("tiktok.com/api/img")
        && !lower.contains("lookaside.fbsbx.com")
        && !lower.contains("bing.com/th/id/ogc")
        && !lower.contains("1x1")
        && !lower.contains("pixel.")
}

pub fn detect_image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

pub fn derive_image_filename(url: &reqwest::Url, ext: &str) -> String {
    let raw_stem = url
        .path_segments()
        .and_then(|mut s| s.rfind(|seg| !seg.is_empty()))
        .unwrap_or("image");
    let stem = raw_stem.rsplit_once('.').map_or(raw_stem, |(base, _)| base);
    let mut clean: String = stem
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .take(40)
        .collect();
    if clean.is_empty() {
        clean = "image".to_owned();
    }
    format!("{clean}.{ext}")
}

fn take_bounded_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub fn parse_bing_image_results(html: &str) -> Vec<ImageSearchItem> {
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut remaining = html;

    while let Some(pos) = remaining.find("murl&quot;:&quot;") {
        let after = &remaining[pos + "murl&quot;:&quot;".len()..];
        let Some(end) = after.find("&quot;") else {
            break;
        };
        let raw_url = decode_html_entities(&after[..end]);
        let window = take_bounded_utf8(&remaining[pos..], 600);
        let title = window
            .split("&quot;t&quot;:&quot;")
            .nth(1)
            .and_then(|s| s.split("&quot;").next())
            .map(decode_html_entities)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Image".to_owned());

        if is_usable_candidate_image_url(&raw_url) && seen.insert(raw_url.clone()) {
            items.push(ImageSearchItem {
                title,
                image_url: raw_url,
                source_url: String::new(),
            });
            if items.len() >= 10 {
                break;
            }
        }
        remaining = &after[end..];
    }

    items
}

pub fn extract_page_image_urls(html: &str, base_url: &reqwest::Url) -> Vec<String> {
    let mut urls = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let lower = html.to_ascii_lowercase();

    for marker in ["og:image", "twitter:image"] {
        if let Some(pos) = lower.find(marker) {
            let window = take_bounded_utf8(&html[pos..], 400);
            let window_lower = take_bounded_utf8(&lower[pos..], 400);
            if let Some(cpos) = window_lower.find("content=") {
                if let Some(extracted) = extract_quoted_attr(&window[cpos + "content=".len()..]) {
                    if let Some(resolved) = normalize_candidate_image_url(&extracted, base_url) {
                        if seen.insert(resolved.clone()) {
                            urls.push(resolved);
                        }
                    }
                }
            }
        }
    }

    let mut offset = 0usize;
    while urls.len() < 3 {
        let Some(img_rel) = lower[offset..].find("<img") else {
            break;
        };
        let tag_start = offset + img_rel;
        let Some(tag_end_rel) = html[tag_start..].find('>') else {
            break;
        };
        let tag = &html[tag_start..tag_start + tag_end_rel + 1];
        let tag_lower = &lower[tag_start..tag_start + tag_end_rel + 1];
        if let Some(src_pos) = tag_lower.find("src=") {
            if let Some(raw_src) = extract_quoted_attr(&tag[src_pos + 4..]) {
                if let Some(resolved) = normalize_candidate_image_url(&raw_src, base_url) {
                    if seen.insert(resolved.clone()) {
                        urls.push(resolved);
                    }
                }
            }
        }
        offset = tag_start + tag_end_rel + 1;
    }

    urls.truncate(3);
    urls
}

fn extract_quoted_attr(after_eq: &str) -> Option<String> {
    let trimmed = after_eq.trim_start();
    let quote = trimmed.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let inner = &trimmed[quote.len_utf8()..];
    let end = inner.find(quote)?;
    Some(decode_html_entities(&inner[..end]))
}

fn normalize_candidate_image_url(raw: &str, base_url: &reqwest::Url) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with("data:") {
        return None;
    }
    let resolved = base_url.join(trimmed).ok()?;
    if resolved.scheme() != "http" && resolved.scheme() != "https" {
        return None;
    }
    let lower = resolved.as_str().to_ascii_lowercase();
    if lower.contains(".svg")
        || lower.contains("favicon")
        || lower.contains("sprite")
        || lower.contains("1x1")
        || lower.contains("pixel.")
    {
        return None;
    }
    Some(resolved.into())
}

fn is_duckduckgo_challenge_html(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("anomaly-modal")
        || lower.contains("challenge-form")
        || lower.contains("cc=botnet")
        || lower.contains("bots use duckduckgo")
}

pub struct ValidatedTarget {
    pub url: reqwest::Url,
    pub host: String,
    pub pinned_addrs: Vec<SocketAddr>,
}

fn build_pinned_client(host: &str, addrs: &[SocketAddr]) -> Result<Client> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(8))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none());

    if !addrs.is_empty() && host.parse::<IpAddr>().is_err() {
        builder = builder.resolve_to_addrs(host, addrs);
    }

    builder
        .build()
        .context("failed to build SSRF-pinned HTTP client")
}

/// Format search results into a compact text block for the LLM tool turn.
pub fn format_search_results(query: &str, results: &[SearchResultItem], max_items: usize) -> String {
    if results.is_empty() {
        return format!("Search query `{query}` returned no results.\n");
    }

    let mut out = format!("Web search results for `{query}`:\n");
    for (idx, item) in results.iter().take(max_items).enumerate() {
        let title = item.title.as_deref().unwrap_or("Untitled").trim();
        let url = item.url.as_deref().unwrap_or("").trim();
        let snippet: String = item
            .content
            .as_deref()
            .unwrap_or("")
            .trim()
            .chars()
            .take(280)
            .collect();
        out.push_str(&format!("{}. {} ({})\n   {}\n", idx + 1, title, url, snippet));
    }
    out
}

/// Extract up to `max_actions` `<search>...</search>` or `<browse>...</browse>` tags from text.
pub fn extract_search_actions(text: &str, max_actions: usize) -> Vec<AiSearchAction> {
    let mut actions = Vec::new();
    let mut remaining = text;

    while actions.len() < max_actions {
        let next_search = remaining.find("<search>");
        let next_browse = remaining.find("<browse>");

        let take_search = match (next_search, next_browse) {
            (Some(s), Some(b)) => s <= b,
            (Some(_), None) => true,
            _ => false,
        };

        if take_search {
            let s = next_search.expect("search index must exist");
            let after = &remaining[s + "<search>".len()..];
            if let Some(end) = after.find("</search>") {
                let query = after[..end].trim();
                if !query.is_empty() && query.len() <= 200 {
                    let act = AiSearchAction::Search(query.to_owned());
                    if !actions.contains(&act) {
                        actions.push(act);
                    }
                }
                remaining = &after[end + "</search>".len()..];
            } else {
                break;
            }
        } else if let Some(b) = next_browse {
            let after = &remaining[b + "<browse>".len()..];
            if let Some(end) = after.find("</browse>") {
                let url = after[..end].trim();
                if url.starts_with("http://") || url.starts_with("https://") {
                    let act = AiSearchAction::Browse(url.to_owned());
                    if !actions.contains(&act) {
                        actions.push(act);
                    }
                }
                remaining = &after[end + "</browse>".len()..];
            } else {
                break;
            }
        } else {
            break;
        }
    }

    actions
}

/// Extract up to `max_calls` `<tool_call>{"name": ..., "arguments": {...}}</tool_call>` blocks
/// emitted as literal text when the model runtime fails to parse special tool tokens.
pub fn extract_inline_tool_calls(
    text: &str,
    max_calls: usize,
) -> Vec<(String, serde_json::Value)> {
    let mut calls = Vec::new();
    let mut remaining = text;

    while calls.len() < max_calls {
        let Some(open_idx) = remaining.find("<tool_call>") else {
            break;
        };
        let after_open = &remaining[open_idx + "<tool_call>".len()..];
        let Some(close_idx) = after_open.find("</tool_call>") else {
            break;
        };
        let body = after_open[..close_idx].trim();
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
            if let Some(name) = val.get("name").and_then(|v| v.as_str()) {
                let args = val
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                calls.push((name.trim().to_owned(), args));
            }
        }
        remaining = &after_open[close_idx + "</tool_call>".len()..];
    }

    calls
}

/// Extract the first `http://` or `https://` URL from a user's question, if present.
pub fn extract_first_url(text: &str) -> Option<String> {
    for token in text.split_whitespace() {
        let trimmed = token.trim_matches(|c: char| "<>\"'()[].,;!".contains(c));
        if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
            return Some(trimmed.to_owned());
        }
    }
    None
}

pub fn is_private_or_local_ip_v4(v4: std::net::Ipv4Addr) -> bool {
    let octets = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_multicast()
        || octets[0] == 0
        || (octets[0] == 100 && (octets[1] & 0xc0) == 64)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 198 && (octets[1] & 0xfe) == 18)
        || octets[0] >= 240
}

pub fn is_private_or_local_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => is_private_or_local_ip_v4(v4),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.to_ipv4_mapped().is_some_and(is_private_or_local_ip_v4)
                || v6.to_ipv4().is_some_and(is_private_or_local_ip_v4)
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || (v6.segments()[0] == 0x2001 && v6.segments()[1] == 0x0db8)
        }
    }
}

pub async fn validate_and_resolve_url(url_str: &str) -> Result<ValidatedTarget> {
    let raw = url_str
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '<' | '>'))
        .trim();
    let parsed = reqwest::Url::parse(raw).context("invalid URL")?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        eyre::bail!("URL scheme must be http or https");
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| eyre::eyre!("URL has no host"))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let host_lower = host.to_ascii_lowercase();

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

    let port = parsed.port_or_known_default().unwrap_or(80);
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_or_local_ip(ip) {
            eyre::bail!("Private or local IP addresses are not allowed: {ip}");
        }
        return Ok(ValidatedTarget {
            url: parsed,
            host,
            pinned_addrs: vec![SocketAddr::new(ip, port)],
        });
    }

    let addrs = tokio::net::lookup_host((host.as_str(), port))
        .await
        .context("failed to resolve host for security check")?;

    let mut pinned_addrs = Vec::new();
    for addr in addrs {
        if is_private_or_local_ip(addr.ip()) {
            eyre::bail!(
                "Host resolves to a private or local IP address: {}",
                addr.ip()
            );
        }
        pinned_addrs.push(addr);
    }
    if pinned_addrs.is_empty() {
        eyre::bail!("Host did not resolve to any IP address");
    }

    Ok(ValidatedTarget {
        url: parsed,
        host,
        pinned_addrs,
    })
}

#[cfg(test)]
pub async fn validate_url_for_ssrf(url_str: &str) -> Result<reqwest::Url> {
    Ok(validate_and_resolve_url(url_str).await?.url)
}

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

pub fn parse_duckduckgo_html(html: &str) -> Vec<SearchResultItem> {
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

        if let Some(a_pos) = chunk.find("class=\"result__a\"") {
            let a_start = &chunk[..a_pos];
            if let Some(tag_open) = a_start.rfind("<a ") {
                let a_tag_full = &chunk[tag_open..];
                if let Some(href_pos) = a_tag_full.find("href=\"") {
                    let href_start = &a_tag_full[href_pos + 6..];
                    if let Some(href_end) = href_start.find('"') {
                        let extracted_url = &href_start[..href_end];
                        let raw_decoded = decode_html_entities(extracted_url);
                        let full_url = if raw_decoded.starts_with("//") {
                            format!("https:{raw_decoded}")
                        } else {
                            raw_decoded
                        };

                        let resolved = if let Ok(parsed_url) = reqwest::Url::parse(&full_url) {
                            if let Some((_, dest)) =
                                parsed_url.query_pairs().find(|(k, _)| k == "uddg")
                            {
                                if dest.starts_with("http") {
                                    dest.into_owned()
                                } else {
                                    full_url
                                }
                            } else {
                                full_url
                            }
                        } else {
                            full_url
                        };

                        if resolved.starts_with("http") {
                            url = Some(resolved);
                        }
                    }
                }
                if let Some(content_start) = a_tag_full.find('>') {
                    let tag_inner = &a_tag_full[content_start + 1..];
                    if let Some(content_end) = tag_inner.find("</a>") {
                        let clean = strip_html_tags(&tag_inner[..content_end]).trim().to_string();
                        if !clean.is_empty() {
                            title = Some(clean);
                        }
                    }
                }
            }
        }

        if let Some(snip_pos) = chunk.find("class=\"result__snippet\"") {
            let snip_after = &chunk[snip_pos..];
            if let Some(content_start) = snip_after.find('>') {
                let snip_inner = &snip_after[content_start + 1..];
                let end_pos = snip_inner.find("</a>").or_else(|| snip_inner.find("</div>"));
                if let Some(content_end) = end_pos {
                    let clean = strip_html_tags(&snip_inner[..content_end]).trim().to_string();
                    if !clean.is_empty() {
                        snippet = Some(clean);
                    }
                }
            }
        }

        if title.is_some() || snippet.is_some() {
            results.push(SearchResultItem {
                title,
                url,
                content: snippet,
            });
        }

        match next_pos {
            Some(n) => remaining = &block[n + "class=\"result__title\"".len()..],
            None => break,
        }
    }

    results
}

const SKIP_TAGS: &[&str] = &["script", "style", "noscript", "svg", "header", "footer", "nav"];

pub fn extract_article_text(html: &str) -> String {
    // Strip excluded elements linearly while preserving unrelated tags like <navigation>.
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut cleaned = String::with_capacity(html.len());
    let mut i = 0usize;

    while i < html.len() {
        if bytes[i] == b'<' {
            let rest = &lower[i + 1..];
            let mut skipped_tag = false;
            for tag in SKIP_TAGS {
                if let Some(after_name) = rest.strip_prefix(tag) {
                    let boundary_ok = after_name.is_empty()
                        || after_name.starts_with(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/');
                    if boundary_ok {
                        let open_end = rest.find('>').map(|p| i + 1 + p + 1);
                        let close_pat = format!("</{tag}>");
                        if let Some(open_end_idx) = open_end {
                            // If a closing tag exists, skip the whole element; otherwise strip only the opening tag.
                            if let Some(close_rel) = lower[open_end_idx..].find(&close_pat) {
                                i = open_end_idx + close_rel + close_pat.len();
                            } else {
                                i = open_end_idx;
                            }
                            skipped_tag = true;
                            break;
                        }
                    }
                }
            }
            if skipped_tag {
                continue;
            }
        }

        let ch = html[i..].chars().next().unwrap();
        cleaned.push(ch);
        i += ch.len_utf8();
    }

    let raw_text = strip_html_tags(&cleaned);
    raw_text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

const IMAGE_REQUEST_NOUNS: &[&str] = &[
    "picture",
    "pictures",
    "pic",
    "pics",
    "photo",
    "photos",
    "image",
    "images",
    "meme",
    "memes",
    "gif",
    "gifs",
    "wallpaper",
    "wallpapers",
    "pfp",
    "fanart",
    "diagram",
    "diagrams",
    "screenshot",
    "screenshots",
];

const IMAGE_REQUEST_VERBS: &[&str] = &[
    "search", "find", "show", "send", "give", "get", "fetch", "lookup", "pull", "grab", "link",
    "post", "attach", "share", "drop",
];

/// Returns true when user text asks to search for, show, or send an image/picture/meme.
pub fn text_requests_image(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();

    let has_noun = words.iter().any(|w| IMAGE_REQUEST_NOUNS.contains(w));
    if !has_noun {
        return false;
    }

    if words.iter().any(|w| IMAGE_REQUEST_VERBS.contains(w)) {
        return true;
    }

    words.windows(2).any(|pair| {
        matches!(pair[1], "of" | "about") && IMAGE_REQUEST_NOUNS.contains(&pair[0])
    })
}

fn trim_extracted_url_tail(candidate: &str) -> &str {
    const TRAILING_PUNCT: [char; 6] = ['.', ',', ';', ':', '!', '?'];
    let mut s = candidate.trim_end_matches(TRAILING_PUNCT);
    // Strip unbalanced trailing ')' (e.g. closing a Markdown link) while keeping Wikipedia `Foo_(bar)`.
    while s.ends_with(')') {
        let opens = s.bytes().filter(|&b| b == b'(').count();
        let closes = s.bytes().filter(|&b| b == b')').count();
        if closes > opens {
            s = &s[..s.len() - 1];
            s = s.trim_end_matches(TRAILING_PUNCT);
        } else {
            break;
        }
    }
    s
}

fn scan_http_url_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut end = start;
    while end < bytes.len() {
        let b = bytes[end];
        if b.is_ascii_whitespace() || matches!(b, b'<' | b'>' | b'"' | b'\'' | b'`' | b'[' | b']' | b'\\')
        {
            break;
        }
        end += 1;
    }
    let trimmed = trim_extracted_url_tail(&text[start..end]);
    start + trimmed.len()
}

/// Extracts all unique `http://` and `https://` URLs from text, preserving order.
pub fn extract_http_urls(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let mut urls = Vec::new();
    let mut seen = HashSet::new();
    let mut pos = 0usize;

    while pos < lower.len() {
        let rest = &lower[pos..];
        let next_http = rest.find("http://");
        let next_https = rest.find("https://");
        let rel = match (next_http, next_https) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => break,
        };

        let start = pos + rel;
        let scheme_len = if lower[start..].starts_with("https://") {
            8
        } else {
            7
        };
        let end = scan_http_url_end(text, start);
        if end > start + scheme_len {
            let url = &text[start..end];
            if url[scheme_len..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
                && seen.insert(url.to_owned())
            {
                urls.push(url.to_owned());
            }
        }
        pos = end.max(start + scheme_len);
    }

    urls
}

fn canonical_url_key(url: &str) -> String {
    let without_frag = url.split('#').next().unwrap_or(url).trim();
    let without_slash = without_frag.trim_end_matches('/');
    if let Some(scheme_pos) = without_slash.find("://") {
        let scheme = &without_slash[..scheme_pos].to_ascii_lowercase();
        let after_scheme = &without_slash[scheme_pos + 3..];
        if let Some(slash_pos) = after_scheme.find('/') {
            let host = after_scheme[..slash_pos].to_ascii_lowercase();
            let path = &after_scheme[slash_pos..];
            format!("{scheme}://{host}{path}")
        } else {
            format!("{scheme}://{}", after_scheme.to_ascii_lowercase())
        }
    } else {
        without_slash.to_ascii_lowercase()
    }
}

fn build_canonical_set(verified: &HashSet<String>) -> HashSet<String> {
    verified.iter().map(|u| canonical_url_key(u)).collect()
}

fn looks_like_image_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let before_frag = lower.split('#').next().unwrap_or(&lower);
    let path_only = before_frag.split('?').next().unwrap_or(before_frag);
    if [".png", ".jpg", ".jpeg", ".webp", ".gif", ".bmp", ".avif"]
        .iter()
        .any(|ext| path_only.ends_with(ext))
    {
        return true;
    }
    ["format=jpg", "format=jpeg", "format=png", "format=webp", "format=gif", "tiktok.com/api/img", "bing.com/th"]
        .iter()
        .any(|pat| before_frag.contains(pat))
}

/// Returns URLs present in `text` that were not observed in `verified`.
pub fn find_unverified_urls(text: &str, verified: &HashSet<String>) -> Vec<String> {
    let canon = build_canonical_set(verified);
    extract_http_urls(text)
        .into_iter()
        .filter(|u| !verified.contains(u) && !canon.contains(&canonical_url_key(u)))
        .collect()
}

fn is_url_verified(url: &str, verified: &HashSet<String>, canon: &HashSet<String>) -> bool {
    verified.contains(url) || canon.contains(&canonical_url_key(url))
}

fn strip_markdown_links(
    text: &str,
    verified: &HashSet<String>,
    canon: &HashSet<String>,
    strip_images: bool,
) -> (String, bool) {
    let mut out = String::with_capacity(text.len());
    let mut did_strip = false;
    let mut i = 0usize;

    while i < text.len() {
        let is_md_image = text[i..].starts_with("![");
        let is_md_link = !is_md_image && text[i..].starts_with('[');
        if is_md_image || is_md_link {
            if let Some(label_end_rel) = text[i..].find("](") {
                let target_start = i + label_end_rel + 2;
                if let Some(target_end_rel) = text[target_start..].find(')') {
                    let span = &text[i..target_start + target_end_rel];
                    let raw_target = text[target_start..target_start + target_end_rel].trim();
                    let target_lower = raw_target.to_ascii_lowercase();
                    let is_placeholder = matches!(
                        target_lower.as_str(),
                        "" | "url" | "link" | "image" | "image_url" | "placeholder"
                    );
                    if !span.contains('\n') && (is_md_image
                        || is_placeholder
                        || target_lower.starts_with("http://")
                        || target_lower.starts_with("https://")
                        || target_lower.starts_with("attachment://"))
                    {
                        let end = target_start + target_end_rel + 1;
                        let should_strip =
                            if is_md_image || is_placeholder || target_lower.starts_with("attachment://") {
                                true
                            } else {
                                let url_end = scan_http_url_end(raw_target, 0);
                                let url = &raw_target[..url_end];
                                !is_url_verified(url, verified, canon)
                                    || (strip_images && looks_like_image_url(url))
                            };
                        if should_strip {
                            did_strip = true;
                            i = end;
                            continue;
                        }
                    }
                }
            }
        }

        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    (out, did_strip)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DraftedToolIssue {
    PlaceholderImage(Option<String>),
    UnfulfilledSearchPromise,
}

/// Detects when a model stopped early after drafting a fake Markdown image (`![alt](url)`)
/// or promising to search (`"... wait, no! Let me find..."`) without issuing a tool call.
pub fn detect_stalled_tool_draft(text: &str) -> Option<DraftedToolIssue> {
    let mut pos = 0usize;
    while let Some(rel) = text[pos..].find("![") {
        let start = pos + rel + 2;
        if let Some(label_end_rel) = text[start..].find("](") {
            let target_start = start + label_end_rel + 2;
            if let Some(target_end_rel) = text[target_start..].find(')') {
                if !text[start..target_start + target_end_rel].contains('\n') {
                    let alt = text[start..start + label_end_rel].trim();
                    let alt_opt = (!alt.is_empty()).then(|| alt.to_owned());
                    return Some(DraftedToolIssue::PlaceholderImage(alt_opt));
                }
            }
        }
        pos = start;
    }

    let trimmed = text.trim();
    let tail_start = trimmed
        .char_indices()
        .rev()
        .nth(180)
        .map_or(0, |(idx, _)| idx);
    let tail_lower = trimmed[tail_start..].to_ascii_lowercase();
    if tail_lower.contains("wait, no!")
        || tail_lower.contains("wait no!")
        || [
            "let me find ",
            "let me search ",
            "let me look up ",
            "let me grab ",
            "let me pull up ",
            "let me fetch ",
        ]
        .iter()
        .any(|pat| {
            tail_lower.starts_with(pat)
                || tail_lower.contains(&format!(" {pat}"))
                || tail_lower.contains(&format!("\n{pat}"))
        })
    {
        return Some(DraftedToolIssue::UnfulfilledSearchPromise);
    }

    None
}

/// Strips Markdown image links, unverified/hallucinated URLs, and redundant bare image URLs
/// (when images are attached), while keeping verified non-image webpage URLs intact.
pub fn clean_image_links_and_unverified_urls(
    text: &str,
    verified: &HashSet<String>,
    has_attached_images: bool,
) -> String {
    let canon = build_canonical_set(verified);
    let (stage1, mut did_strip) = strip_markdown_links(text, verified, &canon, has_attached_images);

    let lower = stage1.to_ascii_lowercase();
    let mut out = String::with_capacity(stage1.len());
    let mut i = 0usize;

    while i < stage1.len() {
        let rest = &lower[i..];
        let starts_http = rest.starts_with("http://") || rest.starts_with("https://");
        if starts_http {
            let scheme_len = if rest.starts_with("https://") { 8 } else { 7 };
            let end = scan_http_url_end(&stage1, i);
            if end > i + scheme_len {
                let url = &stage1[i..end];
                let drop_url =
                    !is_url_verified(url, verified, &canon) || (has_attached_images && looks_like_image_url(url));
                if drop_url {
                    did_strip = true;
                    // Clean enclosing `<url>` autolink brackets if present.
                    if out.ends_with('<') && stage1[end..].starts_with('>') {
                        out.pop();
                        i = end + 1;
                    } else {
                        i = end;
                    }
                    continue;
                }
                out.push_str(url);
                i = end;
                continue;
            }
        }

        let ch = stage1[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    if !did_strip {
        return text.to_owned();
    }

    // Tidy spaces around stripped URLs while preserving leading code indentation.
    let mut lines = Vec::new();
    for line in out.lines() {
        let body = line.trim_start_matches([' ', '\t']);
        let indent = &line[..line.len() - body.len()];
        let collapsed: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            lines.push(String::new());
        } else {
            lines.push(format!("{indent}{collapsed}"));
        }
    }
    let mut cleaned_lines: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines {
        if line.is_empty() && cleaned_lines.last().is_some_and(|prev| prev.is_empty()) {
            continue;
        }
        cleaned_lines.push(line);
    }
    cleaned_lines.join("\n").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_search_actions() {
        let text = "Let me check <search>osu pp rework 2026</search> and <browse>https://osu.ppy.sh</browse>";
        let actions = extract_search_actions(text, 2);
        assert_eq!(
            actions,
            vec![
                AiSearchAction::Search("osu pp rework 2026".to_owned()),
                AiSearchAction::Browse("https://osu.ppy.sh".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn test_ssrf_protection() {
        assert!(validate_url_for_ssrf("http://127.0.0.1:8080").await.is_err());
        assert!(validate_url_for_ssrf("http://localhost/admin").await.is_err());
        assert!(validate_url_for_ssrf("http://[::1]/").await.is_err());
        assert!(validate_url_for_ssrf("http://[fe80::1]/").await.is_err());
        assert!(validate_url_for_ssrf("http://[fd00::1]/").await.is_err());
        assert!(validate_url_for_ssrf("http://[::ffff:127.0.0.1]/").await.is_err());
        assert!(validate_url_for_ssrf("file:///etc/passwd").await.is_err());
    }

    #[test]
    fn test_extract_article_text_preserves_similar_tags_and_unclosed_blocks() {
        let html = "<nav>Menu</nav><navigation>Keep me</navigation><script>bad()</script><header>Unclosed header <p>Article body</p>";
        let text = extract_article_text(html);
        assert!(text.contains("Keep me"), "unexpected text: {text}");
        assert!(text.contains("Article body"), "unexpected text: {text}");
        assert!(!text.contains("bad()"), "unexpected text: {text}");
        assert!(!text.contains("Menu"), "unexpected text: {text}");
    }

    #[test]
    fn test_detects_ddg_challenge() {
        assert!(is_duckduckgo_challenge_html("<form id=\"challenge-form\">Please verify</form>"));
        assert!(!is_duckduckgo_challenge_html("<div class=\"result__title\">Normal result</div>"));
    }

    #[test]
    fn test_image_magic_bytes_and_filename_derivation() {
        assert_eq!(detect_image_extension(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]), Some("png"));
        assert_eq!(detect_image_extension(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(detect_image_extension(b"GIF89a..."), Some("gif"));
        assert_eq!(detect_image_extension(b"RIFF1234WEBP"), Some("webp"));
        assert_eq!(detect_image_extension(b"<html><body>404</body></html>"), None);

        let url = reqwest::Url::parse("https://example.com/photos/cool_cat-2026.jpeg?size=lg").unwrap();
        assert_eq!(derive_image_filename(&url, "jpg"), "cool_cat-2026.jpg");
    }

    #[test]
    fn test_extract_bing_and_page_images() {
        let bing_html = r#"<a m="{"murl":"x"}" mad="murl&quot;:&quot;https://example.com/a.jpg&quot;,&quot;t&quot;:&quot;Funny Cat&quot;">"#;
        let bing = parse_bing_image_results(bing_html);
        assert_eq!(bing.len(), 1);
        assert_eq!(bing[0].image_url, "https://example.com/a.jpg");
        assert_eq!(bing[0].title, "Funny Cat");

        let base = reqwest::Url::parse("https://example.com/post/1").unwrap();
        let page = r#"<html><head><meta property="og:image" content="/hero.png"></head><body><img src="data:image/png;base64,abc"><img src="/icon.svg"><img src="https://cdn.example.com/chart.webp"></body></html>"#;
        let imgs = extract_page_image_urls(page, &base);
        assert_eq!(
            imgs,
            vec![
                "https://example.com/hero.png".to_owned(),
                "https://cdn.example.com/chart.webp".to_owned(),
            ]
        );
    }

    #[test]
    fn test_text_requests_image_detects_prompts_without_false_positives() {
        assert!(text_requests_image("can you search a picture of a cute cat"));
        assert!(text_requests_image("show me a funny Kai Cenat meme"));
        assert!(text_requests_image("find pics of aurora borealis please"));
        assert!(text_requests_image("picture of a golden retriever"));
        assert!(text_requests_image("send a gif of a dancing raccoon"));

        assert!(!text_requests_image("what topic should we discuss today"));
        assert!(!text_requests_image("that is a typical compiler error"));
        assert!(!text_requests_image("attach a python script file for sorting"));
        assert!(!text_requests_image("who won OWC 2025?"));
    }

    #[test]
    fn test_extract_and_verify_urls_handles_markdown_and_parentheses() {
        let input = "See ![bad](https://fake.example.com/a.jpg) and https://en.wikipedia.org/wiki/Osu!_(video_game) plus [docs](https://docs.rs/tokio).";
        let extracted = extract_http_urls(input);
        assert_eq!(
            extracted,
            vec![
                "https://fake.example.com/a.jpg".to_owned(),
                "https://en.wikipedia.org/wiki/Osu!_(video_game)".to_owned(),
                "https://docs.rs/tokio".to_owned(),
            ]
        );

        let verified = HashSet::from([
            "https://en.wikipedia.org/wiki/Osu!_(video_game)".to_owned(),
            "https://docs.rs/tokio/".to_owned(),
        ]);
        assert_eq!(
            find_unverified_urls(input, &verified),
            vec!["https://fake.example.com/a.jpg".to_owned()]
        );
    }

    #[test]
    fn test_clean_image_links_and_unverified_urls() {
        let verified = HashSet::from([
            "https://docs.rs/serenity".to_owned(),
            "https://i.imgur.com/verified.png".to_owned(),
        ]);
        let raw = "Check [serenity](https://docs.rs/serenity) and ![meme](https://i.imgur.com/verified.png)\nFake: <https://broken.example.com/nope.jpg> ![puffy mouth meme](url) and direct https://i.imgur.com/verified.png";
        let cleaned = clean_image_links_and_unverified_urls(raw, &verified, true);
        assert_eq!(cleaned, "Check [serenity](https://docs.rs/serenity) and\nFake: and direct");
    }

    #[test]
    fn test_extract_inline_tool_calls_and_stalled_drafts() {
        let raw_call = "<think>hmm</think><tool_call>{\"name\":\"image_search\",\"arguments\":{\"query\":\"puffy mouth meme\"}}</tool_call>";
        let calls = extract_inline_tool_calls(raw_call, 3);
        assert_eq!(
            calls,
            vec![(
                "image_search".to_owned(),
                serde_json::json!({"query": "puffy mouth meme"}),
            )]
        );

        let stalled = "check this out:\n![puffy mouth meme](url) ... wait, no! Let me find the actual verified image for real";
        assert_eq!(
            detect_stalled_tool_draft(stalled),
            Some(DraftedToolIssue::PlaceholderImage(Some("puffy mouth meme".to_owned())))
        );
        assert_eq!(
            detect_stalled_tool_draft("Hold on... let me find a verified picture first."),
            Some(DraftedToolIssue::UnfulfilledSearchPromise)
        );
        assert_eq!(
            detect_stalled_tool_draft("That exam prep is looking rough ngl 💀"),
            None
        );
    }

    #[test]
    fn test_usable_candidate_image_url_filters_known_bad_hosts() {
        assert!(is_usable_candidate_image_url("https://i.imgflip.com/9abc.jpg"));
        assert!(!is_usable_candidate_image_url("https://www.tiktok.com/api/img/?itemId=123"));
        assert!(!is_usable_candidate_image_url("https://example.com/logo.svg"));
        assert!(!is_usable_candidate_image_url("https://www.bing.com/th/id/ogc123"));
    }
}
