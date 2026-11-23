//! Dialect catalog: per-model wire profiles as data (`dialects.toml`), plus
//! model-selector parsing. Strictness rule: every table rejects unknown
//! keys, so typos in user overlays fail with the offending key name.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The embedded seed catalog.
pub const EMBEDDED: &str = include_str!("../dialects.toml");

/// The generated models.dev overlay: providers and real pricing for
/// tool-capable models (regenerate with `cargo xtask models-sync`).
/// Curated rows in `dialects.toml` win for the same selector.
pub const MODELS_DEV: &str = include_str!("../models-dev.toml");

/// Version marker of the EMBEDDED catalog. Neither `dialects.toml` nor
/// `models-dev.toml` carry an in-file version — the shipping crate
/// version stands in: a runtime catalog file only overrides the
/// embedded rows when its own `"version"` field is strictly greater
/// (semver, numeric component compare; anything unparseable, equal, or
/// older is ignored, so a stale download never clobbers curated rows).
pub const CATALOG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// models.dev api.json endpoint the runtime catalog refreshes from.
const MODELS_DEV_API: &str = "https://models.dev/api.json";

/// Catalog file name inside the ka data dir.
const RUNTIME_CATALOG_FILE: &str = "catalog.json";

/// True when `candidate` is a strictly greater semver than the embedded
/// [`CATALOG_VERSION`] (numeric component compare, shorter version
/// sorts lower: `0.7.3` > `0.7.2`, `0.8` > `0.7.9`).
pub fn catalog_version_is_newer(candidate: &str) -> bool {
    fn parse(v: &str) -> Vec<u64> {
        v.trim()
            .trim_start_matches('v')
            .split('.')
            .map(|p| p.trim().parse::<u64>().unwrap_or(0))
            .collect()
    }
    let (mut a, mut b) = (parse(candidate), parse(CATALOG_VERSION));
    a.resize(b.len().max(a.len()), 0);
    b.resize(a.len(), 0);
    a > b
}

/// Read the runtime catalog from `<data_root>/catalog.json`, if it
/// exists and is fresh: the file must parse as JSON, carry a `"version"`
/// string strictly greater than the embedded [`CATALOG_VERSION`], and
/// hold a `dialects` object (`{"version": "0.7.3", "dialects": {
/// "vendor/model": { ...dialect fields... } }}`). Anything else —
/// missing file, stale version, malformed JSON — is `None` and the
/// embedded catalog stands alone.
pub fn load_runtime_catalog(data_root: &std::path::Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(data_root.join(RUNTIME_CATALOG_FILE)).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let version = json.get("version")?.as_str()?;
    if !catalog_version_is_newer(version) {
        return None;
    }
    if !json
        .get("dialects")
        .is_some_and(serde_json::Value::is_object)
    {
        return None;
    }
    Some(json)
}

/// Persist a runtime catalog to `<data_root>/catalog.json` (creating the
/// data dir). Writes exactly the given JSON — callers shape it (see
/// [`load_runtime_catalog`] for the expected form); the fetch path is
/// `fetch_models_dev()` plus a convert step, so a failed conversion
/// never poisons the live catalog.
pub fn store_runtime_catalog(
    data_root: &std::path::Path,
    json: &serde_json::Value,
) -> std::io::Result<()> {
    std::fs::create_dir_all(data_root)?;
    let text = serde_json::to_string_pretty(json)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    std::fs::write(data_root.join(RUNTIME_CATALOG_FILE), text)
}

/// Fetch the models.dev api.json (network; no tests exercise this).
/// The raw provider-shaped JSON converts to catalog rows before
/// [`store_runtime_catalog`] — the wire shape here is models.dev's, not
/// the catalog's.
pub async fn fetch_models_dev() -> Result<serde_json::Value, String> {
    let resp = reqwest::get(MODELS_DEV_API)
        .await
        .map_err(|e| format!("models.dev fetch failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("models.dev fetch returned {}", resp.status()));
    }
    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| format!("models.dev response is not JSON: {e}"))
}

/// Which wire protocol a model speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wire {
    /// Anthropic Messages API.
    AnthropicMessages,
    /// OpenAI Responses API (reasoning models, item-based history).
    OpenaiResponses,
    /// OpenAI Chat Completions (and every compatible endpoint).
    OpenaiChat,
}

/// Local-endpoint discovery probe for a dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Discovery {
    /// Ollama native API probe.
    Ollama,
    /// LM Studio probe.
    LmStudio,
    /// vLLM probe.
    Vllm,
}

/// Input modality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    /// Text.
    Text,
    /// Images.
    Image,
}

/// How the wire supports prompt caching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cache {
    /// No caching support known.
    #[default]
    Off,
    /// Anthropic-style `cache_control` breakpoints.
    Control,
    /// Provider-side key/implicit caching.
    Key,
}

/// Per-mtok pricing (placeholder seed values; verify before release).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Price {
    /// USD per million input tokens.
    #[serde(default)]
    pub input_per_mtok: f64,
    /// USD per million output tokens.
    #[serde(default)]
    pub output_per_mtok: f64,
}

/// Per-model behavioral flags — the growing dialect system.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flags {
    /// Wire field carrying reasoning config (empty = not supported).
    #[serde(default)]
    pub reasoning_field: Option<String>,
    /// Re-emit reasoning content byte-exact for KV-cache prefix reuse.
    #[serde(default)]
    pub replay_reasoning: bool,
    /// Request field for output-token cap (None = `max_tokens`).
    #[serde(default)]
    pub max_tokens_field: Option<String>,
    /// Whether the model accepts a structured-output schema. Defaults to
    /// true on parsed rows: every wire ka speaks today carries schema
    /// support; opt out per row with `structured = false`.
    /// [`Flags::default`] (discovery rows) is false — local models stay
    /// conservative.
    #[serde(default = "default_true")]
    pub structured: bool,
    /// Whether the model accepts tool calls on the wire (sniffed for
    /// discovered local models; `false` = unknown or absent).
    #[serde(default)]
    pub tools: bool,
    /// Provider-native server-side web search: when true, the wire
    /// injects the provider's own search tool (Anthropic's
    /// `web_search_20250305` server tool, OpenAI Responses'
    /// `web_search`) and drops ka's client-side `web_search` hand for
    /// that model — no separate `[[search]]` API key needed. Supported
    /// on the `anthropic_messages` and `openai_responses` wires only.
    #[serde(default)]
    pub server_search: bool,
}

/// One model's wire profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dialect {
    /// Wire protocol.
    pub wire: Wire,
    /// Endpoint base URL (required for speaking; catalog seeds carry it).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Environment variable holding the API token (None = keyless local).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Authentication tier override: `"passport"` = the bearer token
    /// comes from a registered `TokenSource` (ka-passport), never an env
    /// var — the row then carries no `api_key_env`. `None`/any other
    /// value = the standard env/dotenv/registry/keyring ladder over
    /// `api_key_env` (ratified catalog key, sdk.md §6).
    #[serde(default)]
    pub auth: Option<String>,
    /// Vendor docs URL (pricing/console) surfaced by the key prompt.
    #[serde(default)]
    pub doc_url: Option<String>,
    /// Model id override sent on the wire (None = selector model part).
    #[serde(default)]
    pub wire_model: Option<String>,
    /// Local discovery probe, if any.
    #[serde(default)]
    pub discovery: Option<Discovery>,
    /// Context window (tokens; 0 = unknown, discovered).
    #[serde(default)]
    pub context: u32,
    /// Max output tokens.
    #[serde(default = "default_max_output")]
    pub max_output: u32,
    /// Supported effort levels (empty = not controllable).
    #[serde(default)]
    pub efforts: Vec<String>,
    /// Input modalities.
    #[serde(default)]
    pub input: Vec<Modality>,
    /// Prompt-caching mode.
    #[serde(default)]
    pub cache: Cache,
    /// Chars-per-token estimate for meters and digest triggers.
    #[serde(default = "default_ratio")]
    pub ratio: f32,
    /// First-byte timeout in ms (0 = unbounded, for local prefill).
    #[serde(default = "default_first_byte_timeout_ms")]
    pub first_byte_timeout_ms: u64,
    /// Effort → thinking budget (tokens), for wires that take budgets.
    #[serde(default)]
    pub effort_budgets: BTreeMap<String, u32>,
    /// Pricing.
    #[serde(default)]
    pub price: Price,
    /// Whether the pricing is vendor-verified (false = placeholders —
    /// surfaces must not display costs from them).
    #[serde(default)]
    pub priced: bool,
    /// Behavioral flags.
    #[serde(default)]
    pub flags: Flags,
}

fn default_true() -> bool {
    true
}

fn default_max_output() -> u32 {
    8_192
}
fn default_ratio() -> f32 {
    4.0
}
fn default_first_byte_timeout_ms() -> u64 {
    120_000
}

/// The full catalog: model id (`vendor/model`) → dialect.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    /// All known dialects.
    pub dialects: BTreeMap<String, Dialect>,
}

impl Catalog {
    /// Parse a catalog from TOML text (strict: unknown keys error).
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// The embedded catalog: the generated models.dev table first, then
    /// the curated `dialects.toml` rows overlaid on top — curated wins
    /// per selector, so a hand-curated row can enrich a models.dev
    /// entry (efforts, flags) without the generator clobbering it.
    pub fn embedded() -> Self {
        let mut catalog = Self::parse(MODELS_DEV)
            .unwrap_or_else(|e| panic!("models.dev overlay catalog is invalid: {e}"));
        match Self::parse(EMBEDDED) {
            Ok(curated) => catalog.overlay(curated),
            Err(e) => panic!("embedded dialect catalog is invalid: {e}"),
        }
        catalog
    }

    /// Leaf-merge partial user overlay TOMLs onto the embedded catalog.
    /// Unlike [`Catalog::overlay`] (whole-row replacement, used for the
    /// models.dev path by design), user rows winning per leaf: a
    /// `[dialects."x"]` table setting only `context` keeps the
    /// embedded row's wire/base_url/api_key_env/flags, and unknown
    /// selectors create new rows. Multiple overlay files compose
    /// leaf-wise (later files win per leaf over earlier ones — never
    /// back to embedded). Strict-TOML discipline: an overlay whose
    /// top level is not exactly a `[dialects]` table is a hard error
    /// naming what was found (a typo'd `[dialect]` must not silently
    /// no-op), and row-level unknown keys still name the key.
    pub fn merged_embedded_with_overlays<'a>(
        overlay_texts: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, String> {
        Self::merged_embedded_with_runtime(None, overlay_texts)
    }

    /// The runtime-catalog-aware merge. Layer order, most general
    /// first: embedded (models.dev + curated `dialects.toml`) < runtime
    /// catalog file (`load_runtime_catalog`, leaf-merged BEFORE user
    /// overlays so a hand-written `--dialects` overlay always wins) <
    /// user overlay TOMLs in order. The runtime value is the JSON
    /// document [`load_runtime_catalog`] returns; its `dialects` object
    /// merges leaf-wise exactly like an overlay. `None` (no fresh
    /// runtime catalog) skips the layer.
    pub fn merged_embedded_with_runtime<'a>(
        runtime: Option<&serde_json::Value>,
        overlay_texts: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, String> {
        fn parse_table(text: &str, what: &str) -> Result<toml::Table, String> {
            toml::from_str(text).map_err(|e| format!("{what} is invalid: {e}"))
        }
        let mut base = parse_table(MODELS_DEV, "models.dev overlay catalog")?;
        let curated = parse_table(EMBEDDED, "embedded dialect catalog")?;
        // curated rows win per selector (same semantics as `embedded`)
        if let (Some(b), Some(c)) = (base.get_mut("dialects"), curated.get("dialects")) {
            if let (Some(bd), Some(cd)) = (b.as_table_mut(), c.as_table()) {
                for (k, v) in cd {
                    bd.insert(k.clone(), v.clone());
                }
            }
        }
        // runtime catalog: embedded < catalog.json < --dialects
        if let Some(json) = runtime {
            let rows = json
                .get("dialects")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| {
                    "runtime catalog is invalid: missing a `dialects` object".to_string()
                })?;
            let as_toml = json_to_toml(&serde_json::Value::Object(rows.clone()));
            if let Some(bd) = base.get_mut("dialects") {
                merge_tables(bd, &as_toml);
            }
        }
        for overlay_text in overlay_texts {
            let overlay = parse_table(overlay_text, "user catalog overlay")?;
            if overlay.is_empty() {
                continue;
            }
            // EXACTLY a `[dialects]` table: a valid section beside a
            // typo'd sibling (`[dialect]`) must not silently drop the
            // typo'd row — reject naming every top-level key found
            if overlay.len() != 1 || !overlay.contains_key("dialects") {
                return Err(format!(
                    "user catalog overlay is invalid: expected exactly a `[dialects]` \
                     table, found keys: {}",
                    overlay
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            let od = overlay
                .get("dialects")
                .and_then(|v| v.as_table())
                .ok_or_else(|| {
                    "user catalog overlay is invalid: `dialects` must be a table".to_string()
                })?;
            if let Some(bd) = base.get_mut("dialects") {
                merge_tables(bd, &toml::Value::Table(od.clone()));
            }
        }
        let text = toml::to_string(&base)
            .map_err(|e| format!("re-serializing merged catalog failed: {e}"))?;
        Self::parse(&text).map_err(|e| e.to_string())
    }

    /// Overlay another catalog on top (user wins per key).
    pub fn overlay(&mut self, other: Catalog) {
        for (k, v) in other.dialects {
            self.dialects.insert(k, v);
        }
    }

    /// Look a dialect up by exact `vendor/model` id.
    pub fn get(&self, model_id: &str) -> Option<&Dialect> {
        self.dialects.get(model_id)
    }
}

/// Recursively merge TOML values per leaf: `src` wins field by field;
/// scalar/array leaves replace, tables recurse. Unknown selectors
/// create new rows.
fn merge_tables(dst: &mut toml::Value, src: &toml::Value) {
    if let (Some(d), Some(s)) = (dst.as_table_mut(), src.as_table()) {
        for (k, v) in s {
            match d.get_mut(k) {
                Some(slot) if slot.is_table() && v.is_table() => merge_tables(slot, v),
                _ => {
                    d.insert(k.clone(), v.clone());
                }
            }
        }
    }
}

/// Convert a JSON value into a TOML value for the runtime-catalog merge.
/// JSON `null`s have no TOML representation — they drop out of tables
/// (a null leaf means "unset", never an explicit empty string).
fn json_to_toml(v: &serde_json::Value) -> toml::Value {
    match v {
        serde_json::Value::Null => toml::Value::String(String::new()),
        serde_json::Value::Bool(b) => toml::Value::Boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else if let Some(u) = n.as_u64() {
                toml::Value::Integer(u as i64)
            } else {
                toml::Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => toml::Value::String(s.clone()),
        serde_json::Value::Array(items) => {
            toml::Value::Array(items.iter().map(json_to_toml).collect())
        }
        serde_json::Value::Object(map) => {
            let mut table = toml::Table::new();
            for (k, item) in map {
                if item.is_null() {
                    continue;
                }
                table.insert(k.clone(), json_to_toml(item));
            }
            toml::Value::Table(table)
        }
    }
}

/// A parsed model selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    /// Vendor prefix.
    pub vendor: String,
    /// Model id within the vendor.
    pub model: String,
    /// Optional effort suffix.
    pub effort: Option<String>,
}

impl Selector {
    /// The `vendor/model` id without effort.
    pub fn model_id(&self) -> String {
        format!("{}/{}", self.vendor, self.model)
    }
}

/// Selector parse failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorError {
    /// Missing the `vendor/model` slash.
    MissingVendor,
    /// Empty vendor or model component.
    EmptyPart,
}

impl std::fmt::Display for SelectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelectorError::MissingVendor => write!(f, "selector needs vendor/model, got no '/'"),
            SelectorError::EmptyPart => write!(f, "selector vendor and model must be non-empty"),
        }
    }
}
impl std::error::Error for SelectorError {}

/// Parse `vendor/model[@effort]`. Effort uses `@` because model ids may
/// contain colons (e.g. Ollama tags like `qwen3:32b`).
pub fn parse_selector(s: &str) -> Result<Selector, SelectorError> {
    let (base, effort) = match s.rsplit_once('@') {
        Some((b, e)) => (b, Some(e.to_string())),
        None => (s, None),
    };
    let Some((vendor, model)) = base.split_once('/') else {
        return Err(SelectorError::MissingVendor);
    };
    if vendor.is_empty() || model.is_empty() {
        return Err(SelectorError::EmptyPart);
    }
    Ok(Selector {
        vendor: vendor.to_string(),
        model: model.to_string(),
        effort,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    // ── MODEL-06 · runtime catalog ──────────────────────────────────

    /// Unique temp dir per call (no tempfile dependency in this crate).
    fn temp_data_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ka-dialect-catalog-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn runtime_json(version: &str, model: &str, context: u32) -> serde_json::Value {
        serde_json::json!({
            "version": version,
            "dialects": {
                model: { "context": context }
            }
        })
    }

    #[test]
    fn version_marker_compares_semver_numerically() {
        assert!(catalog_version_is_newer("99.0.0"));
        let bumped = format!(
            "{}.999",
            CATALOG_VERSION
                .rsplit_once('.')
                .map(|(a, _)| a.to_string())
                .unwrap_or_else(|| CATALOG_VERSION.to_string())
        );
        assert!(
            catalog_version_is_newer(&bumped),
            "same-minor higher patch counts as newer: {bumped}"
        );
        assert!(
            !catalog_version_is_newer(CATALOG_VERSION),
            "equal is not newer"
        );
        assert!(!catalog_version_is_newer("0.0.1"));
        assert!(!catalog_version_is_newer("not-a-version"));
        assert!(!catalog_version_is_newer(""));
    }

    #[test]
    fn runtime_catalog_roundtrips_through_disk() {
        let root = temp_data_root("roundtrip");
        assert!(
            load_runtime_catalog(&root).is_none(),
            "missing file is None"
        );
        store_runtime_catalog(&root, &runtime_json("99.0.0", "vendor/model", 123_456)).unwrap();
        let loaded = load_runtime_catalog(&root).expect("fresh file loads");
        assert_eq!(loaded["dialects"]["vendor/model"]["context"], 123_456);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_or_malformed_runtime_catalog_is_ignored() {
        let root = temp_data_root("stale");
        // stale version: embedded wins, the file is not an error
        store_runtime_catalog(&root, &runtime_json("0.0.1", "vendor/model", 1)).unwrap();
        assert!(load_runtime_catalog(&root).is_none());
        // same version is not newer either
        store_runtime_catalog(&root, &runtime_json(CATALOG_VERSION, "vendor/model", 1)).unwrap();
        assert!(load_runtime_catalog(&root).is_none());
        // malformed JSON
        std::fs::write(root.join("catalog.json"), "{not json").unwrap();
        assert!(load_runtime_catalog(&root).is_none());
        // fresh version but no `dialects` object: invalid shape
        store_runtime_catalog(&root, &serde_json::json!({"version": "99.0.0"})).unwrap();
        assert!(load_runtime_catalog(&root).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn runtime_merge_sits_between_embedded_and_user_overlays() {
        let root = temp_data_root("merge");
        // the runtime file enriches an EMBEDDED row leaf-wise and adds a
        // brand-new selector
        store_runtime_catalog(
            &root,
            &runtime_json("99.0.0", "anthropic/claude-sonnet-5", 777_777),
        )
        .unwrap();
        let runtime = load_runtime_catalog(&root);
        let with_runtime = Catalog::merged_embedded_with_runtime(runtime.as_ref(), []).unwrap();
        let runtime_row = with_runtime.get("anthropic/claude-sonnet-5").unwrap();
        assert_eq!(runtime_row.context, 777_777, "runtime leaf won");
        // wire/base_url survive: the runtime layer is a leaf merge, not
        // a whole-row replacement
        assert_eq!(runtime_row.wire, Wire::AnthropicMessages);
        assert_eq!(
            runtime_row.base_url.as_deref(),
            Some("https://api.anthropic.com")
        );
        assert!(
            with_runtime.get("runtime-only/new-model").is_none(),
            "runtime layer only carried the one selector"
        );

        // user overlay wins OVER the runtime file: same leaf, later layer
        let overlay = "[dialects.\"anthropic/claude-sonnet-5\"]\ncontext = 1\n\n\
                       [dialects.\"runtime-only/new-model\"]\nwire = \"openai_chat\"\n";
        let merged = Catalog::merged_embedded_with_runtime(runtime.as_ref(), [overlay]).unwrap();
        assert_eq!(
            merged.get("anthropic/claude-sonnet-5").unwrap().context,
            1,
            "--dialects overlay beats the runtime file"
        );
        let synth = merged.get("runtime-only/new-model").unwrap();
        assert_eq!(
            synth.wire,
            Wire::OpenaiChat,
            "overlay's new selector landed"
        );
        assert_eq!(
            synth.base_url, None,
            "overlay row keeps only what it sets (embedded has no such row)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn runtime_merge_rejects_unknown_row_keys_strictly() {
        let root = temp_data_root("strict");
        store_runtime_catalog(
            &root,
            &serde_json::json!({
                "version": "99.0.0",
                "dialects": { "vendor/typo": { "cotnext": 1 } }
            }),
        )
        .unwrap();
        let runtime = load_runtime_catalog(&root);
        let err = Catalog::merged_embedded_with_runtime(runtime.as_ref(), []).unwrap_err();
        assert!(
            err.contains("cotnext"),
            "unknown runtime keys name the key like overlays do: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn embedded_catalog_carries_the_claude_plan_rows() {
        let c = Catalog::embedded();
        for id in ["claude-plan/claude-sonnet-5", "claude-plan/claude-opus-4"] {
            let d = c
                .get(id)
                .unwrap_or_else(|| panic!("missing curated row {id}"));
            assert_eq!(d.wire, Wire::AnthropicMessages, "{id}");
            assert_eq!(
                d.base_url.as_deref(),
                Some("https://api.anthropic.com"),
                "{id}"
            );
            assert_eq!(d.auth.as_deref(), Some("passport"), "{id}");
            assert_eq!(
                d.api_key_env, None,
                "{id}: subscription rows carry no env key"
            );
            assert!(!d.priced, "{id}: never fabricate subscription costs");
        }
    }

    #[test]
    fn seeded_rows_never_fabricate_pricing() {
        // curated rows default to placeholder pricing (priced = false);
        // a seed row may claim real pricing only with positive price fields
        let c = Catalog::parse(EMBEDDED).unwrap();
        for (id, d) in &c.dialects {
            if d.priced {
                assert!(
                    d.price.input_per_mtok > 0.0,
                    "{id} priced without input cost"
                );
                assert!(
                    d.price.output_per_mtok > 0.0,
                    "{id} priced without output cost"
                );
            }
        }
    }

    #[test]
    fn deepseek_flash_row_is_verified_and_priced() {
        let c = Catalog::embedded();
        let d = c
            .get("deepseek/deepseek-flash")
            .expect("deepseek-flash row");
        assert_eq!(d.wire, Wire::OpenaiChat);
        assert_eq!(d.base_url.as_deref(), Some("https://api.deepseek.com"));
        assert_eq!(d.api_key_env.as_deref(), Some("DEEPSEEK_API_KEY"));
        assert_eq!(d.context, 1_000_000);
        assert_eq!(d.max_output, 384_000);
        assert!(d.priced, "peak pricing is vendor-verified");
        assert_eq!(d.price.input_per_mtok, 0.30);
        assert_eq!(d.price.output_per_mtok, 1.20);
    }

    #[test]
    fn models_dev_overlay_merges_with_real_pricing() {
        let c = Catalog::embedded();
        // z.ai api tier: real published pricing
        let zai = c.get("zai/glm-4.7").expect("zai api tier present");
        assert!(zai.priced);
        assert_eq!(
            zai.base_url.as_deref(),
            Some("https://api.z.ai/api/paas/v4")
        );
        assert_eq!(zai.api_key_env.as_deref(), Some("ZHIPU_API_KEY"));
        // z.ai coding plan: same key, different endpoint, unpriced plan
        let plan = c
            .get("zai-coding-plan/glm-5.3")
            .expect("zai coding plan present");
        assert_eq!(
            plan.base_url.as_deref(),
            Some("https://api.z.ai/api/coding/paas/v4")
        );
        assert!(!plan.priced, "subscription plan is not per-token priced");
        // curated rows keep their richer flags (not replaced by the overlay)
        let curated = c.get("anthropic/claude-sonnet-5").unwrap();
        assert_eq!(curated.cache, Cache::Control);
    }

    #[test]
    fn opencode_go_rows_merge_from_overlay() {
        let c = Catalog::embedded();
        let k = c
            .get("opencode-go/kimi-k3")
            .expect("opencode-go/kimi-k3 row");
        assert_eq!(k.wire, Wire::OpenaiChat);
        assert_eq!(
            k.base_url.as_deref(),
            Some("https://opencode.ai/inference/go/openai/v1")
        );
        assert_eq!(k.api_key_env.as_deref(), Some("OPENCODE_API_KEY"));
        assert_eq!(k.context, 1_048_576);
        assert_eq!(k.max_output, 131_072);
        assert!(k.priced, "OpenCode Go publishes real per-token pricing");
        assert_eq!(k.price.input_per_mtok, 3.0);
        assert_eq!(k.price.output_per_mtok, 15.0);
        // free preview rows carry 0/0 costs — not per-token pricing
        let free = c
            .get("opencode-go/cat-jam-free")
            .expect("opencode-go/cat-jam-free row");
        assert!(!free.priced);
    }

    #[test]
    fn embedded_catalog_parses() {
        // generated data regressions must be loud: both embedded tables
        // parse (Catalog::embedded panics otherwise)
        assert!(Catalog::parse(MODELS_DEV).is_ok());
        let c = Catalog::embedded();
        assert!(c.dialects.len() >= 3);
        let d = c.get("anthropic/claude-sonnet-5").unwrap();
        assert_eq!(d.wire, Wire::AnthropicMessages);
        assert_eq!(d.cache, Cache::Control);
        assert_eq!(d.base_url.as_deref(), Some("https://api.anthropic.com"));
        assert_eq!(d.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(d.flags.reasoning_field.as_deref(), Some("thinking"));
        assert_eq!(d.effort_budgets.get("high"), Some(&16_384));
        let o = c.get("openai/gpt-5.1").unwrap();
        assert_eq!(
            o.flags.max_tokens_field.as_deref(),
            Some("max_completion_tokens")
        );
        let q = c.get("ollama/qwen3-32b").unwrap();
        assert_eq!(q.first_byte_timeout_ms, 0);
        assert!(q.flags.replay_reasoning);
    }

    #[test]
    fn embedded_vision_rows_claim_image() {
        // the `input` claim is load-bearing: every wire hard-errors on
        // image-bearing turns without it, and the models.dev overlay
        // never backfills it (whole-row replacement). Pin the curated
        // vision rows so a stray edit cannot silently disable images.
        let c = Catalog::embedded();
        for id in [
            "anthropic/claude-sonnet-5",
            "openai/gpt-5.1",
            "openai/o3",
            "openai/o4-mini",
            "deepseek/deepseek-flash",
        ] {
            let d = c.get(id).unwrap_or_else(|| panic!("{id} missing"));
            assert!(
                d.input.iter().any(|m| matches!(m, Modality::Image)),
                "{id} must claim image input"
            );
        }
    }

    #[test]
    fn embedded_curated_rows_win_over_models_dev() {
        // the zai rows exist in the generated models.dev table without
        // efforts; the curated seed row must survive the overlay
        let c = Catalog::embedded();
        let glm = c.get("zai/glm-5.3-flash").expect("zai row present");
        assert_eq!(
            glm.efforts,
            vec!["low".to_string(), "high".to_string(), "max".to_string()]
        );
        assert!(glm.flags.reasoning_field.is_some());
        assert_eq!(glm.api_key_env.as_deref(), Some("ZHIPU_API_KEY"));
        assert_eq!(
            glm.base_url.as_deref(),
            Some("https://api.z.ai/api/paas/v4")
        );
    }

    #[test]
    fn unknown_keys_rejected() {
        let bad = "[dialects.\"x/y\"]\nwire = \"openai_chat\"\ncontext = 1\nbogus_key = true\n";
        let err = Catalog::parse(bad).unwrap_err().to_string();
        assert!(err.contains("bogus_key"), "got: {err}");
    }

    #[test]
    fn unknown_flag_rejected() {
        let bad = "[dialects.\"x/y\"]\nwire = \"openai_chat\"\ncontext = 1\n[dialects.\"x/y\".flags]\nmystery = 1\n";
        let err = Catalog::parse(bad).unwrap_err().to_string();
        assert!(err.contains("mystery"), "got: {err}");
    }

    #[test]
    fn overlay_user_wins() {
        let mut c = Catalog::embedded();
        let over = Catalog::parse(
            "[dialects.\"anthropic/claude-sonnet-5\"]\nwire = \"anthropic_messages\"\ncontext = 123\n",
        )
        .unwrap();
        c.overlay(over);
        assert_eq!(c.get("anthropic/claude-sonnet-5").unwrap().context, 123);
    }

    #[test]
    fn partial_user_overlay_keeps_embedded_fields() {
        let c = Catalog::merged_embedded_with_overlays([
            "[dialects.\"anthropic/claude-sonnet-5\"]\ncontext = 123\n",
        ])
        .unwrap();
        let d = c.get("anthropic/claude-sonnet-5").unwrap();
        assert_eq!(d.wire, Wire::AnthropicMessages);
        assert_eq!(d.base_url.as_deref(), Some("https://api.anthropic.com"));
        assert_eq!(d.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(d.context, 123);
        // unknown selectors create new rows
        let c = Catalog::merged_embedded_with_overlays([
            "[dialects.\"acme/new-model\"]\nwire = \"openai_chat\"\ncontext = 7\n",
        ])
        .unwrap();
        assert_eq!(c.get("acme/new-model").unwrap().context, 7);
        // multiple files compose LEAF-wise: b's merged row must not
        // revert a's context override back to the embedded value
        let c = Catalog::merged_embedded_with_overlays([
            "[dialects.\"anthropic/claude-sonnet-5\"]\ncontext = 123\n",
            "[dialects.\"anthropic/claude-sonnet-5\"]\nmax_output = 456\n",
        ])
        .unwrap();
        let d = c.get("anthropic/claude-sonnet-5").unwrap();
        assert_eq!(d.context, 123, "earlier file's leaf survives");
        assert_eq!(d.max_output, 456, "later file's leaf applies");
        // a typo'd top-level section is a hard error, never a silent no-op
        let err = Catalog::merged_embedded_with_overlays(["[dialect]\ncontext = 1\n"]).unwrap_err();
        assert!(err.contains("dialect"), "got: {err}");
        // unknown keys still hard-error naming the key
        let err = Catalog::merged_embedded_with_overlays([
            "[dialects.\"acme/new-model\"]\nwire = \"openai_chat\"\nbogus_key = 1\n",
        ])
        .unwrap_err();
        assert!(err.contains("bogus_key"), "got: {err}");
    }

    #[test]
    fn selectors_parse() {
        let s = parse_selector("openai/gpt-5.1@high").unwrap();
        assert_eq!(s.model_id(), "openai/gpt-5.1");
        assert_eq!(s.effort.as_deref(), Some("high"));
        let tagged = parse_selector("ollama/qwen3:32b").unwrap();
        assert_eq!(tagged.model_id(), "ollama/qwen3:32b");
        assert!(tagged.effort.is_none());
        assert!(parse_selector("nopath").unwrap_err() == SelectorError::MissingVendor);
        assert!(parse_selector("/model").unwrap_err() == SelectorError::EmptyPart);
    }
}
