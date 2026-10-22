//! On-disk MCP tool-schema cache: the tool list a server advertised
//! last time, keyed by a hash of the server's config. On startup a
//! cache hit registers the hands immediately — a slow (or temporarily
//! dead) server never blocks the session start or shifts the tool
//! list between runs (provider prompt-cache stability) — while a
//! background handshake verifies the list and swaps it only on a real
//! change. Fail-closed frame: the cached list only advertises tools;
//! every call still goes through the live gate, and an unverifiable
//! entry is a cache miss, never a guess.

use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hasher};
use std::path::PathBuf;

use crate::mcp::{McpServerConfig, McpTool};

/// One cached server: the config hash it was listed under + the tools.
#[derive(serde::Serialize, serde::Deserialize)]
struct Entry {
    hash: u64,
    tools: Vec<McpTool>,
}

/// The whole cache file: server name → entry.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Cache {
    servers: BTreeMap<String, Entry>,
}

/// Where the cache lives: `$XDG_STATE_HOME/ka/mcp-tools.json`
/// (fallback `~/.local/state/ka/…`, temp-dir last resort).
fn cache_file() -> PathBuf {
    #[cfg(any(test, feature = "test-util"))]
    if let Some(f) = test_support::current_file() {
        return f;
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| {
                let mut p = PathBuf::from(h);
                p.push(".local/state");
                p
            })
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("ka").join("mcp-tools.json")
}

/// Stable hash of a server's launch config: any change to command,
/// args, env, url, or headers is a different server, and the cached
/// list must not be trusted across it. serde_json's map ordering is
/// sorted, so the canonical string is deterministic.
pub fn config_hash(cfg: &McpServerConfig) -> u64 {
    let Ok(value) = serde_json::to_value(cfg) else {
        return 0;
    };
    let Ok(canonical) = serde_json::to_string(&value) else {
        return 0;
    };
    let mut h = DefaultHasher::new();
    h.write(canonical.as_bytes());
    h.finish()
}

/// The cached tool list for `cfg`, when one exists under the exact
/// same config hash. Corrupt files and hash mismatches are misses.
pub fn load_tools(cfg: &McpServerConfig) -> Option<Vec<McpTool>> {
    let text = std::fs::read_to_string(cache_file()).ok()?;
    let cache: Cache = serde_json::from_str(&text).ok()?;
    let entry = cache.servers.get(&cfg.name)?;
    (entry.hash == config_hash(cfg) && !entry.tools.is_empty()).then(|| {
        let mut tools = entry.tools.clone();
        // Caches written before wire names were sanitized carry dotted
        // `server.tool` names; re-derive so they never reach the wire.
        for t in &mut tools {
            t.name = crate::mcp::wire_name(&cfg.name, &t.raw_name);
        }
        tools
    })
}

/// Remember `tools` under `cfg`'s hash — best-effort, read-modify-write
/// so sibling servers keep their entries. A failed write is a cache
/// miss next time, nothing more.
pub fn store_tools(cfg: &McpServerConfig, tools: &[McpTool]) {
    let mut cache = std::fs::read_to_string(cache_file())
        .ok()
        .and_then(|t| serde_json::from_str::<Cache>(&t).ok())
        .unwrap_or_default();
    cache.servers.insert(
        cfg.name.clone(),
        Entry {
            hash: config_hash(cfg),
            tools: tools.to_vec(),
        },
    );
    let Ok(text) = serde_json::to_string(&cache) else {
        return;
    };
    if let Some(parent) = cache_file().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(cache_file(), text);
}

/// Test-only redirection of the cache path (the trust-store idiom:
/// no env mutation, process-global, serialized).
#[cfg(any(test, feature = "test-util"))]
pub mod test_support {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::PathBuf;

    static CACHE_FILE: parking_lot::Mutex<Option<PathBuf>> = parking_lot::Mutex::new(None);

    static FILE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    pub fn current_file() -> Option<PathBuf> {
        CACHE_FILE.lock().clone()
    }

    /// Redirect the cache to a fresh temp file until the guard drops —
    /// usable across `.await` points; serialized process-globally.
    pub fn cache_guard() -> CacheGuard {
        let lock = FILE_LOCK.lock();
        let dir = std::env::temp_dir().join(format!(
            "ka-mcpcache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        *CACHE_FILE.lock() = Some(dir.join("mcp-tools.json"));
        CacheGuard {
            dir: Some(dir),
            _lock: Some(lock),
        }
    }

    pub struct CacheGuard {
        dir: Option<PathBuf>,
        _lock: Option<parking_lot::MutexGuard<'static, ()>>,
    }

    impl Drop for CacheGuard {
        fn drop(&mut self) {
            *CACHE_FILE.lock() = None;
            if let Some(dir) = self.dir.take() {
                let _ = std::fs::remove_dir_all(dir);
            }
            drop(self._lock.take());
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn cfg(name: &str, command: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            command: Some(command.into()),
            args: vec!["--stdio".into()],
            env: std::collections::HashMap::new(),
            url: None,
            headers: Vec::new(),
        }
    }

    fn tool(name: &str) -> McpTool {
        McpTool {
            name: format!("demo__{name}"),
            raw_name: name.into(),
            description: "a tool".into(),
            schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn roundtrip_hits_and_config_changes_miss() {
        let _g = test_support::cache_guard();
        let c = cfg("demo", "server-bin");
        store_tools(&c, &[tool("fetch"), tool("probe")]);
        let back = load_tools(&c).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].name, "demo__fetch");
        // any config change is a miss: different args → different hash
        let mut changed = cfg("demo", "server-bin");
        changed.args.push("--verbose".into());
        assert!(load_tools(&changed).is_none());
        // a different server name is a miss
        assert!(load_tools(&cfg("other", "server-bin")).is_none());
    }

    #[test]
    fn legacy_dotted_names_are_rederived_on_load() {
        let _g = test_support::cache_guard();
        let c = cfg("demo", "server-bin");
        let mut legacy = tool("fetch");
        legacy.name = "demo.fetch".into(); // written before wire-name sanitization
        store_tools(&c, &[legacy]);
        let back = load_tools(&c).unwrap();
        assert_eq!(back[0].name, "demo__fetch");
        assert_eq!(back[0].raw_name, "fetch");
    }

    #[test]
    fn corrupt_file_is_a_miss_not_an_error() {
        let _g = test_support::cache_guard();
        std::fs::write(cache_file(), "{not json").unwrap();
        assert!(load_tools(&cfg("demo", "server-bin")).is_none());
    }

    #[test]
    fn store_keeps_sibling_entries() {
        let _g = test_support::cache_guard();
        store_tools(&cfg("a", "bin-a"), &[tool("fa")]);
        store_tools(&cfg("b", "bin-b"), &[tool("fb")]);
        assert_eq!(load_tools(&cfg("a", "bin-a")).unwrap().len(), 1);
        assert_eq!(load_tools(&cfg("b", "bin-b")).unwrap().len(), 1);
    }
}
