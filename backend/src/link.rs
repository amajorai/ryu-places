//! Read a shared link (a TikTok, Instagram reel, YouTube video, blog post, or
//! list) into text the place extractor can work on.
//!
//! TikTok and YouTube publish public oEmbed endpoints that return the caption or
//! title without scraping. Everything else is fetched once and reduced to its
//! title, OpenGraph/meta description, and a bounded slice of visible text.
//! Instagram and some other platforms put posts behind a login wall; when the
//! page yields nothing useful the caller says so and asks for pasted text.
//!
//! This is a server-side fetch of a user-supplied URL, so it refuses anything
//! that could reach the node's own network: only `https`, only public IPs
//! (checked after DNS resolution, and again for every redirect hop), a short
//! timeout, and a size cap.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_BYTES: usize = 1_500_000;
const MAX_TEXT_CHARS: usize = 8_000;
const MAX_REDIRECTS: usize = 4;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LinkError {
    #[error("enter a full https:// link")]
    NotHttps,
    #[error("that link points at a private or local address")]
    PrivateAddress,
    #[error("the link could not be read; paste the caption or text instead")]
    Unreadable,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LinkContent {
    pub url: String,
    pub provider: String,
    pub title: Option<String>,
    pub author: Option<String>,
    /// Caption, description, and visible text, bounded.
    pub text: String,
}

pub fn provider_of(url: &reqwest::Url) -> &'static str {
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_start_matches("www.");
    let is = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    if is("tiktok.com") {
        "tiktok"
    } else if is("youtube.com") || host == "youtu.be" {
        "youtube"
    } else if is("instagram.com") {
        "instagram"
    } else if is("google.com") && url.path().starts_with("/maps") || host == "maps.app.goo.gl" {
        "google-maps"
    } else if is("x.com") || is("twitter.com") {
        "x"
    } else if is("reddit.com") {
        "reddit"
    } else {
        "web"
    }
}

/// Parse and vet a user-supplied URL before any request is made.
pub fn parse_public_url(raw: &str) -> Result<reqwest::Url, LinkError> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|_| LinkError::NotHttps)?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(LinkError::NotHttps);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(LinkError::NotHttps);
    }
    Ok(url)
}

pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || a == 0
                || (a == 100 && (64..=127).contains(&b)) // CGNAT
                || (a == 198 && (b == 18 || b == 19)) // benchmarking
                || a >= 224) // multicast + reserved
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00 // unique local
                || (first & 0xffc0) == 0xfe80) // link local
        }
    }
}

async fn ensure_public_host(url: &reqwest::Url) -> Result<()> {
    let host = url.host_str().ok_or(LinkError::NotHttps)?;
    let port = url.port_or_known_default().unwrap_or(443);
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        return if is_public_ip(ip) {
            Ok(())
        } else {
            Err(LinkError::PrivateAddress.into())
        };
    }
    let addresses: Vec<_> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| LinkError::Unreadable)?
        .collect();
    if addresses.is_empty() || addresses.iter().any(|addr| !is_public_ip(addr.ip())) {
        return Err(LinkError::PrivateAddress.into());
    }
    Ok(())
}

#[derive(Clone)]
pub struct LinkReader {
    http: reqwest::Client,
}

impl Default for LinkReader {
    fn default() -> Self {
        Self::new()
    }
}

impl LinkReader {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!(
                "Mozilla/5.0 (compatible; ryu-places/",
                env!("CARGO_PKG_VERSION"),
                ")"
            ))
            .build()
            .unwrap_or_default();
        Self { http }
    }

    pub async fn read(&self, raw: &str) -> Result<LinkContent> {
        let url = parse_public_url(raw)?;
        let provider = provider_of(&url);
        let oembed = match provider {
            "tiktok" => Some(format!(
                "https://www.tiktok.com/oembed?url={}",
                encode(url.as_str())
            )),
            "youtube" => Some(format!(
                "https://www.youtube.com/oembed?format=json&url={}",
                encode(url.as_str())
            )),
            _ => None,
        };
        if let Some(endpoint) = oembed {
            if let Ok(content) = self.read_oembed(&endpoint, &url, provider).await {
                return Ok(content);
            }
        }
        let (final_url, html) = self.fetch(url).await?;
        let content = from_html(&final_url, provider_of(&final_url), &html);
        if content.text.trim().is_empty() && content.title.is_none() {
            return Err(LinkError::Unreadable.into());
        }
        Ok(content)
    }

    async fn read_oembed(
        &self,
        endpoint: &str,
        url: &reqwest::Url,
        provider: &str,
    ) -> Result<LinkContent> {
        let (_, body) = self.fetch(reqwest::Url::parse(endpoint)?).await?;
        let value: Value = serde_json::from_str(&body).context("oEmbed was not JSON")?;
        let text_of = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        let title = text_of("title");
        Ok(LinkContent {
            url: url.to_string(),
            provider: provider.to_owned(),
            text: title.clone().unwrap_or_default(),
            title,
            author: text_of("author_name"),
        })
    }

    /// GET with manual, re-vetted redirects and a hard size cap.
    async fn fetch(&self, mut url: reqwest::Url) -> Result<(reqwest::Url, String)> {
        for _ in 0..=MAX_REDIRECTS {
            ensure_public_host(&url).await?;
            let mut response = self
                .http
                .get(url.clone())
                .send()
                .await
                .map_err(|_| LinkError::Unreadable)?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or(LinkError::Unreadable)?;
                let next = url.join(location).map_err(|_| LinkError::Unreadable)?;
                url = parse_public_url(next.as_str())?;
                continue;
            }
            if !response.status().is_success() {
                return Err(LinkError::Unreadable.into());
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| LinkError::Unreadable)? {
                body.extend_from_slice(&chunk);
                if body.len() > MAX_BYTES {
                    break;
                }
            }
            body.truncate(MAX_BYTES);
            return Ok((url, String::from_utf8_lossy(&body).into_owned()));
        }
        Err(anyhow!(LinkError::Unreadable))
    }
}

fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Reduce an HTML page to the parts that name places: its title, OpenGraph and
/// meta descriptions, and the visible text with scripts and styles removed.
pub fn from_html(url: &reqwest::Url, provider: &str, html: &str) -> LinkContent {
    let meta = |key: &str| meta_content(html, key);
    let title = meta("og:title").or_else(|| tag_text(html, "title"));
    let description = meta("og:description").or_else(|| meta("description"));
    let mut text = String::new();
    if let Some(description) = &description {
        text.push_str(description);
        text.push_str("\n\n");
    }
    text.push_str(&visible_text(html));
    let text: String = text.chars().take(MAX_TEXT_CHARS).collect();
    LinkContent {
        url: url.to_string(),
        provider: provider.to_owned(),
        title: title.map(|t| decode_entities(t.trim())),
        author: meta("author").or_else(|| meta("og:site_name")),
        text: decode_entities(text.trim()),
    }
}

fn meta_content(html: &str, key: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let needles = [format!("property=\"{key}\""), format!("name=\"{key}\"")];
    for needle in needles {
        let mut from = 0;
        while let Some(found) = lower[from..].find(&needle) {
            let at = from + found;
            let start = lower[..at].rfind("<meta")?;
            let end = lower[at..].find('>').map_or(lower.len(), |e| at + e);
            let tag = &html[start..end];
            let tag_lower = &lower[start..end];
            if let Some(content_at) = tag_lower.find("content=") {
                let rest = &tag[content_at + 8..];
                let quote = rest.chars().next()?;
                if quote == '"' || quote == '\'' {
                    let value = rest[1..].split(quote).next()?.trim();
                    if !value.is_empty() {
                        return Some(decode_entities(value));
                    }
                }
            }
            from = end;
        }
    }
    None
}

fn tag_text(html: &str, tag: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find(&format!("<{tag}"))?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find(&format!("</{tag}"))?;
    let value = html[start..end].trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn visible_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::new();
    let mut index = 0;
    let bytes = html.as_bytes();
    while index < bytes.len() {
        if bytes[index] == b'<' {
            for skip in ["script", "style", "noscript", "svg", "head"] {
                if lower[index..].starts_with(&format!("<{skip}")) {
                    if let Some(close) = lower[index..].find(&format!("</{skip}>")) {
                        index += close + skip.len() + 3;
                    }
                    break;
                }
            }
            if index >= bytes.len() {
                break;
            }
            if bytes[index] == b'<' {
                match html[index..].find('>') {
                    Some(close) => index += close + 1,
                    None => break,
                }
                out.push(' ');
                continue;
            }
        }
        let next = html[index..].find('<').map_or(html.len(), |n| index + n);
        out.push_str(&html[index..next]);
        index = next;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_entities(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
}
