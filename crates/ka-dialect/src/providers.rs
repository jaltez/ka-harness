//! Provider registry: known vendors with their wire protocol, endpoint,
//! and API-key environment variable. Powers `ka providers`, the settings
//! panel, and synthetic dialects for unseeded models — any
//! `provider/model` selector on a registered provider works without a
//! catalog row.
//!
//! Curation (v2): official providers first and FEW — Anthropic, OpenAI,
//! Google, z.ai (API), z.ai (Coding Readonly) — then community/alternative
//! vendors, then local runtimes last. The z.ai family is merged: legacy
//! `zhipu`/`zhipuai` selectors resolve to the z.ai provider (see
//! [`find`]). The final display order for both providers and catalog
//! models is defined by [`vendor_rank`]; surfaces sort by it and inherit
//! the order with no per-surface policy.
//!
//! Excluded entirely: Azure/Bedrock/Vertex (sigv4/Entra machinery),
//! JWT-only flows, and mirrors/duplicates of the official five.

use crate::dialects::{Dialect, Wire};

/// One known provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    /// Selector vendor prefix (`groq/llama-…`).
    pub name: &'static str,
    /// Wire protocol spoken at the endpoint.
    pub wire: Wire,
    /// Base URL (OpenAI-compatible path included).
    pub base_url: &'static str,
    /// Env var holding the API key (None = keyless local).
    pub key_env: Option<&'static str>,
    /// One-line note for listings.
    pub note: &'static str,
}

/// The v2 registry, in display order: official first, community next,
/// local runtimes last (mirrored by [`vendor_rank`]).
pub const PROVIDERS: &[Provider] = &[
    // ── official (first and few) ────────────────────────────────
    Provider {
        name: "anthropic",
        wire: Wire::AnthropicMessages,
        base_url: "https://api.anthropic.com",
        key_env: Some("ANTHROPIC_API_KEY"),
        note: "Claude models",
    },
    Provider {
        name: "openai",
        wire: Wire::OpenaiChat,
        base_url: "https://api.openai.com/v1",
        key_env: Some("OPENAI_API_KEY"),
        note: "GPT models",
    },
    Provider {
        name: "google",
        wire: Wire::OpenaiChat,
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        key_env: Some("GEMINI_API_KEY"),
        note: "Gemini via OpenAI-compatible bridge",
    },
    Provider {
        name: "zai",
        wire: Wire::OpenaiChat,
        base_url: "https://api.z.ai/api/paas/v4",
        key_env: Some("ZHIPU_API_KEY"),
        note: "z.ai API — GLM models (absorbs the legacy zhipu entry)",
    },
    Provider {
        name: "zai-plan",
        wire: Wire::OpenaiChat,
        base_url: "https://api.z.ai/api/coding/paas/v4",
        key_env: Some("ZHIPU_API_KEY"),
        note: "z.ai Coding Readonly subscription endpoint",
    },
    // ── community / alternative ─────────────────────────────────
    Provider {
        name: "deepseek",
        wire: Wire::OpenaiChat,
        base_url: "https://api.deepseek.com",
        key_env: Some("DEEPSEEK_API_KEY"),
        note: "DeepSeek models",
    },
    Provider {
        name: "mistral",
        wire: Wire::OpenaiChat,
        base_url: "https://api.mistral.ai/v1",
        key_env: Some("MISTRAL_API_KEY"),
        note: "Mistral models",
    },
    Provider {
        name: "groq",
        wire: Wire::OpenaiChat,
        base_url: "https://api.groq.com/openai/v1",
        key_env: Some("GROQ_API_KEY"),
        note: "ultra-fast open-model inference",
    },
    Provider {
        name: "cerebras",
        wire: Wire::OpenaiChat,
        base_url: "https://api.cerebras.ai/v1",
        key_env: Some("CEREBRAS_API_KEY"),
        note: "very fast Llama/Qwen inference",
    },
    Provider {
        name: "qwen",
        wire: Wire::OpenaiChat,
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        key_env: Some("DASHSCOPE_API_KEY"),
        note: "Alibaba Qwen (DashScope)",
    },
    Provider {
        name: "moonshot",
        wire: Wire::OpenaiChat,
        base_url: "https://api.moonshot.cn/v1",
        key_env: Some("MOONSHOT_API_KEY"),
        note: "Kimi models",
    },
    Provider {
        name: "xai",
        wire: Wire::OpenaiChat,
        base_url: "https://api.x.ai/v1",
        key_env: Some("XAI_API_KEY"),
        note: "Grok models",
    },
    Provider {
        name: "nvidia",
        wire: Wire::OpenaiChat,
        base_url: "https://integrate.api.nvidia.com/v1",
        key_env: Some("NVIDIA_API_KEY"),
        note: "NVIDIA AI Foundation models",
    },
    Provider {
        name: "opencode-go",
        wire: Wire::OpenaiChat,
        base_url: "https://opencode.ai/inference/go/openai/v1",
        key_env: Some("OPENCODE_API_KEY"),
        note: "OpenCode Go subscription gateway — open coding models",
    },
    Provider {
        name: "openrouter",
        wire: Wire::OpenaiChat,
        base_url: "https://openrouter.ai/api/v1",
        key_env: Some("OPENROUTER_API_KEY"),
        note: "gateway to 100+ models",
    },
    Provider {
        name: "together",
        wire: Wire::OpenaiChat,
        base_url: "https://api.together.xyz/v1",
        key_env: Some("TOGETHER_API_KEY"),
        note: "hosted open models",
    },
    Provider {
        name: "fireworks",
        wire: Wire::OpenaiChat,
        base_url: "https://api.fireworks.ai/inference/v1",
        key_env: Some("FIREWORKS_API_KEY"),
        note: "fast open-model inference",
    },
    // ── local runtimes (always last) ────────────────────────────
    Provider {
        name: "ollama",
        wire: Wire::OpenaiChat,
        base_url: "http://127.0.0.1:11434/v1",
        key_env: None,
        note: "local runtime",
    },
    Provider {
        name: "lmstudio",
        wire: Wire::OpenaiChat,
        base_url: "http://127.0.0.1:1234/v1",
        key_env: None,
        note: "local server",
    },
    Provider {
        name: "llamacpp",
        wire: Wire::OpenaiChat,
        base_url: "http://127.0.0.1:8080/v1",
        key_env: None,
        note: "llama.cpp server",
    },
    Provider {
        name: "vllm",
        wire: Wire::OpenaiChat,
        base_url: "http://127.0.0.1:8000/v1",
        key_env: None,
        note: "vLLM server (key optional at the endpoint)",
    },
];

/// Display tier for a vendor prefix — the stable ordering every catalog
/// listing and the model picker sort by (ascending; ties fall back to
/// the model id). Final order:
///
/// 1. official block — `anthropic` (0), `openai` (1), `google` (2),
///    `zai` (3), `zai-plan`/`zai-coding-plan` (4)
/// 2. community / alternative vendors (500)
/// 3. local runtimes + discovered endpoints — `ollama`, `lmstudio`,
///    `llamacpp`, `vllm` (900)
pub fn vendor_rank(vendor: &str) -> u32 {
    match vendor {
        "anthropic" => 0,
        "openai" => 1,
        "google" => 2,
        "zai" => 3,
        "zai-plan" | "zai-coding-plan" => 4,
        "ollama" | "lmstudio" | "llamacpp" | "vllm" => 900,
        _ => 500,
    }
}

/// Look a provider up by vendor prefix. Legacy z.ai aliases merge into
/// the canonical `zai` entry so `zhipu/model` and `zhipuai/model`
/// selectors keep synthesizing against the z.ai endpoint.
pub fn find(vendor: &str) -> Option<&'static Provider> {
    let canonical = match vendor {
        "zhipu" | "zhipuai" => "zai",
        other => other,
    };
    PROVIDERS.iter().find(|p| p.name == canonical)
}

/// Extra per-request header some vendors require on every call, when
/// any — the value is the request's session id (`SpeakRequest.session_id`,
/// usually the active strand). opencode-go routes by conversation and
/// rejects requests without a stable per-session id in
/// `x-opencode-session` (HTTP 400 `MissingSessionID`, observed live).
pub fn session_header(vendor: &str) -> Option<&'static str> {
    match vendor {
        "opencode-go" => Some("x-opencode-session"),
        _ => None,
    }
}

/// Synthesize a dialect for `provider/model` when the catalog has no row.
/// Context window unknown (0); conservative defaults elsewhere.
pub fn synthetic_dialect(provider: &Provider, model: &str) -> Dialect {
    Dialect {
        wire: provider.wire,
        base_url: Some(provider.base_url.to_string()),
        doc_url: None,
        api_key_env: provider.key_env.map(str::to_string),
        wire_model: Some(model.to_string()),
        discovery: None,
        context: 0,
        max_output: 8_192,
        efforts: Vec::new(),
        input: Vec::new(),
        cache: Default::default(),
        ratio: 3.6,
        price: Default::default(),
        priced: false,
        flags: Default::default(),
        effort_budgets: Default::default(),
        first_byte_timeout_ms: 120_000,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn registry_covers_expected_vendors() {
        for name in [
            "openai",
            "anthropic",
            "google",
            "groq",
            "deepseek",
            "qwen",
            "opencode-go",
            "openrouter",
            "ollama",
        ] {
            assert!(find(name).is_some(), "missing provider {name}");
        }
    }

    #[test]
    fn every_provider_name_is_unique() {
        let mut names: Vec<_> = PROVIDERS.iter().map(|p| p.name).collect();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len());
    }

    #[test]
    fn synthetic_dialect_carries_endpoint_and_key() {
        let p = find("groq").unwrap();
        let d = synthetic_dialect(p, "llama-3.3-70b-versatile");
        assert_eq!(
            d.base_url.as_deref(),
            Some("https://api.groq.com/openai/v1")
        );
        assert_eq!(d.api_key_env.as_deref(), Some("GROQ_API_KEY"));
        assert_eq!(d.wire, Wire::OpenaiChat);
        assert_eq!(d.wire_model.as_deref(), Some("llama-3.3-70b-versatile"));
    }

    #[test]
    fn opencode_go_selector_synthesizes_without_catalog_row() {
        // any opencode-go/<model> selector must resolve even when the
        // catalog has no row (gateway models churn faster than seeds)
        let p = find("opencode-go").unwrap();
        let d = synthetic_dialect(p, "some-future-model");
        assert_eq!(
            d.base_url.as_deref(),
            Some("https://opencode.ai/inference/go/openai/v1")
        );
        assert_eq!(d.api_key_env.as_deref(), Some("OPENCODE_API_KEY"));
        assert_eq!(d.wire, Wire::OpenaiChat);
    }

    #[test]
    fn session_header_maps_opencode_go_only() {
        assert_eq!(session_header("opencode-go"), Some("x-opencode-session"));
        assert_eq!(session_header("openai"), None);
        assert_eq!(session_header("openrouter"), None);
    }

    #[test]
    fn keyless_locals_have_no_env() {
        for name in ["ollama", "lmstudio", "llamacpp", "vllm"] {
            assert!(find(name).unwrap().key_env.is_none());
        }
    }

    #[test]
    fn official_block_comes_first_and_locals_last() {
        let official: Vec<_> = PROVIDERS.iter().take(5).map(|p| p.name).collect();
        assert_eq!(
            official,
            ["anthropic", "openai", "google", "zai", "zai-plan"]
        );
        let last = PROVIDERS.last().unwrap().name;
        assert_eq!(last, "vllm");
        for p in PROVIDERS.iter().skip(PROVIDERS.len() - 4) {
            assert_eq!(vendor_rank(p.name), 900, "{} must be local-tier", p.name);
        }
        for name in ["anthropic", "openai", "google", "zai", "zai-plan"] {
            assert!(vendor_rank(name) < 500, "{name} must be official-tier");
        }
    }

    #[test]
    fn zai_aliases_merge_into_canonical_provider() {
        for legacy in ["zhipu", "zhipuai"] {
            let p = find(legacy).unwrap();
            assert_eq!(p.name, "zai", "{legacy} must resolve to zai");
            assert_eq!(p.key_env, Some("ZHIPU_API_KEY"));
        }
    }

    #[test]
    fn vendor_rank_orders_catalog_listing() {
        let mut ids = [
            "zai-coding-plan/glm-4.7",
            "ollama/qwen3",
            "deepseek/deepseek-v3",
            "anthropic/claude-sonnet-5",
            "openai/gpt-5.1",
            "zai/glm-4.7",
            "google/gemini-3-pro",
        ];
        ids.sort_by_key(|id| {
            let vendor = id.split('/').next().unwrap_or(id);
            (vendor_rank(vendor), *id)
        });
        assert_eq!(
            ids,
            [
                "anthropic/claude-sonnet-5",
                "openai/gpt-5.1",
                "google/gemini-3-pro",
                "zai/glm-4.7",
                "zai-coding-plan/glm-4.7",
                "deepseek/deepseek-v3",
                "ollama/qwen3",
            ]
        );
    }
}
