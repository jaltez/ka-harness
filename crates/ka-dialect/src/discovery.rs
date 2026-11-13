//! Local-endpoint discovery: probe `/v1/models` on local OpenAI-compatible
//! servers and turn findings into ephemeral dialect rows.

use crate::dialects::{Dialect, Discovery, Flags, Modality, Wire};

/// A discovered local model.
#[derive(Debug, Clone)]
pub struct FoundModel {
    /// `vendor/model` id.
    pub model_id: String,
    /// Dialect row for it.
    pub dialect: Dialect,
}

/// Probe an OpenAI-compatible `/v1/models` endpoint and build dialect rows.
/// Capabilities are sniffed per model from detail endpoints (Ollama
/// `/api/show`, else `/v1/models/{id}`), falling back to family
/// heuristics, then to conservative defaults (context 32768, no vision,
/// no tools). No LLM inference calls are made.
pub async fn discover_openai_compatible(
    client: &reqwest::Client,
    vendor: &str,
    base_url: &str,
    discovery: Discovery,
) -> Vec<FoundModel> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let Ok(resp) = client.get(&url).timeout(PROBE_TIMEOUT).send().await else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(text) = resp.text().await else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(ids) = v.get("data").and_then(|d: &serde_json::Value| d.as_array()) else {
        return Vec::new();
    };
    let probes: Vec<_> = ids
        .iter()
        .filter_map(|entry| entry.get("id").and_then(|i: &serde_json::Value| i.as_str()))
        .take(MAX_PROBE_FANOUT)
        .map(|id| {
            let sniffed = sniff_caps(client, base_url, discovery, id);
            async move {
                let sniffed = sniffed.await;
                let (context, vision, tools) = resolve_caps(id, sniffed);
                FoundModel {
                    model_id: format!("{vendor}/{id}"),
                    dialect: Dialect {
                        wire: Wire::OpenaiChat,
                        base_url: Some(base_url.to_string()),
                        discovery: Some(discovery),
                        first_byte_timeout_ms: 0,
                        context,
                        input: if vision {
                            vec![Modality::Text, Modality::Image]
                        } else {
                            vec![Modality::Text]
                        },
                        flags: Flags {
                            tools,
                            ..Flags::default()
                        },
                        ..Dialect::default_for_discovery()
                    },
                }
            }
        })
        .collect();
    futures_util::future::join_all(probes).await
}

/// Per-probe timeout: local endpoints must answer fast or be skipped
/// (discovery runs at CLI startup against a ~50ms cold-start budget).
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Cap on concurrent detail probes per endpoint: giant local model
/// lists (100+ ids) must not fan out into hundreds of simultaneous
/// requests. The `/v1/models` list still enumerates every id.
const MAX_PROBE_FANOUT: usize = 64;

/// Default context assumed when nothing sniffs or heuristically matches.
const DEFAULT_CONTEXT: u32 = 32_768;

/// Capabilities gathered from a detail endpoint; `None` fields fall
/// through to family heuristics.
#[derive(Debug, Clone, Copy, Default)]
struct SniffedCaps {
    context: Option<u32>,
    vision: Option<bool>,
    tools: Option<bool>,
}

/// Probe a model's detail endpoint and sniff capabilities.
async fn sniff_caps(
    client: &reqwest::Client,
    base_url: &str,
    discovery: Discovery,
    id: &str,
) -> SniffedCaps {
    match discovery {
        Discovery::Ollama => sniff_ollama_show(client, base_url, id).await,
        Discovery::LmStudio | Discovery::Vllm => sniff_detail(client, base_url, id).await,
    }
}

/// Ollama: `POST {root}/api/show` carries `model_info` context length and
/// a `capabilities` list (`completion`, `tools`, `vision`, …).
async fn sniff_ollama_show(client: &reqwest::Client, base_url: &str, id: &str) -> SniffedCaps {
    let root = base_url.trim_end_matches('/').trim_end_matches("/v1");
    let url = format!("{root}/api/show");
    let Ok(body) = serde_json::to_vec(&serde_json::json!({ "model": id })) else {
        return SniffedCaps::default();
    };
    let Ok(resp) = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(body)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
    else {
        return SniffedCaps::default();
    };
    if !resp.status().is_success() {
        return SniffedCaps::default();
    }
    let Ok(text) = resp.text().await else {
        return SniffedCaps::default();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return SniffedCaps::default();
    };
    parse_ollama_show(&v)
}

/// Parse an Ollama `/api/show` response.
fn parse_ollama_show(v: &serde_json::Value) -> SniffedCaps {
    let mut caps = SniffedCaps::default();
    if let Some(info) = v.get("model_info").and_then(|m| m.as_object()) {
        caps.context = info.iter().find_map(|(k, val)| {
            if k.ends_with("context_length") {
                val.as_u64().map(|n| n.min(u32::MAX as u64) as u32)
            } else {
                None
            }
        });
    }
    if let Some(has) = capabilities_of(v.get("capabilities")) {
        caps.vision = Some(has("vision"));
        caps.tools = Some(has("tools"));
    }
    caps
}

/// Build a membership check over a JSON array of capability strings.
fn capabilities_of(v: Option<&serde_json::Value>) -> Option<impl Fn(&str) -> bool + '_> {
    let list = v?.as_array()?;
    Some(move |needle: &str| {
        list.iter()
            .any(|s| s.as_str().is_some_and(|s| s.eq_ignore_ascii_case(needle)))
    })
}

/// Generic OpenAI-style detail: `GET {base}/models/{id}` with
/// `context_length`/`context_window`, `supports_tools`, `supports_vision`
/// (top level or nested under `data`) plus an optional `capabilities`
/// array.
async fn sniff_detail(client: &reqwest::Client, base_url: &str, id: &str) -> SniffedCaps {
    let url = format!("{}/models/{}", base_url.trim_end_matches('/'), id);
    let Ok(resp) = client.get(&url).timeout(PROBE_TIMEOUT).send().await else {
        return SniffedCaps::default();
    };
    if !resp.status().is_success() {
        return SniffedCaps::default();
    }
    let Ok(text) = resp.text().await else {
        return SniffedCaps::default();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return SniffedCaps::default();
    };
    parse_detail(&v)
}

/// Parse a generic `/v1/models/{id}` detail response.
fn parse_detail(v: &serde_json::Value) -> SniffedCaps {
    let mut caps = SniffedCaps::default();
    let obj = v.get("data").filter(|d| d.is_object()).unwrap_or(v);
    caps.context = ["context_length", "context_window"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(serde_json::Value::as_u64))
        .map(|n| n.min(u32::MAX as u64) as u32);
    caps.tools = obj
        .get("supports_tools")
        .and_then(serde_json::Value::as_bool);
    caps.vision = obj
        .get("supports_vision")
        .and_then(serde_json::Value::as_bool);
    if caps.vision.is_none() || caps.tools.is_none() {
        if let Some(has) = capabilities_of(obj.get("capabilities")) {
            if caps.vision.is_none() {
                caps.vision = Some(has("vision"));
            }
            if caps.tools.is_none() {
                caps.tools = Some(has("tools"));
            }
        }
    }
    caps
}

/// Family heuristics for models whose detail endpoint told us nothing:
/// `*-vl*`/`*vision*` names take image input; common tool-tuned families
/// accept tools. Everything else stays conservative.
fn heuristic_caps(id: &str) -> SniffedCaps {
    let lower = id.to_ascii_lowercase();
    let vision = Some(lower.contains("-vl") || lower.contains("vision"));
    let tools = Some(
        lower.starts_with("llama3.")
            || lower.starts_with("qwen2.5")
            || lower.starts_with("mistral")
            || lower.starts_with("gpt"),
    );
    SniffedCaps {
        context: None,
        vision,
        tools,
    }
}

/// Merge sniffs: detail endpoint wins per field, heuristics fill the
/// gaps, conservative defaults last.
fn resolve_caps(id: &str, sniffed: SniffedCaps) -> (u32, bool, bool) {
    let heuristic = heuristic_caps(id);
    (
        sniffed
            .context
            .or(heuristic.context)
            .unwrap_or(DEFAULT_CONTEXT),
        sniffed.vision.or(heuristic.vision).unwrap_or(false),
        sniffed.tools.or(heuristic.tools).unwrap_or(false),
    )
}

/// Probe default local endpoints (Ollama, LM Studio) and insert findings
/// into the catalog. Explicit catalog rows always win over discovered ones.
pub async fn overlay_discovered(catalog: &mut crate::dialects::Catalog) {
    let client = reqwest::Client::new();
    // independent endpoints: probe both in parallel so one dead port
    // cannot add its full timeout to cold start
    let (ollama, lmstudio) = tokio::join!(discover_ollama(&client), discover_lmstudio(&client));
    for f in ollama.into_iter().chain(lmstudio) {
        catalog.dialects.entry(f.model_id).or_insert(f.dialect);
    }
}

/// Probe a local Ollama server on the default port.
pub async fn discover_ollama(client: &reqwest::Client) -> Vec<FoundModel> {
    discover_openai_compatible(
        client,
        "ollama",
        "http://127.0.0.1:11434/v1",
        Discovery::Ollama,
    )
    .await
}

/// Probe a local LM Studio server on the default port.
pub async fn discover_lmstudio(client: &reqwest::Client) -> Vec<FoundModel> {
    discover_openai_compatible(
        client,
        "lmstudio",
        "http://127.0.0.1:1234/v1",
        Discovery::LmStudio,
    )
    .await
}

impl Dialect {
    /// Defaults for discovered rows: cheap, safe, unknown-context.
    pub fn default_for_discovery() -> Self {
        Self {
            wire: Wire::OpenaiChat,
            base_url: None,
            doc_url: None,
            api_key_env: None,
            wire_model: None,
            discovery: None,
            context: 0,
            max_output: 8_192,
            efforts: Vec::new(),
            input: Vec::new(),
            cache: crate::dialects::Cache::Off,
            ratio: 4.0,
            first_byte_timeout_ms: 0,
            effort_budgets: Default::default(),
            price: Default::default(),
            priced: false,
            flags: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[tokio::test]
    async fn discovers_models_from_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let body = r#"{"data":[{"id":"qwen3:8b"},{"id":"llama4:latest"}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        });
        let client = reqwest::Client::new();
        let found = discover_openai_compatible(
            &client,
            "ollama",
            &format!("http://{addr}/v1"),
            Discovery::Ollama,
        )
        .await;
        let ids: Vec<&str> = found.iter().map(|f| f.model_id.as_str()).collect();
        assert!(ids.contains(&"ollama/qwen3:8b"), "got: {ids:?}");
        assert!(ids.contains(&"ollama/llama4:latest"), "got: {ids:?}");
        let d = &found[0].dialect;
        assert_eq!(d.wire, Wire::OpenaiChat);
        assert_eq!(d.first_byte_timeout_ms, 0);
        assert_eq!(
            d.base_url.as_deref(),
            Some(format!("http://{addr}/v1").as_str())
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn dead_endpoint_is_empty() {
        let client = reqwest::Client::new();
        let found =
            discover_openai_compatible(&client, "x", "http://127.0.0.1:1/v1", Discovery::Ollama)
                .await;
        assert!(found.is_empty());
    }

    /// Serve one JSON response per accepted connection, draining the
    /// request fully (headers + Content-Length body) first.
    async fn serve_json(listener: &tokio::net::TcpListener, body: &str) {
        let (sock, _) = listener.accept().await.unwrap();
        serve_json_on(sock, body).await;
    }

    /// Drain one request from an accepted socket, then answer with one
    /// JSON response.
    async fn serve_json_on(mut sock: tokio::net::TcpStream, body: &str) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 8192];
        let mut got = 0;
        loop {
            let n = sock.read(&mut buf[got..]).await.unwrap();
            if n == 0 {
                break;
            }
            got += n;
            let head_end = buf[..got].windows(4).position(|w| w == b"\r\n\r\n");
            if let Some(h) = head_end {
                let cl = buf[..h]
                    .split(|&b| b == b'\n')
                    .find_map(|line| {
                        let line = std::str::from_utf8(line).ok()?;
                        let rest = line.strip_prefix("Content-Length:")?;
                        rest.trim().parse::<usize>().ok()
                    })
                    .unwrap_or(0);
                if got >= h + 4 + cl {
                    break;
                }
            }
        }
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
    }

    #[test]
    fn heuristics_map_families() {
        let v = heuristic_caps("qwen2.5-vl:7b");
        assert_eq!(v.vision, Some(true));
        assert_eq!(v.tools, Some(true));
        let v = heuristic_caps("llama3.2-vision:latest");
        assert_eq!(v.vision, Some(true));
        assert_eq!(v.tools, Some(true));
        let v = heuristic_caps("gpt-4o-mini");
        assert_eq!(v.vision, Some(false));
        assert_eq!(v.tools, Some(true));
        let v = heuristic_caps("mistral-small");
        assert_eq!(v.tools, Some(true));
        let v = heuristic_caps("mystery:12b");
        assert_eq!(v.vision, Some(false));
        assert_eq!(v.tools, Some(false));
    }

    #[test]
    fn parse_ollama_show_reads_caps() {
        let c = parse_ollama_show(&serde_json::json!({
            "model_info": {"qwen3.context_length": 65536},
            "capabilities": ["completion", "tools", "vision"]
        }));
        assert_eq!(c.context, Some(65536));
        assert_eq!(c.vision, Some(true));
        assert_eq!(c.tools, Some(true));
        let c =
            parse_ollama_show(&serde_json::json!({"model_info": {"llama.context_length": 4096}}));
        assert_eq!(c.context, Some(4096));
        assert_eq!(c.vision, None);
        assert_eq!(c.tools, None);
    }

    #[test]
    fn parse_detail_reads_caps() {
        let c = parse_detail(
            &serde_json::json!({"id": "m", "context_window": 4096, "supports_tools": true}),
        );
        assert_eq!(c.context, Some(4096));
        assert_eq!(c.tools, Some(true));
        assert_eq!(c.vision, None);
        let c = parse_detail(&serde_json::json!({
            "data": {"context_length": 1024, "capabilities": ["vision"]}
        }));
        assert_eq!(c.context, Some(1024));
        assert_eq!(c.vision, Some(true));
        assert_eq!(c.tools, Some(false));
    }

    #[test]
    fn resolve_caps_falls_back_to_defaults() {
        let (ctx, vis, tools) = resolve_caps("mystery:1b", SniffedCaps::default());
        assert_eq!((ctx, vis, tools), (32_768, false, false));
    }

    #[tokio::test]
    async fn ollama_show_caps_fill_dialect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            serve_json(&listener, r#"{"data":[{"id":"custom-owl:3b"}]}"#).await;
            serve_json(
                &listener,
                r#"{"model_info":{"custom.context_length":8192},"capabilities":["completion","tools","vision"]}"#,
            )
            .await;
        });
        let client = reqwest::Client::new();
        let found = discover_openai_compatible(
            &client,
            "ollama",
            &format!("http://{addr}/v1"),
            Discovery::Ollama,
        )
        .await;
        let f = found
            .iter()
            .find(|f| f.model_id == "ollama/custom-owl:3b")
            .unwrap();
        assert_eq!(f.dialect.context, 8192);
        assert!(f.dialect.input.contains(&Modality::Image));
        assert!(f.dialect.flags.tools);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn detail_endpoint_caps_fill_dialect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            serve_json(&listener, r#"{"data":[{"id":"hf-owl"}]}"#).await;
            serve_json(
                &listener,
                r#"{"data":{"context_window":4096,"supports_vision":true}}"#,
            )
            .await;
        });
        let client = reqwest::Client::new();
        let found = discover_openai_compatible(
            &client,
            "lmstudio",
            &format!("http://{addr}/v1"),
            Discovery::LmStudio,
        )
        .await;
        let f = found
            .iter()
            .find(|f| f.model_id == "lmstudio/hf-owl")
            .unwrap();
        assert_eq!(f.dialect.context, 4096);
        assert!(f.dialect.input.contains(&Modality::Image));
        assert!(!f.dialect.flags.tools);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn probe_fanout_is_capped() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let list = format!(
            r#"{{"data":[{}]}}"#,
            (0..100)
                .map(|i| format!(r#"{{"id":"m{i}"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let server = tokio::spawn(async move {
            serve_json(&listener, &list).await;
            let mut detail_hits = 0usize;
            while let Ok(Ok((sock, _))) =
                tokio::time::timeout(std::time::Duration::from_millis(750), listener.accept()).await
            {
                serve_json_on(sock, r#"{"data":{"context_length":1024}}"#).await;
                detail_hits += 1;
            }
            detail_hits
        });
        let client = reqwest::Client::new();
        let found = discover_openai_compatible(
            &client,
            "lmstudio",
            &format!("http://{addr}/v1"),
            Discovery::LmStudio,
        )
        .await;
        assert_eq!(found.len(), 64, "only the first 64 ids are probed");
        let hits = server.await.unwrap();
        assert!(hits <= 64, "detail probes must be capped, got {hits}");
    }

    #[tokio::test]
    async fn parallel_discovery_bounds_dead_endpoint() {
        // worst case: the ollama port accepts and never answers. Hold a
        // bound-but-silent listener there when the port is free; if a
        // real ollama answers instead, discovery is live-fast anyway.
        let _dead = tokio::net::TcpListener::bind("127.0.0.1:11434").await.ok();
        let start = std::time::Instant::now();
        let mut catalog = crate::dialects::Catalog::default();
        overlay_discovered(&mut catalog).await;
        assert!(
            start.elapsed() < std::time::Duration::from_millis(1500),
            "dead-endpoint probes must run in parallel within one probe timeout (took {:?})",
            start.elapsed()
        );
    }
}
