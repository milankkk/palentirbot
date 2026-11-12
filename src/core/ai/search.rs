use std::time::Duration;

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
struct SearxResponse {
    #[serde(default)]
    pub results: Vec<SearchResultItem>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum AiSearchAction {
    Search(String),
    Browse(String),
}

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
            .header(
                reqwest::header::USER_AGENT,
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36",
            )
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
        if !status.is_success() {
            eyre::bail!("DuckDuckGo returned HTTP {status}");
        }

        let body = res
            .text()
            .await
            .context("failed to read DuckDuckGo response")?;
        Ok(parse_duckduckgo_html(&body))
    }

    /// Fetch and extract readable plain text from a URL, following up to 3 SSRF-validated redirects.
    pub async fn browse_url(&self, raw_url: &str, max_chars: usize) -> Result<String> {
        let mut current_url = validate_url_for_ssrf(raw_url).await?;

        for _ in 0..4 {
            let res = self
                .client
                .get(current_url.clone())
                .header(
                    reqwest::header::USER_AGENT,
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36",
                )
                .header(
                    reqwest::header::ACCEPT,
                    "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5",
                )
                .send()
                .await
                .context("failed to fetch webpage")?;

            if res.status().is_redirection() {
                if let Some(loc) = res.headers().get(reqwest::header::LOCATION) {
                    if let Ok(loc_str) = loc.to_str() {
                        let next_url = current_url
                            .join(loc_str)
                            .context("invalid redirect Location")?;
                        current_url = validate_url_for_ssrf(next_url.as_str()).await?;
                        continue;
                    }
                }
                eyre::bail!("redirect without valid Location header");
            }

            if !res.status().is_success() {
                eyre::bail!("webpage returned HTTP {}", res.status());
            }

            let bytes = res.bytes().await.context("failed to read webpage bytes")?;
            let slice = &bytes[..bytes.len().min(256 * 1024)];
            let html = String::from_utf8_lossy(slice);
            let extracted = extract_article_text(&html);
            let truncated: String = extracted.chars().take(max_chars).collect();
            return Ok(truncated);
        }

        eyre::bail!("too many redirects")
    }
}

/// Format search results into a compact text block for the LLM system prompt.
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
                || v6.to_ipv4_mapped().is_some_and(is_private_or_local_ip_v4)
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || (v6.segments()[0] == 0x2001 && v6.segments()[1] == 0x0db8)
        }
    }
}

pub async fn validate_url_for_ssrf(url_str: &str) -> Result<reqwest::Url> {
    let raw = url_str
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '<' | '>'))
        .trim();
    let parsed = reqwest::Url::parse(raw).context("invalid URL")?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        eyre::bail!("URL scheme must be http or https");
    }

    let host_str = parsed
        .host_str()
        .ok_or_else(|| eyre::eyre!("URL has no host"))?;
    let host_lower = host_str.to_ascii_lowercase();

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

pub fn extract_article_text(html: &str) -> String {
    let mut cleaned = html.to_string();
    for tag in &["script", "style", "noscript", "svg", "header", "footer", "nav"] {
        let open_pat = format!("<{tag}");
        let close_pat = format!("</{tag}>");
        loop {
            let lower = cleaned.to_ascii_lowercase();
            if let Some(start) = lower.find(&open_pat) {
                if let Some(end) = lower[start..].find(&close_pat) {
                    cleaned.drain(start..start + end + close_pat.len());
                    continue;
                } else {
                    cleaned.truncate(start);
                }
            }
            break;
        }
    }

    let raw_text = strip_html_tags(&cleaned);
    raw_text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
        assert!(validate_url_for_ssrf("file:///etc/passwd").await.is_err());
    }
}
