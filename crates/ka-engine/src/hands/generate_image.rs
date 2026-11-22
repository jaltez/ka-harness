//! The `generate_image` hand: text-to-image through the configured
//! REST provider (`gemini`, `openai`, or `grok`), saved under
//! `[generate_image] output_dir` (default `<cwd>/.ka/images`) and
//! returned as an image part inline. GATED: the hand registers only
//! when the config names a provider AND its API key resolves (env var,
//! dotenv, or keyring through the shared auth layer) — an unkeyed
//! build never shows the tool.
//!
//! The HTTP shapes are isolated in pure request-builder/parse
//! functions ([`build_request`], [`parse_response`]) so providers can
//! be tested against fixtures without network.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use super::{Clearance, Hand, HandContext, HandDef, ToolOutput};

/// Supported image providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProvider {
    /// Google Gemini (`generativelanguage.googleapis.com`).
    Gemini,
    /// OpenAI images API (`api.openai.com`).
    OpenAI,
    /// xAI Grok images API (`api.x.ai`).
    Grok,
}

impl ImageProvider {
    /// Parse the `[generate_image] provider` string.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "gemini" => Some(Self::Gemini),
            "openai" => Some(Self::OpenAI),
            "grok" => Some(Self::Grok),
            _ => None,
        }
    }

    /// Canonical name (errors and output name it).
    pub fn name(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::OpenAI => "openai",
            Self::Grok => "grok",
        }
    }

    /// Default model per provider.
    pub fn default_model(self) -> &'static str {
        match self {
            Self::Gemini => "gemini-2.5-flash-image",
            Self::OpenAI => "gpt-image-1",
            Self::Grok => "grok-2-image",
        }
    }

    /// API base per provider.
    pub fn default_base(self) -> &'static str {
        match self {
            Self::Gemini => "https://generativelanguage.googleapis.com",
            Self::OpenAI => "https://api.openai.com",
            Self::Grok => "https://api.x.ai",
        }
    }

    /// API-key env ladder per provider (first present wins).
    pub fn key_envs(self) -> &'static [&'static str] {
        match self {
            Self::Gemini => &["GEMINI_API_KEY"],
            Self::OpenAI => &["OPENAI_API_KEY"],
            Self::Grok => &["GROK_API_KEY", "XAI_API_KEY"],
        }
    }
}

/// Fully-resolved generation config (engine bootstrap).
#[derive(Debug, Clone)]
pub struct ResolvedImageGen {
    pub provider: ImageProvider,
    /// API key resolved at bootstrap (env/dotenv/keyring ladder).
    pub api_key: String,
    pub model: String,
    /// Where saved images land.
    pub output_dir: PathBuf,
    /// API base override (fixture tests, proxies).
    pub base_url: Option<String>,
}

/// The gating + defaulting core behind `[generate_image]`, with the
/// key lookup injected (production passes the env/dotenv/keyring
/// ladder; tests pass a map). `None` = the hand stays unregistered:
/// unsupported provider, or no key resolvable anywhere on the ladder.
pub fn resolve(
    provider: Option<&str>,
    api_key_env: Option<&str>,
    model: Option<String>,
    output_dir: Option<String>,
    base_url: Option<String>,
    lookup: impl Fn(&str) -> Option<String>,
    cwd: &std::path::Path,
) -> Option<ResolvedImageGen> {
    let provider = ImageProvider::parse(provider?)?;
    let key = match api_key_env {
        Some(env) => lookup(env),
        None => provider.key_envs().iter().find_map(|env| lookup(env)),
    }?;
    Some(ResolvedImageGen {
        provider,
        api_key: key,
        model: model.unwrap_or_else(|| provider.default_model().to_string()),
        output_dir: match output_dir {
            Some(dir) => PathBuf::from(dir),
            None => cwd.join(".ka").join("images"),
        },
        base_url,
    })
}

/// One built REST request (pure — testable without network).
#[derive(Debug, Clone, PartialEq)]
pub struct ImageApiRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

/// What a provider response decoded to.
#[derive(Debug, Clone, PartialEq)]
pub enum ImageResult {
    /// Inline base64 (Gemini always; OpenAI/Grok with b64_json).
    Inline {
        /// Base64 of the image bytes (kept verbatim for the ImagePart).
        base64: String,
        /// Decoded bytes (saved to disk).
        bytes: Vec<u8>,
        media_type: String,
    },
    /// A URL to download (OpenAI without b64_json).
    Url(String),
}

/// Build the provider request for one prompt. Pure.
pub fn build_request(cfg: &ResolvedImageGen, prompt: &str) -> ImageApiRequest {
    let base = cfg
        .base_url
        .clone()
        .unwrap_or_else(|| cfg.provider.default_base().to_string());
    match cfg.provider {
        ImageProvider::Gemini => ImageApiRequest {
            url: format!("{base}/v1beta/models/{}:generateContent", cfg.model),
            headers: vec![("x-goog-api-key".into(), cfg.api_key.clone())],
            body: json!({
                "contents": [{"parts": [{"text": prompt}]}],
            }),
        },
        ImageProvider::OpenAI => ImageApiRequest {
            url: format!("{base}/v1/images/generations"),
            headers: vec![("Authorization".into(), format!("Bearer {}", cfg.api_key))],
            body: json!({
                "model": cfg.model,
                "prompt": prompt,
                "n": 1,
                "size": "1024x1024",
            }),
        },
        ImageProvider::Grok => ImageApiRequest {
            url: format!("{base}/v1/images/generations"),
            headers: vec![("Authorization".into(), format!("Bearer {}", cfg.api_key))],
            body: json!({
                "model": cfg.model,
                "prompt": prompt,
                "n": 1,
                "response_format": "b64_json",
            }),
        },
    }
}

/// Decode one provider response body into image bytes (or a download
/// URL). Pure; `provider` picks the response shape.
pub fn parse_response(provider: ImageProvider, body: &Value) -> Result<ImageResult, String> {
    let err_text = || {
        body["error"]["message"]
            .as_str()
            .unwrap_or("unknown provider error")
            .to_string()
    };
    match provider {
        ImageProvider::Gemini => {
            if body.get("error").is_some() {
                return Err(err_text());
            }
            let parts = body["candidates"][0]["content"]["parts"]
                .as_array()
                .ok_or_else(|| "gemini: no candidates in response".to_string())?;
            for part in parts {
                if let Some(data) = part["inlineData"]["data"].as_str() {
                    let media_type = part["inlineData"]["mimeType"]
                        .as_str()
                        .unwrap_or("image/png")
                        .to_string();
                    let bytes = base64_decode(data)
                        .ok_or_else(|| "gemini: inline image is not valid base64".to_string())?;
                    return Ok(ImageResult::Inline {
                        base64: data.to_string(),
                        bytes,
                        media_type,
                    });
                }
            }
            Err("gemini: response carried no inline image".to_string())
        }
        ImageProvider::OpenAI | ImageProvider::Grok => {
            if body.get("error").is_some() {
                return Err(err_text());
            }
            let first = body["data"][0]
                .as_object()
                .ok_or_else(|| "no images in response".to_string())?;
            if let Some(b64) = first.get("b64_json").and_then(Value::as_str) {
                let bytes = base64_decode(b64)
                    .ok_or_else(|| "provider image is not valid base64".to_string())?;
                return Ok(ImageResult::Inline {
                    base64: b64.to_string(),
                    bytes,
                    media_type: "image/png".to_string(),
                });
            }
            if let Some(url) = first.get("url").and_then(Value::as_str) {
                return Ok(ImageResult::Url(url.to_string()));
            }
            Err("response carried neither b64_json nor url".to_string())
        }
    }
}

/// File extension for a media type (falls back to png).
fn ext_for(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

/// Whether `stem` already carries a known image extension.
fn has_image_ext(name: &str) -> bool {
    std::path::Path::new(name).extension().is_some_and(|e| {
        ["png", "jpg", "jpeg", "webp", "gif"]
            .contains(&e.to_string_lossy().to_ascii_lowercase().as_str())
    })
}

/// Sanitize a caller-supplied file name: keep the last path component,
/// no separators, no traversal; a known image extension is kept
/// verbatim, anything else gets the media type's extension appended.
fn sanitize_file_name(name: &str, media_type: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let stem: String = last
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = stem.trim_matches('.').trim();
    let stem = if stem.is_empty() { "image" } else { stem };
    if has_image_ext(stem) {
        stem.to_string()
    } else {
        format!("{stem}.{}", ext_for(media_type))
    }
}

/// Session-unique counter for default file names.
static IMAGE_SEQ: AtomicU64 = AtomicU64::new(0);

/// The generate_image tool.
pub struct GenerateImageHand {
    cfg: ResolvedImageGen,
}

impl GenerateImageHand {
    /// Construct with the bootstrap-resolved config (the hand is only
    /// registered when this exists — see engine registration).
    pub fn new(cfg: ResolvedImageGen) -> Self {
        Self { cfg }
    }
}

impl Hand for GenerateImageHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "generate_image".to_string(),
            description: format!(
                "Generate an image from a text prompt via the configured {} provider \
                (model {}) and save it under {}. Arguments: {{\"prompt\": \"a red barn at \
                dusk\", \"file_name\": \"barn.png\"}} (file_name optional; png/jpg/webp \
                extensions honored). Returns the saved path and the image inline.",
                self.cfg.provider.name(),
                self.cfg.model,
                self.cfg.output_dir.display(),
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "What the image should show"
                    },
                    "file_name": {
                        "type": "string",
                        "description": "Output file name (optional; extension may be corrected)"
                    }
                },
                "required": ["prompt"]
            }),
            clearance: Clearance::Write,
            read_only: false,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _ctx: &'a HandContext,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let Some(prompt) = args.get("prompt").and_then(Value::as_str) else {
                return ToolOutput::err("generate_image: missing required 'prompt'");
            };
            if prompt.trim().is_empty() {
                return ToolOutput::err("generate_image: 'prompt' must not be empty");
            }
            self.generate(prompt, args.get("file_name").and_then(Value::as_str))
                .await
        })
    }
}

impl GenerateImageHand {
    /// One generation: build → send → parse → (download) → save.
    async fn generate(&self, prompt: &str, file_name: Option<&str>) -> ToolOutput {
        let req = build_request(&self.cfg, prompt);
        let client = reqwest::Client::new();
        let mut call = client.post(&req.url);
        for (k, v) in &req.headers {
            call = call.header(k, v);
        }
        let body: Value = match call.json(&req.body).send().await {
            Ok(resp) => match resp.error_for_status() {
                Ok(resp) => match resp.json().await {
                    Ok(v) => v,
                    Err(e) => return ToolOutput::err(format!("generate_image: {e}")),
                },
                Err(e) => return ToolOutput::err(format!("generate_image: {e}")),
            },
            Err(e) => return ToolOutput::err(format!("generate_image: {e}")),
        };
        let parsed = match parse_response(self.cfg.provider, &body) {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(format!("generate_image: {e}")),
        };
        let (bytes, media_type, base64) = match parsed {
            ImageResult::Inline {
                base64,
                bytes,
                media_type,
            } => (bytes, media_type, base64),
            ImageResult::Url(url) => {
                let resp = match client.get(&url).send().await {
                    Ok(r) => r,
                    Err(e) => return ToolOutput::err(format!("generate_image download: {e}")),
                };
                let media_type = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("image/png")
                    .split(';')
                    .next()
                    .unwrap_or("image/png")
                    .trim()
                    .to_string();
                match resp.bytes().await {
                    Ok(b) => {
                        let bytes = b.to_vec();
                        let base64 = base64_encode(&bytes);
                        (bytes, media_type, base64)
                    }
                    Err(e) => return ToolOutput::err(format!("generate_image download: {e}")),
                }
            }
        };
        // save
        let name = sanitize_file_name(
            file_name.unwrap_or(&format!(
                "image-{}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0),
                IMAGE_SEQ.fetch_add(1, Ordering::Relaxed),
            )),
            &media_type,
        );
        if let Err(e) = std::fs::create_dir_all(&self.cfg.output_dir) {
            return ToolOutput::err(format!(
                "generate_image: cannot create {}: {e}",
                self.cfg.output_dir.display()
            ));
        }
        let path = self.cfg.output_dir.join(name);
        if let Err(e) = std::fs::write(&path, &bytes) {
            return ToolOutput::err(format!(
                "generate_image: cannot write {}: {e}",
                path.display()
            ));
        }
        let mut out = ToolOutput::ok(format!(
            "saved {} ({} bytes, {}, model {} via {})",
            path.display(),
            bytes.len(),
            media_type,
            self.cfg.model,
            self.cfg.provider.name(),
        ));
        out.images.push(ka_protocol::ImagePart {
            data: base64,
            media_type,
        });
        out
    }
}

/// Minimal standard base64 (RFC 4648, padded). Enough for image parts
/// without pulling a new dependency.
pub fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Minimal standard base64 decode (RFC 4648; whitespace tolerated).
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let clean: Vec<u8> = text
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        if chunk.len() == 1 {
            return None; // impossible in valid input
        }
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn cfg(provider: ImageProvider) -> ResolvedImageGen {
        ResolvedImageGen {
            provider,
            api_key: "k-test".into(),
            model: provider.default_model().to_string(),
            output_dir: std::env::temp_dir(),
            base_url: None,
        }
    }

    #[test]
    fn gemini_request_shape() {
        let mut c = cfg(ImageProvider::Gemini);
        c.base_url = Some("http://127.0.0.1:1".into());
        let req = build_request(&c, "a red barn");
        assert_eq!(
            req.url,
            "http://127.0.0.1:1/v1beta/models/gemini-2.5-flash-image:generateContent"
        );
        assert_eq!(
            req.headers,
            vec![("x-goog-api-key".to_string(), "k-test".to_string())]
        );
        assert_eq!(
            req.body["contents"][0]["parts"][0]["text"],
            json!("a red barn")
        );
    }

    #[test]
    fn openai_and_grok_request_shapes() {
        let req = build_request(&cfg(ImageProvider::OpenAI), "cat");
        assert_eq!(req.url, "https://api.openai.com/v1/images/generations");
        assert_eq!(req.headers[0].0, "Authorization");
        assert!(req.headers[0].1.starts_with("Bearer "));
        assert_eq!(req.body["model"], json!("gpt-image-1"));
        assert_eq!(req.body["n"], json!(1));

        let req = build_request(&cfg(ImageProvider::Grok), "cat");
        assert_eq!(req.url, "https://api.x.ai/v1/images/generations");
        assert_eq!(req.body["model"], json!("grok-2-image"));
        assert_eq!(req.body["response_format"], json!("b64_json"));
    }

    #[test]
    fn gemini_response_parses_inline_data() {
        let b64 = base64_encode(b"\x89PNG tiny");
        let body = json!({
            "candidates": [{
                "content": {"parts": [{"inlineData": {
                    "mimeType": "image/png",
                    "data": b64
                }}]}
            }]
        });
        match parse_response(ImageProvider::Gemini, &body).unwrap() {
            ImageResult::Inline {
                base64,
                bytes,
                media_type,
            } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(bytes, b"\x89PNG tiny");
                assert_eq!(base64, b64);
            }
            other => panic!("{other:?}"),
        }
        // provider error surfaces its message
        let err = json!({"error": {"message": "safety block"}});
        assert_eq!(
            parse_response(ImageProvider::Gemini, &err).unwrap_err(),
            "safety block"
        );
    }

    #[test]
    fn openai_response_parses_b64_and_url() {
        let b64 = base64_encode(b"PNG!");
        let body = json!({"data": [{"b64_json": b64}]});
        match parse_response(ImageProvider::OpenAI, &body).unwrap() {
            ImageResult::Inline { bytes, .. } => assert_eq!(bytes, b"PNG!"),
            other => panic!("{other:?}"),
        }
        let body = json!({"data": [{"url": "https://cdn.example/x.png"}]});
        assert_eq!(
            parse_response(ImageProvider::OpenAI, &body).unwrap(),
            ImageResult::Url("https://cdn.example/x.png".into())
        );
        let err = json!({"error": {"message": "billing"}});
        assert_eq!(
            parse_response(ImageProvider::Grok, &err).unwrap_err(),
            "billing"
        );
    }

    #[test]
    fn base64_round_trips_and_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
        // whitespace and padding tolerated (wire-shaped payloads)
        assert_eq!(base64_decode("Zm9v\nYmFy\n").unwrap(), b"foobar");
        // the canonical 1x1 red PNG decodes to a PNG signature
        let png1x1 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let bytes = base64_decode(png1x1).unwrap();
        assert_eq!(&bytes[..4], b"\x89PNG");
        assert_eq!(base64_encode(&bytes), png1x1);
        assert!(base64_decode("!!!!").is_none());
    }

    #[test]
    fn file_names_are_sanitized() {
        assert_eq!(sanitize_file_name("barn.png", "image/png"), "barn.png");
        assert_eq!(sanitize_file_name("barn", "image/png"), "barn.png");
        assert_eq!(sanitize_file_name("barn", "image/jpeg"), "barn.jpg");
        assert_eq!(sanitize_file_name("barn.jpg", "image/png"), "barn.jpg");
        assert_eq!(
            sanitize_file_name("../../etc/passwd", "image/png"),
            "passwd.png"
        );
        assert_eq!(
            sanitize_file_name("we ird!/name", "image/webp"),
            "name.webp"
        );
        assert_eq!(sanitize_file_name("...", "image/png"), "image.png");
        assert_eq!(sanitize_file_name("", "image/png"), "image.png");
    }

    #[test]
    fn registration_gating_and_defaults() {
        let env = |names: &[(&str, &str)]| {
            let map: std::collections::HashMap<String, String> = names
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            move |name: &str| map.get(name).cloned()
        };
        let cwd = std::path::Path::new("/work");

        // no provider → no hand
        assert!(resolve(None, None, None, None, None, env(&[]), cwd).is_none());
        // unsupported provider → no hand
        assert!(resolve(Some("midjourney"), None, None, None, None, env(&[]), cwd).is_none());
        // provider without any key → no hand
        assert!(resolve(Some("gemini"), None, None, None, None, env(&[]), cwd).is_none());
        // provider + its default key → registered with per-provider defaults
        let cfg = resolve(
            Some("gemini"),
            None,
            None,
            None,
            None,
            env(&[("GEMINI_API_KEY", "gk")]),
            cwd,
        )
        .unwrap();
        assert_eq!(cfg.provider, ImageProvider::Gemini);
        assert_eq!(cfg.api_key, "gk");
        assert_eq!(cfg.model, "gemini-2.5-flash-image");
        assert_eq!(cfg.output_dir, cwd.join(".ka").join("images"));

        // grok ladder: GROK_API_KEY before XAI_API_KEY
        let cfg = resolve(
            Some("grok"),
            None,
            None,
            None,
            None,
            env(&[("XAI_API_KEY", "xk")]),
            cwd,
        )
        .unwrap();
        assert_eq!(cfg.api_key, "xk");
        let cfg = resolve(
            Some("grok"),
            None,
            None,
            None,
            None,
            env(&[("GROK_API_KEY", "gk"), ("XAI_API_KEY", "xk")]),
            cwd,
        )
        .unwrap();
        assert_eq!(cfg.api_key, "gk");

        // an explicit api_key_env override is the only var consulted
        let cfg = resolve(
            Some("openai"),
            Some("MY_KEY"),
            Some("gpt-image-1-min".into()),
            Some("/imgs".into()),
            Some("http://proxy".into()),
            env(&[("MY_KEY", "mk"), ("OPENAI_API_KEY", "ok")]),
            cwd,
        )
        .unwrap();
        assert_eq!(cfg.api_key, "mk");
        assert_eq!(cfg.model, "gpt-image-1-min");
        assert_eq!(cfg.output_dir, PathBuf::from("/imgs"));
        assert_eq!(cfg.base_url.as_deref(), Some("http://proxy"));
        // override env missing → no hand even though OPENAI_API_KEY is set
        assert!(
            resolve(
                Some("openai"),
                Some("MY_KEY"),
                None,
                None,
                None,
                env(&[("OPENAI_API_KEY", "ok")]),
                cwd
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn write_path_against_a_local_fixture() {
        use std::io::{Read as _, Write as _};
        // a tiny local images API: one POST that answers openai-style
        // b64_json with the canonical 1x1 PNG
        let png1x1 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                match stream.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        let s = String::from_utf8_lossy(&buf).into_owned();
                        let Some(pos) = s.find("\r\n\r\n") else {
                            continue;
                        };
                        let complete = s.lines().find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().ok())
                        });
                        if let Some(Some(cl)) = complete {
                            if buf.len() >= pos + 4 + cl {
                                break;
                            }
                        }
                    }
                }
            }
            let body = format!("{{\"data\": [{{\"b64_json\": \"{png1x1}\"}}]}}");
            let http = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(http.as_bytes());
            let _ = stream.shutdown(std::net::Shutdown::Both);
        });

        let out_dir = std::env::temp_dir().join(format!(
            "ka-img-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&out_dir);
        let hand = GenerateImageHand::new(ResolvedImageGen {
            provider: ImageProvider::OpenAI,
            api_key: "k-test".into(),
            model: "gpt-image-1".into(),
            output_dir: out_dir.clone(),
            base_url: Some(format!("http://{addr}")),
        });
        let out = hand.generate("a tiny red pixel", Some("pixel")).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains(out_dir.to_string_lossy().as_ref()));
        let saved = out_dir.join("pixel.png");
        let bytes = std::fs::read(&saved).unwrap_or_else(|e| panic!("{}: {e}", saved.display()));
        assert_eq!(&bytes[..4], b"\x89PNG");
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].media_type, "image/png");
        assert_eq!(base64_decode(&out.images[0].data).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&out_dir);
    }
}
