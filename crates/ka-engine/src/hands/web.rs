//! Web search + URL reader hands. Search walks the configured
//! `[[search]]` entries as a fallback chain (tavily / brave / bing
//! keyed backends, duckduckgo / searxng keyless — payload shapes mapped
//! to one `SearchHit` list); the first entry that succeeds wins, and
//! total failure reports every backend's error. `[[search.sites]]`
//! entries scope a provider's queries with the `site:` operator (v1
//! handlers are site-scoping only — no per-site scraping). `web_fetch`
//! reads a URL and strips HTML to plain text. Both pay exec-tier
//! clearance: the query leaves the machine, and fetches are
//! SSRF-guarded against private targets.

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};

use super::{Clearance, Hand, HandContext, HandDef, ToolOutput};
use crate::config::SearchProvider;

/// One normalized search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// `web_search` hand: walks the provider fallback chain (first
/// success wins).
pub struct WebSearchHand {
    providers: Vec<SearchProvider>,
}

impl WebSearchHand {
    pub fn new(providers: Vec<SearchProvider>) -> Self {
        Self { providers }
    }
}

/// `web_fetch` hand.
pub struct WebFetchHand {
    allow_private: bool,
}

impl WebFetchHand {
    pub fn new(allow_private: bool) -> Self {
        Self { allow_private }
    }
}

fn base_for(provider: &SearchProvider, default: &str) -> String {
    provider
        .base_url
        .clone()
        .unwrap_or_else(|| default.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// One provider attempt: keyless backends go straight through; keyed
/// backends resolve their env key first (missing/unset key = a
/// chain-recoverable failure, not a hard error).
async fn provider_search(provider: &SearchProvider, query: &str) -> Result<Vec<SearchHit>, String> {
    match provider.provider.as_str() {
        "duckduckgo" => ddg_search(provider, query).await,
        "searxng" => searxng_search(provider, query).await,
        "tavily" | "brave" | "bing" => {
            let env = provider.api_key_env.as_deref().ok_or_else(|| {
                format!(
                    "{}: requires api_key_env (the keyed backends carry no default env ladder)",
                    provider.provider
                )
            })?;
            let key = ka_dialect::auth::resolve_token(env)
                .ok_or_else(|| format!("key ${env} is not set"))?;
            provider_search_with(provider, query, &key).await
        }
        other => Err(format!("unknown provider {other:?}")),
    }
}

/// Append the provider's `[[search.sites]]` entries to the query as
/// `site:` operators. Unknown handler ids fail the provider (v1 has no
/// site-scraping handlers — `auto` is the only one).
fn scoped_query(provider: &SearchProvider, query: &str) -> Result<String, String> {
    if provider.sites.is_empty() {
        return Ok(query.to_string());
    }
    let mut scoped = query.to_string();
    for site in &provider.sites {
        match site.handler.as_deref().unwrap_or("auto") {
            "auto" => scoped.push_str(&format!(" site:{}", site.site)),
            other => {
                return Err(format!(
                    "sites.handler {other:?} is not supported (v1 handlers = site-scoping only: \
                     omit handler or set \"auto\")"
                ));
            }
        }
    }
    Ok(scoped)
}

/// Walk the provider chain: the first backend that returns hits (or an
/// empty-but-valid result) wins; every failure is collected so the
/// total-failure error says what each backend did.
async fn chained_search(
    providers: &[SearchProvider],
    query: &str,
) -> Result<Vec<SearchHit>, String> {
    let mut failures: Vec<String> = Vec::new();
    for provider in providers {
        let scoped = match scoped_query(provider, query) {
            Ok(q) => q,
            Err(e) => {
                failures.push(format!("{}: {e}", provider.provider));
                continue;
            }
        };
        match provider_search(provider, &scoped).await {
            Ok(hits) => return Ok(hits),
            Err(e) => failures.push(format!("{}: {e}", provider.provider)),
        }
    }
    Err(format!(
        "web_search: all configured providers failed — {}",
        failures.join(" | ")
    ))
}

/// DuckDuckGo HTML endpoint (keyless): `html.duckduckgo.com/html/?q=`
/// serves an HTML result page; anchors carry `result__a` (title/link)
/// and `result__snippet` classes. Hits often point at DDG's redirector
/// (`…/l/?uddg=<encoded-target>`) — the real target is decoded out.
async fn ddg_search(provider: &SearchProvider, query: &str) -> Result<Vec<SearchHit>, String> {
    let url = format!(
        "{}/html/?q={}",
        base_for(provider, "https://html.duckduckgo.com"),
        urlencode(query)
    );
    let client = reqwest::Client::new();
    let html = client
        .get(&url)
        .header("User-Agent", "ka-web-search")
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("duckduckgo: {e}"))?
        .error_for_status()
        .map_err(|e| format!("duckduckgo: {e}"))?
        .text()
        .await
        .map_err(|e| format!("duckduckgo: {e}"))?;
    Ok(parse_ddg_html(&html))
}

/// SearXNG JSON API (keyless): `<instance>/search?q=&format=json`.
/// The instance URL is mandatory configuration — without it there is
/// nothing keyless to fall back on.
async fn searxng_search(provider: &SearchProvider, query: &str) -> Result<Vec<SearchHit>, String> {
    let base = provider
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "searxng: base_url is required (the SearXNG instance to query, e.g. \
             \"https://searx.example.org\")"
                .to_string()
        })?;
    let url = format!(
        "{}/search?q={}&format=json",
        base.trim_end_matches('/'),
        urlencode(query)
    );
    let client = reqwest::Client::new();
    let resp: Value = client
        .get(&url)
        .header("User-Agent", "ka-web-search")
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("searxng: {e}"))?
        .error_for_status()
        .map_err(|e| format!("searxng: {e}"))?
        .json()
        .await
        .map_err(|e| format!("searxng JSON: {e}"))?;
    Ok(resp["results"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|r| SearchHit {
                    title: r["title"].as_str().unwrap_or_default().into(),
                    url: r["url"].as_str().unwrap_or_default().into(),
                    snippet: r["content"].as_str().unwrap_or_default().into(),
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Map DDG HTML anchors (`result__a` title/link pairs followed by a
/// `result__snippet`) to hits. DDG's markup is stable enough for the
/// two classes; anything else on the page is ignored.
fn parse_ddg_html(html: &str) -> Vec<SearchHit> {
    let mut hits: Vec<SearchHit> = Vec::new();
    let mut open: Option<SearchHit> = None; // title+url awaiting its snippet
    let mut cursor = 0usize;
    while let Some(rel) = html[cursor..].find("<a ") {
        let tag_start = cursor + rel;
        let Some(gt) = html[tag_start..].find('>') else {
            break;
        };
        let tag = &html[tag_start..=tag_start + gt];
        let class = attr_value(tag, "class").unwrap_or_default();
        let is_title = class.contains("result__a");
        let is_snippet = class.contains("result__snippet");
        if !is_title && !is_snippet {
            cursor = tag_start + gt + 1;
            continue;
        }
        // anchor text runs to the matching close tag
        let body_start = tag_start + gt + 1;
        let Some(close) = html[body_start..].find("</a>") else {
            break;
        };
        let text = decode_entities(&html[body_start..body_start + close]);
        cursor = body_start + close + 4;
        if is_title {
            let href = attr_value(tag, "href").unwrap_or_default();
            open = Some(SearchHit {
                title: text,
                url: ddg_real_url(&href),
                snippet: String::new(),
            });
        } else if let Some(mut hit) = open.take() {
            hit.snippet = text;
            if !hit.url.is_empty() {
                hits.push(hit);
            }
        }
    }
    hits
}

/// Resolve DDG's redirect links (`//duckduckgo.com/l/?uddg=<encoded>`)
/// to the real target; plain links come back normalized to https.
fn ddg_real_url(href: &str) -> String {
    if let Some(rest) = href.split("uddg=").nth(1) {
        let enc = rest.split('&').next().unwrap_or(rest);
        return percent_decode(enc);
    }
    if let Some(rest) = href.strip_prefix("//") {
        return format!("https://{rest}");
    }
    href.to_string()
}

/// Minimal percent-decoding for query parameters (UTF-8 lossy).
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decode the HTML entities that show up in search titles/snippets
/// (named + decimal + hex); unknown entities pass through untouched.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        let mut entity = String::new();
        for n in chars.by_ref() {
            if n == ';' {
                break;
            }
            entity.push(n);
            if entity.len() > 10 {
                break;
            }
        }
        match entity.as_str() {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            "nbsp" => out.push(' '),
            numeric if numeric.starts_with("#x") || numeric.starts_with("#X") => {
                match u32::from_str_radix(&numeric[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
                {
                    Some(ch) => out.push(ch),
                    None => out.push_str(&format!("&{entity};")),
                }
            }
            numeric if numeric.starts_with('#') => {
                match numeric[1..].parse::<u32>().ok().and_then(char::from_u32) {
                    Some(ch) => out.push(ch),
                    None => out.push_str(&format!("&{entity};")),
                }
            }
            _ => out.push_str(&format!("&{entity};")),
        }
    }
    out
}

/// Search with an explicit key (tests inject one; the env ladder is
/// untestable under edition-2024 set_var rules).
async fn provider_search_with(
    provider: &SearchProvider,
    query: &str,
    key: &str,
) -> Result<Vec<SearchHit>, String> {
    let client = reqwest::Client::new();
    let hits: Vec<SearchHit> = match provider.provider.as_str() {
        "tavily" => {
            let url = format!("{}/search", base_for(provider, "https://api.tavily.com"));
            let resp: Value = client
                .post(&url)
                .header("Authorization", format!("Bearer {key}"))
                .json(&json!({"query": query, "max_results": 5}))
                .send()
                .await
                .map_err(|e| format!("tavily: {e}"))?
                .error_for_status()
                .map_err(|e| format!("tavily: {e}"))?
                .json()
                .await
                .map_err(|e| format!("tavily JSON: {e}"))?;
            resp["results"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|r| SearchHit {
                            title: r["title"].as_str().unwrap_or_default().into(),
                            url: r["url"].as_str().unwrap_or_default().into(),
                            snippet: r["content"].as_str().unwrap_or_default().into(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        "brave" => {
            let url = format!(
                "{}/v1/web/search?q={}",
                base_for(provider, "https://api.search.brave.com"),
                urlencode(query)
            );
            let resp: Value = client
                .get(&url)
                .header("X-Subscription-Token", key)
                .header("Accept", "application/json")
                .send()
                .await
                .map_err(|e| format!("brave: {e}"))?
                .error_for_status()
                .map_err(|e| format!("brave: {e}"))?
                .json()
                .await
                .map_err(|e| format!("brave JSON: {e}"))?;
            resp["web"]["results"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|r| SearchHit {
                            title: r["title"].as_str().unwrap_or_default().into(),
                            url: r["url"].as_str().unwrap_or_default().into(),
                            snippet: r["description"].as_str().unwrap_or_default().into(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        "bing" => {
            let url = format!(
                "{}/v7.0/search?q={}",
                base_for(provider, "https://api.bing.microsoft.com"),
                urlencode(query)
            );
            let resp: Value = client
                .get(&url)
                .header("Ocp-Apim-Subscription-Key", key)
                .send()
                .await
                .map_err(|e| format!("bing: {e}"))?
                .error_for_status()
                .map_err(|e| format!("bing: {e}"))?
                .json()
                .await
                .map_err(|e| format!("bing JSON: {e}"))?;
            resp["webPages"]["value"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|r| SearchHit {
                            title: r["name"].as_str().unwrap_or_default().into(),
                            url: r["url"].as_str().unwrap_or_default().into(),
                            snippet: r["snippet"].as_str().unwrap_or_default().into(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        other => return Err(format!("web_search: unknown provider {other:?}")),
    };
    Ok(hits)
}

/// Minimal percent-encoding for query strings.
fn urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Whether a URL's host is private/loopback/link-local.
pub fn is_private_host(url: &str) -> bool {
    let Some(host) = host_of(url) else {
        return true; // unparseable: treat as private (refuse)
    };
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local") {
        return true;
    }
    match lower.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        Ok(IpAddr::V6(v6)) => {
            // unique-local fc00::/7, loopback, link-local fe80::/10 ...
            let scoped = v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80;
            // ... and anything IPv4-mapped (e.g. ::ffff:127.0.0.1) judged
            // by its embedded v4 address — the mapped spelling must not
            // slip past the v4 checks
            scoped
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_loopback() || v4.is_private() || v4.is_link_local())
        }
        Err(_) => false, // public hostname: allowed (SSRF via DNS rebinding
                         // is out of scope for a tool-level guard)
    }
}

/// Extract the host component of a URL (no external crates).
fn host_of(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://").unwrap_or(("https", url));
    let rest = rest.split('/').next()?;
    let rest = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    if let Some(inner) = rest.strip_prefix('[') {
        return inner.split(']').next().map(str::to_string);
    }
    Some(rest.split(':').next()?.to_string())
}

/// Reduce HTML to readable plain text: drop script/style/nav blocks,
/// keep headings, paragraphs, list items, links (text + href) and code.
pub fn html_to_text(html: &str) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(html.len() / 2);
    let mut chars = html.chars().peekable();
    // block-level tags that introduce a line break
    const BLOCKS: &[&str] = &[
        "p",
        "div",
        "br",
        "li",
        "tr",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "table",
        "ul",
        "ol",
        "pre",
        "blockquote",
        "section",
        "article",
        "header",
        "footer",
    ];
    let mut skip_depth = 0usize; // inside script/style/nav
    while let Some(c) = chars.next() {
        if c == '<' {
            let mut tag = String::new();
            for n in chars.by_ref() {
                if n == '>' {
                    break;
                }
                tag.push(n);
            }
            let lower = tag.to_ascii_lowercase();
            let name = lower
                .trim()
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            if matches!(name.as_str(), "script" | "style" | "nav" | "head") {
                if lower.starts_with('/') {
                    skip_depth = skip_depth.saturating_sub(1);
                } else {
                    skip_depth += 1;
                }
                continue;
            }
            if skip_depth > 0 {
                continue;
            }
            if BLOCKS.contains(&name.as_str()) {
                text.push('\n');
            }
            // keep href of links as plain reference
            if name == "a" && !lower.starts_with('/') {
                if let Some(href) = attr_value(&lower, "href") {
                    let _ = write!(text, "[{href}] ");
                }
            }
            continue;
        }
        if skip_depth > 0 {
            continue;
        }
        if c == '&' {
            // tiny entity handling
            let mut entity = String::new();
            for n in chars.by_ref() {
                if n == ';' {
                    break;
                }
                entity.push(n);
                if entity.len() > 8 {
                    break;
                }
            }
            match entity.as_str() {
                "amp" => text.push('&'),
                "lt" => text.push('<'),
                "gt" => text.push('>'),
                "quot" => text.push('"'),
                "nbsp" => text.push(' '),
                _ => {
                    text.push('&');
                    text.push_str(&entity);
                    text.push(';');
                }
            }
            continue;
        }
        text.push(c);
    }
    text.trim().to_string()
}

/// Pull `attr="value"` out of a tag string.
fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let idx = tag.to_ascii_lowercase().find(attr)?;
    let rest = &tag[idx + attr.len()..];
    let rest = rest.trim_start_matches('=');
    let rest = rest.trim_start();
    let quoted = rest.starts_with('"') || rest.starts_with('\'');
    let quote = rest.chars().next()?;
    if quoted {
        rest[1..].split(quote).next().map(str::to_string)
    } else {
        rest.split_whitespace().next().map(str::to_string)
    }
}

impl Hand for WebSearchHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "web_search".into(),
            description: "Search the web via the configured providers (fallback chain, first \
                          success wins; [[search.sites]] entries scope queries with site:). \
                          Returns title/url/snippet results. The query leaves the machine."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query"}
                },
                "required": ["query"]
            }),
            clearance: Clearance::Exec,
            read_only: true,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        let providers = self.providers.clone();
        Box::pin(async move {
            let Some(query) = args.get("query").and_then(Value::as_str) else {
                return ToolOutput::err("web_search: missing required 'query'");
            };
            match chained_search(&providers, query).await {
                Ok(hits) if hits.is_empty() => ToolOutput::ok("(no results)"),
                Ok(hits) => {
                    let mut out = String::new();
                    for (i, h) in hits.iter().enumerate() {
                        out.push_str(&format!(
                            "{}. {} — {}\n   {}\n",
                            i + 1,
                            h.title,
                            h.url,
                            h.snippet
                        ));
                    }
                    ToolOutput::ok(out)
                }
                Err(e) => ToolOutput::err(e),
            }
        })
    }
}

/// Fetch size cap: 200 KB.
const FETCH_CAP: usize = 200 * 1024;

impl Hand for WebFetchHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "web_fetch".into(),
            description: "Fetch a URL and return its content as plain text (HTML reduced to \
                          readable text). Private/loopback hosts are refused unless allowed."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Absolute http(s) URL"}
                },
                "required": ["url"]
            }),
            clearance: Clearance::Exec,
            read_only: true,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        let allow_private = self.allow_private;
        Box::pin(async move {
            let Some(url) = args.get("url").and_then(Value::as_str) else {
                return ToolOutput::err("web_fetch: missing required 'url'");
            };
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return ToolOutput::err("web_fetch: only http(s) URLs are supported");
            }
            if !allow_private && is_private_host(url) {
                return ToolOutput::err(format!(
                    "web_fetch: refusing private/loopback target {url} ([tools.web] \
                     allow_private_hosts = true to override)"
                ));
            }
            // redirect policy: every hop is re-validated — an open
            // redirect (or shortened URL) pointing at a private host
            // must fail the fetch, not bypass the guard above
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                    if attempt.previous().len() > 10 {
                        return attempt.error("web_fetch: too many redirects");
                    }
                    if !allow_private && is_private_host(attempt.url().as_str()) {
                        return attempt
                            .error("web_fetch: redirect to private/loopback target refused");
                    }
                    attempt.follow()
                }))
                .build();
            let Ok(client) = client else {
                return ToolOutput::err("web_fetch: client build failed");
            };
            let resp = client
                .get(url)
                .header("User-Agent", "ka-web-fetch")
                .timeout(std::time::Duration::from_secs(20))
                .send()
                .await;
            let resp = match resp {
                Ok(r) => r,
                Err(e) => return ToolOutput::err(format!("web_fetch: {e}")),
            };
            let status = resp.status();
            if !status.is_success() {
                return ToolOutput::err(format!("web_fetch: status {status}"));
            }
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            // stream the body with a hard byte budget — a multi-GB
            // response must never be fully materialized before capping
            let mut body: Vec<u8> = Vec::with_capacity(64 * 1024);
            let mut resp = resp;
            while body.len() < FETCH_CAP {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        let room = FETCH_CAP - body.len();
                        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
                    }
                    Ok(None) => break,
                    Err(e) => return ToolOutput::err(format!("web_fetch: {e}")),
                }
            }
            let raw = String::from_utf8_lossy(&body).into_owned();
            let text = if ct.contains("html") || raw.trim_start().starts_with('<') {
                html_to_text(&raw)
            } else {
                raw
            };
            if text.trim().is_empty() {
                return ToolOutput::ok("(empty page)");
            }
            ToolOutput::ok(text)
        })
    }
}

/// Both web hands over the provider chain (engine bootstrap). The
/// search hand registers only when at least one `[[search]]` entry
/// exists; fetch always registers.
pub fn hands(providers: Vec<SearchProvider>, allow_private: bool) -> Vec<Arc<dyn Hand>> {
    let mut out: Vec<Arc<dyn Hand>> = Vec::new();
    if !providers.is_empty() {
        out.push(Arc::new(WebSearchHand::new(providers)));
    }
    out.push(Arc::new(WebFetchHand::new(allow_private)));
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::config::SearchSite;

    #[test]
    fn html_to_text_strips_scripts_keeps_structure() {
        let html = "<html><head><style>p{}</style></head><body>                    <nav>menu</nav><h1>Title</h1>                    <p>First para &amp; more.</p>                    <script>alert(1)</script>                    <a href=\"https://x.y\">a link</a>                    <ul><li>item</li></ul></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Title"), "{text}");
        assert!(text.contains("First para & more."), "{text}");
        assert!(text.contains("a link"), "{text}");
        assert!(text.contains("https://x.y"), "{text}");
        assert!(text.contains("item"), "{text}");
        assert!(!text.contains("alert"), "{text}");
        assert!(!text.contains("menu"), "{text}");
        assert!(!text.contains("p{}"), "{text}");
    }

    #[test]
    fn ssrf_guard_refuses_private_targets() {
        assert!(is_private_host("http://127.0.0.1:9000/x"));
        assert!(is_private_host("http://localhost/x"));
        assert!(is_private_host("http://10.0.0.5/"));
        assert!(is_private_host("http://192.168.1.1/"));
        assert!(is_private_host("http://169.254.1.1/"));
        assert!(is_private_host("http://[::1]/"));
        assert!(!is_private_host("https://example.com/x"));
    }

    /// Serve a canned HTTP response; returns the bound address.
    async fn serve(body: &'static str) -> std::net::SocketAddr {
        serve_status("200 OK", "application/json", body).await.0
    }

    /// Serve a canned response with an explicit status line; the
    /// request line (method + path) is captured for query assertions.
    async fn serve_status(
        status: &str,
        content_type: &str,
        body: &'static str,
    ) -> (
        std::net::SocketAddr,
        std::sync::Arc<parking_lot::Mutex<String>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(String::new()));
        let seen_for_task = seen.clone();
        let status = status.to_string();
        let content_type = content_type.to_string();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen_for_task.clone();
                let status = status.clone();
                let content_type = content_type.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    // capture the request line for query-shape assertions
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    if let Some(line) = head.lines().next() {
                        *seen.lock() = line.to_string();
                    }
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    sock.write_all(resp.as_bytes()).await.ok();
                });
            }
        });
        (addr, seen)
    }

    fn keyed(provider: &str, env: &str, base: Option<String>) -> SearchProvider {
        SearchProvider {
            provider: provider.into(),
            api_key_env: Some(env.into()),
            base_url: base,
            sites: Vec::new(),
        }
    }

    #[tokio::test]
    async fn tavily_provider_maps_hits() {
        let body = r#"{"results":[
            {"title":"R1","url":"https://a","content":"snippet a"},
            {"title":"R2","url":"https://b","content":"snippet b"}]}"#;
        let addr = serve(body).await;
        let provider = keyed(
            "tavily",
            "KA_TEST_TAVILY_KEY",
            Some(format!("http://{addr}")),
        );
        let hits = provider_search_with(&provider, "rust async", "test-key")
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "R1");
        assert_eq!(hits[0].url, "https://a");
        assert_eq!(hits[1].snippet, "snippet b");
    }

    #[tokio::test]
    async fn brave_provider_maps_hits() {
        let body = r#"{"web":{"results":[
            {"title":"B1","url":"https://b1","description":"d1"}]}}"#;
        let addr = serve(body).await;
        let provider = keyed("brave", "KA_TEST_BRAVE_KEY", Some(format!("http://{addr}")));
        let hits = provider_search_with(&provider, "q", "test-key")
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "B1");
        assert_eq!(hits[0].snippet, "d1");
    }

    #[tokio::test]
    async fn bing_provider_maps_hits() {
        let body = r#"{"webPages":{"value":[
            {"name":"N1","url":"https://n1","snippet":"s1"}]}}"#;
        let addr = serve(body).await;
        let provider = keyed("bing", "KA_TEST_BING_KEY", Some(format!("http://{addr}")));
        let hits = provider_search_with(&provider, "q", "test-key")
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "N1");
        assert_eq!(hits[0].snippet, "s1");
    }

    #[tokio::test]
    async fn keyed_provider_without_api_key_env_is_chain_failure() {
        // config regression guard: api_key_env used to be mandatory;
        // now an omitted key fails just that provider, with a clear
        // message, instead of failing the TOML parse
        let provider = SearchProvider {
            provider: "tavily".into(),
            api_key_env: None,
            base_url: None,
            sites: Vec::new(),
        };
        let err = provider_search(&provider, "q").await.unwrap_err();
        assert!(err.contains("api_key_env"), "{err}");
    }

    // ------------------------------------------------ duckduckgo html

    /// A realistic DDG html-endpoint page: title anchors whose hrefs go
    /// through DDG's redirector, snippets with entities.
    const DDG_HTML: &str = r#"<html><body>
<div class="result">
  <h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Fka&amp;rut=abc">Ka docs &amp; guide</a></h2>
  <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Fka">The &#39;ka&#39; crate &lt;guide&gt;</a>
</div>
<div class="result">
  <h2 class="result__title"><a rel="nofollow" class="result__a" href="https://direct.example.com/page">Direct link</a></h2>
  <a class="result__snippet" href="https://direct.example.com/page">plain snippet</a>
</div>
<div class="result result--ad">
  <h2><a class="result__a" href="https://duckduckgo.com/y.js">An ad</a></h2>
</div>
</body></html>"#;

    #[tokio::test]
    async fn duckduckgo_html_fixture_parses_hits_and_decodes_redirects() {
        let (addr, _seen) = serve_status("200 OK", "text/html", DDG_HTML).await;
        let provider = SearchProvider {
            provider: "duckduckgo".into(),
            api_key_env: None,
            base_url: Some(format!("http://{addr}")),
            sites: Vec::new(),
        };
        let hits = provider_search(&provider, "ka harness").await.unwrap();
        assert_eq!(hits.len(), 2, "ad block has no snippet pair: {hits:?}");
        assert_eq!(hits[0].title, "Ka docs & guide");
        assert_eq!(hits[0].url, "https://docs.rs/ka", "uddg redirect decoded");
        assert_eq!(hits[0].snippet, "The 'ka' crate <guide>");
        assert_eq!(hits[1].url, "https://direct.example.com/page");
    }

    #[test]
    fn ddg_parser_ignores_pages_without_results() {
        assert!(parse_ddg_html("<html><body>nothing here</body></html>").is_empty());
        assert!(parse_ddg_html("").is_empty());
    }

    #[test]
    fn percent_decode_roundtrip_matches_urlencode() {
        assert_eq!(
            percent_decode("https%3A%2F%2Fdocs.rs%2Fka"),
            "https://docs.rs/ka"
        );
        assert_eq!(percent_decode("a%2b"), "a+");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("truncated%2"), "truncated%2");
        let q = "héllo wörld & more";
        assert_eq!(percent_decode(&urlencode(q)), q);
    }

    #[test]
    fn decode_entities_handles_named_decimal_and_hex() {
        assert_eq!(decode_entities("a&amp;b"), "a&b");
        assert_eq!(decode_entities("&#39;"), "'");
        assert_eq!(decode_entities("&#x27;"), "'");
        assert_eq!(decode_entities("no entities"), "no entities");
        assert_eq!(
            decode_entities("unknown &woff; stays"),
            "unknown &woff; stays"
        );
    }

    // ------------------------------------------------------ searxng

    #[tokio::test]
    async fn searxng_json_fixture_maps_hits() {
        let body = r#"{"results":[
            {"title":"S1","url":"https://s1","content":"content s1"},
            {"title":"S2","url":"https://s2","content":"content s2"}]}"#;
        let (addr, _seen) = serve_status("200 OK", "application/json", body).await;
        let provider = SearchProvider {
            provider: "searxng".into(),
            api_key_env: None,
            base_url: Some(format!("http://{addr}")),
            sites: Vec::new(),
        };
        let hits = provider_search(&provider, "rust").await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "S1");
        assert_eq!(hits[0].snippet, "content s1");
    }

    #[tokio::test]
    async fn searxng_requires_instance_base_url() {
        let provider = SearchProvider {
            provider: "searxng".into(),
            api_key_env: None,
            base_url: None,
            sites: Vec::new(),
        };
        let err = provider_search(&provider, "q").await.unwrap_err();
        assert!(err.contains("base_url"), "{err}");

        let provider = SearchProvider {
            provider: "searxng".into(),
            api_key_env: None,
            base_url: Some("   ".into()),
            sites: Vec::new(),
        };
        assert!(
            provider_search(&provider, "q")
                .await
                .unwrap_err()
                .contains("base_url")
        );
    }

    // ------------------------------------------ site scoping + chain

    #[tokio::test]
    async fn site_scoping_appends_site_operator_to_the_query() {
        const EMPTY: &str = r#"{"results":[]}"#;
        let (addr, seen) = serve_status("200 OK", "application/json", EMPTY).await;
        let provider = SearchProvider {
            provider: "searxng".into(),
            api_key_env: None,
            base_url: Some(format!("http://{addr}")),
            sites: vec![
                SearchSite {
                    site: "docs.rs".into(),
                    handler: None,
                },
                SearchSite {
                    site: "github.com".into(),
                    handler: Some("auto".into()),
                },
            ],
        };
        let hits = chained_search(&[provider], "flate2 gzip").await.unwrap();
        assert!(hits.is_empty(), "(no results) is a valid success");
        let request = seen.lock().clone();
        assert!(
            request.contains("q=flate2%20gzip%20site%3Adocs.rs%20site%3Agithub.com"),
            "both site: operators must ride the query: {request}"
        );
    }

    #[tokio::test]
    async fn unknown_site_handler_fails_the_provider_not_the_parse() {
        let provider = SearchProvider {
            provider: "searxng".into(),
            api_key_env: None,
            base_url: Some("http://127.0.0.1:9".into()),
            sites: vec![SearchSite {
                site: "docs.rs".into(),
                handler: Some("scrape".into()),
            }],
        };
        let err = chained_search(&[provider], "q").await.unwrap_err();
        assert!(err.contains("handler"), "{err}");
        assert!(err.contains("all configured providers failed"), "{err}");
    }

    #[tokio::test]
    async fn fallback_chain_falls_through_to_the_next_provider() {
        // first backend: server error
        let (bad, _seen) =
            serve_status("500 Internal Server Error", "application/json", "{}").await;
        let failing = keyed(
            "tavily",
            "KA_TEST_TAVILY_KEY",
            Some(format!("http://{bad}")),
        );
        // second backend: a working DDG fixture
        let (good, _seen) = serve_status("200 OK", "text/html", DDG_HTML).await;
        let working = SearchProvider {
            provider: "duckduckgo".into(),
            api_key_env: None,
            base_url: Some(format!("http://{good}")),
            sites: Vec::new(),
        };
        let hits = chained_search(&[failing, working], "ka").await.unwrap();
        assert_eq!(hits.len(), 2, "second provider must serve the hits");
        assert_eq!(hits[0].title, "Ka docs & guide");
    }

    #[tokio::test]
    async fn chain_reports_every_failure_when_all_backends_fail() {
        // keyless backend with a broken instance: reaches the fixture,
        // gets a 500
        let (bad, _s) = serve_status("500 Internal Server Error", "application/json", "{}").await;
        let failing = SearchProvider {
            provider: "searxng".into(),
            api_key_env: None,
            base_url: Some(format!("http://{bad}")),
            sites: Vec::new(),
        };
        // missing required instance URL
        let unkeyed = SearchProvider {
            provider: "searxng".into(),
            api_key_env: None,
            base_url: None,
            sites: Vec::new(),
        };
        let unknown = keyed("megaquery", "KA_TEST_X", None);
        let err = chained_search(&[failing, unkeyed, unknown], "q")
            .await
            .unwrap_err();
        assert!(err.contains("searxng") && err.contains("500"), "{err}");
        assert!(err.contains("searxng: base_url is required"), "{err}");
        assert!(err.contains("unknown provider"), "{err}");
    }

    #[tokio::test]
    async fn web_search_hand_executes_through_the_chain() {
        let (good, _s) = serve_status("200 OK", "text/html", DDG_HTML).await;
        let hand = WebSearchHand::new(vec![SearchProvider {
            provider: "duckduckgo".into(),
            api_key_env: None,
            base_url: Some(format!("http://{good}")),
            sites: Vec::new(),
        }]);
        let ctx = test_ctx();
        let out = hand.execute(&json!({"query": "ka harness"}), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("https://docs.rs/ka"),
            "{}",
            out.content
        );

        let missing = hand.execute(&json!({}), &ctx).await;
        assert!(missing.is_error);
        assert!(missing.content.contains("missing required 'query'"));
    }

    #[test]
    fn hands_bootstrap_needs_at_least_one_provider_entry() {
        assert_eq!(
            hands(Vec::new(), false)
                .iter()
                .map(|h| h.def().name)
                .collect::<Vec<_>>(),
            vec!["web_fetch"]
        );
        let names = hands(vec![SearchProvider::default()], false)
            .iter()
            .map(|h| h.def().name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["web_search", "web_fetch"]);
    }

    #[tokio::test]
    async fn web_fetch_refuses_private_hosts_unless_allowed() {
        let hand = WebFetchHand::new(false);
        let out = hand
            .execute(&json!({"url": "http://127.0.0.1:1/x"}), &test_ctx())
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("private"), "{}", out.content);

        // allowed: fetches through the loopback fixture
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut sock, _) = listener.accept().await.unwrap();
            let html = "<html><body><h1>Hi</h1><p>Body text</p></body></html>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                html.len()
            );
            sock.write_all(resp.as_bytes()).await.ok();
        });
        let hand = WebFetchHand::new(true);
        let out = hand
            .execute(&json!({"url": format!("http://{addr}/page")}), &test_ctx())
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("Hi"), "{}", out.content);
        assert!(out.content.contains("Body text"), "{}", out.content);
    }

    fn test_ctx() -> HandContext {
        HandContext {
            cwd: std::env::temp_dir(),
            ledger: std::sync::Arc::new(parking_lot::Mutex::new(crate::hands::Ledger::default())),
            spill: std::sync::Arc::new(crate::hands::Spill::new()),
            snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
            jobs: std::sync::Arc::new(crate::hands::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
        }
    }
}
