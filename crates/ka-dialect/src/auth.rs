//! Token sources and the `.env` chain. Ladder: process env > `./.env` >
//! `~/.config/ka/.env` > passport registry (vendors served by a
//! registered [`TokenSource`], behind the `passport` cargo feature) >
//! OS keyring (`service "ka"`, user = the env var name). `!command`
//! indirection keeps secrets out of files. A missing or unusable keyring
//! backend silently falls through.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;

/// OS keyring service name passport tiers store credentials under
/// (ratified 2026-09-29: same class as ka's own `service "ka"` keys).
pub const PASSPORT_KEYRING_SERVICE: &str = "ka-passport";

/// Anthropic beta header value that unlocks OAuth-bearer requests on the
/// Messages API. The `anthropic_messages` wire attaches it (plus
/// `Authorization: Bearer`, never `x-api-key`) when the dialect's
/// `auth = "passport"` row carries a token; ka-passport re-exports the
/// constant so the CLI surfaces stay in lockstep.
pub const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";

/// The one auth abstraction core ka-dialect accepts (design shape from
/// research/ka-harness/sdk.md §6, ratified 2026-09-29). A passport crate
/// implements this and hooks the auth ladder between the dotenv and
/// keyring tiers via [`register_token_source`].
pub trait TokenSource: Send + Sync {
    /// Bearer token for the vendor's API, refreshing when stale.
    fn token(&self, vendor: &str) -> Result<String, AuthError>;
    /// Which vendors this source can serve (e.g. `["claude-plan"]`).
    fn vendors(&self) -> &[String];
}

/// Passport token failures. Never carries secret material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// No stored credentials for the vendor (never logged in, or logged
    /// out).
    NotLoggedIn {
        /// Vendor prefix asked for.
        vendor: String,
    },
    /// A credential refresh call failed (expired refresh token,
    /// transport error, provider refusal).
    Refresh {
        /// Vendor prefix asked for.
        vendor: String,
        /// Human-readable failure detail (safe to surface).
        message: String,
    },
    /// The OAuth flow itself failed (state mismatch, malformed callback,
    /// endpoint refusal) — distinct from refresh so the CLI can tell
    /// "log in again" from "your session broke".
    Protocol {
        /// Human-readable failure detail (safe to surface).
        message: String,
    },
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::NotLoggedIn { vendor } => {
                write!(f, "no passport credentials for vendor {vendor:?}")
            }
            AuthError::Refresh { vendor, message } => {
                write!(f, "passport refresh for {vendor:?} failed: {message}")
            }
            AuthError::Protocol { message } => write!(f, "passport protocol error: {message}"),
        }
    }
}
impl std::error::Error for AuthError {}

#[cfg(feature = "passport")]
static REGISTRY: LazyLock<std::sync::RwLock<Vec<std::sync::Arc<dyn TokenSource>>>> =
    LazyLock::new(|| std::sync::RwLock::new(Vec::new()));

/// Register a passport token source globally (ka-passport calls this
/// once at startup, behind the harness `passport` feature). Later
/// registrations for an already-served vendor shadow earlier ones —
/// the LAST registration for a vendor wins.
#[cfg(feature = "passport")]
pub fn register_token_source(source: std::sync::Arc<dyn TokenSource>) {
    if let Ok(mut registry) = REGISTRY.write() {
        registry.retain(|s| !s.vendors().iter().any(|v| source.vendors().contains(v)));
        registry.push(source);
    }
}

/// The registered source serving `vendor`, if any (passport tier only;
/// `None` builds without the feature — keys-only core).
#[cfg(feature = "passport")]
pub fn registered_source_for(vendor: &str) -> Option<std::sync::Arc<dyn TokenSource>> {
    REGISTRY
        .read()
        .ok()?
        .iter()
        .find(|s| s.vendors().iter().any(|v| v == vendor))
        .cloned()
}

/// Bearer token for a passport-served vendor via the registry. `None`
/// when the tier is off, no source serves the vendor, or the source
/// errors — callers fall through the rest of the ladder (which then
/// typically finds nothing, surfacing the instructive bootstrap error).
pub fn resolve_passport_token(vendor: &str) -> Option<String> {
    #[cfg(feature = "passport")]
    {
        registered_source_for(vendor)
            .and_then(|s| s.token(vendor).ok())
            .filter(|t| !t.is_empty())
    }
    #[cfg(not(feature = "passport"))]
    {
        let _ = vendor;
        None
    }
}

/// Whether a registered source serves `vendor` (passport tier; false
/// without the feature). The engine's bootstrap instructive error for
/// `auth = "passport"` rows keys off this.
pub fn passport_serves(vendor: &str) -> bool {
    #[cfg(feature = "passport")]
    {
        registered_source_for(vendor).is_some()
    }
    #[cfg(not(feature = "passport"))]
    {
        let _ = vendor;
        false
    }
}

static DOTENV: LazyLock<std::sync::RwLock<HashMap<String, String>>> =
    LazyLock::new(|| std::sync::RwLock::new(scan_dotenv()));

/// Insert or update a key in the in-process dotenv layer (used by the TUI
/// key prompt so a saved key works without a restart).
pub fn set_dotenv_key(key: &str, value: &str) {
    if let Ok(mut map) = DOTENV.write() {
        map.insert(key.to_string(), value.to_string());
    }
}

/// Whether a key variable is present anywhere ka would read it: the
/// process environment, the dotenv layer (`./.env`, `~/.config/ka/.env`),
/// or the OS keyring. Reading the map forces the lazy scan if it has not
/// run yet, so a key saved to the dotenv before this process started
/// counts as set — matching what [`resolve_token`] would find.
pub fn key_is_set(env_var: &str) -> bool {
    let spec = env_var.trim();
    !spec.is_empty()
        && (std::env::var(spec).is_ok_and(|v| !v.is_empty())
            || DOTENV
                .read()
                .ok()
                .is_some_and(|map| map.get(spec).is_some_and(|v| !v.is_empty()))
            || keyring_is_set(spec))
}

/// Keyring service name ka stores credential entries under.
const KEYRING_SERVICE: &str = "ka";

/// Best-effort keyring lookup for an env-var-named entry. Any failure —
/// no backend, no entry, blank value — returns `None` so callers fall
/// through as if the layer did not exist.
fn keyring_lookup(env_var: &str) -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, env_var).ok()?;
    match entry.get_password() {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Whether a keyring entry exists for the variable (best effort; a dead
/// backend counts as absent).
fn keyring_is_set(env_var: &str) -> bool {
    keyring_lookup(env_var).is_some()
}

fn scan_dotenv() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from(".env")];
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(home).join(".config/ka/.env"));
    }
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            if let Some((k, v)) = parse_env_line(line) {
                map.entry(k).or_insert(v);
            }
        }
    }
    map
}

fn parse_env_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    // shell-sourceable env files often carry `export KEY=...`
    let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
    let (key, value) = line.split_once('=')?;
    let key = key.trim().to_string();
    if key.is_empty() {
        return None;
    }
    let value = value.trim();
    let unquoted = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value);
    Some((key, unquoted.to_string()))
}

/// Resolve a token spec: an environment variable name (checked against
/// the process env first, then the lazily-scanned `.env` map, then —
/// when a registered [`TokenSource`] serves the name as a vendor — the
/// passport registry, before the OS keyring tier), or `!command` to run
/// (trimmed stdout). The map never overrides real variables.
pub fn resolve_token(spec: &str) -> Option<String> {
    let spec = spec.trim();
    if let Some(cmd) = spec.strip_prefix('!') {
        return run_command(cmd);
    }
    if let Ok(v) = std::env::var(spec) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    DOTENV
        .read()
        .ok()
        .and_then(|map| map.get(spec).cloned())
        .filter(|v| !v.is_empty())
        .or_else(|| resolve_passport_token(spec))
        .or_else(|| keyring_lookup(spec))
}

/// Testable core of [`resolve_token`] against an explicit fallback map
/// (standing in for the dotenv layer).
pub fn resolve_token_with(fallback: &HashMap<String, String>, spec: &str) -> Option<String> {
    let spec = spec.trim();
    if let Some(cmd) = spec.strip_prefix('!') {
        return run_command(cmd);
    }
    if let Ok(v) = std::env::var(spec) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    fallback
        .get(spec)
        .cloned()
        .filter(|v| !v.is_empty())
        .or_else(|| keyring_lookup(spec))
}

fn run_command(cmd: &str) -> Option<String> {
    let mut parts = cmd.split_whitespace();
    let program = parts.next()?;
    let output = std::process::Command::new(program)
        .args(parts)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!stdout.is_empty()).then_some(stdout)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    // ── MODEL-01 · passport registry (passport tier only) ───────────

    #[cfg(feature = "passport")]
    mod passport {
        use super::*;
        use std::sync::Arc;

        /// Serializes tests that touch the process-global registry
        /// (there is no unregister; one lock keeps orders deterministic).
        static REG_SER: LazyLock<parking_lot::Mutex<()>> =
            LazyLock::new(|| parking_lot::Mutex::new(()));

        struct FixedSource {
            vendors: Vec<String>,
            token: String,
            fail: bool,
        }

        impl FixedSource {
            fn serving(vendor: &str, token: &str) -> Self {
                Self {
                    vendors: vec![vendor.to_string()],
                    token: token.to_string(),
                    fail: false,
                }
            }
        }

        impl TokenSource for FixedSource {
            fn token(&self, _vendor: &str) -> Result<String, AuthError> {
                if self.fail {
                    Err(AuthError::Refresh {
                        vendor: _vendor.to_string(),
                        message: "refresh endpoint down".to_string(),
                    })
                } else {
                    Ok(self.token.clone())
                }
            }
            fn vendors(&self) -> &[String] {
                &self.vendors
            }
        }

        #[test]
        fn registry_consults_before_keyring_and_only_for_served_vendors() {
            let _guard = REG_SER.lock();
            let var = "claude-plan";
            // a keyring entry the registry must shadow for served vendors
            let keyring_seeded = match keyring::Entry::new("ka", var) {
                Ok(entry) => entry.set_password("from-keyring").is_ok(),
                Err(_) => false,
            };
            register_token_source(Arc::new(FixedSource::serving(var, "from-passport")));
            // served vendor: the registry answers before the keyring tier
            assert_eq!(
                resolve_token(var).as_deref(),
                Some("from-passport"),
                "registry sits before the keyring in the ladder"
            );
            assert!(passport_serves(var));
            assert!(resolve_passport_token(var).as_deref() == Some("from-passport"));
            // env still beats the registry (ladder order preserved)
            // — asserted indirectly: HOME resolves to env, not registry
            if std::env::var("HOME").is_ok() {
                assert_ne!(resolve_token("HOME").as_deref(), Some("from-passport"));
            }
            // an unserved vendor name never consults the registry
            assert!(!passport_serves("unserved-vendor-xyz"));
            assert_eq!(resolve_passport_token("unserved-vendor-xyz"), None);
            if keyring_seeded {
                // an unserved spec still reaches the keyring tier
                let key_var = "KA_TEST_REG_KEYRING_XYZ";
                if keyring::Entry::new("ka", key_var)
                    .ok()
                    .is_some_and(|e| e.set_password("from-keyring").is_ok())
                {
                    assert_eq!(resolve_token(key_var).as_deref(), Some("from-keyring"));
                    let _ = keyring::Entry::new("ka", key_var)
                        .unwrap()
                        .delete_credential();
                }
                let _ = keyring::Entry::new("ka", var).unwrap().delete_credential();
            }
        }

        #[test]
        fn failing_source_falls_through_to_lower_tiers() {
            let _guard = REG_SER.lock();
            let vendor = "failing-plan-vendor";
            register_token_source(Arc::new(FixedSource {
                vendors: vec![vendor.to_string()],
                token: String::new(),
                fail: true,
            }));
            // a failing source resolves to None (fall-through), not Err
            assert_eq!(resolve_passport_token(vendor), None);
            assert!(resolve_token(vendor).is_none());
        }

        #[test]
        fn later_registration_for_a_vendor_shadows_earlier() {
            let _guard = REG_SER.lock();
            let vendor = "shadowed-plan-vendor";
            register_token_source(Arc::new(FixedSource::serving(vendor, "first")));
            register_token_source(Arc::new(FixedSource::serving(vendor, "second")));
            assert_eq!(resolve_passport_token(vendor).as_deref(), Some("second"));
        }
    }

    #[test]
    fn passport_helpers_are_none_without_a_registry() {
        // holds under both feature sets: without the tier nothing serves;
        // with it, no source is registered for a unique vendor name
        assert!(!passport_serves("registry-absent-vendor-xyz"));
        assert_eq!(resolve_passport_token("registry-absent-vendor-xyz"), None);
    }

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn key_is_set_sees_env_and_rejects_blank() {
        // HOME is present in every test environment (see the note above);
        // a var set in the process counts as set
        if std::env::var("HOME").is_ok() {
            assert!(key_is_set("HOME"));
        }
        // a unique name absent from env and from the developer's dotenv
        assert!(!key_is_set("KA_TEST_KEY_ABSENT_XYZ_9183"));
        // blank specs are never set
        assert!(!key_is_set(""));
        assert!(!key_is_set("   "));
    }

    #[test]
    fn dotenv_lines_parse() {
        assert_eq!(parse_env_line("A=1"), Some(("A".into(), "1".into())));
        assert_eq!(
            parse_env_line(" B = \"two\" "),
            Some(("B".into(), "two".into()))
        );
        assert_eq!(parse_env_line("# comment"), None);
        assert_eq!(parse_env_line("noequals"), None);
        assert_eq!(parse_env_line("=value"), None);
        assert_eq!(
            parse_env_line("export OPENAI_API_KEY=sk-x"),
            Some(("OPENAI_API_KEY".into(), "sk-x".into()))
        );
    }

    #[test]
    fn process_env_beats_fallback_map() {
        // HOME is present in every test environment; the fallback map must
        // lose to the real variable without us mutating the environment
        // (set_var is unsafe in edition 2024).
        let fb = map(&[("HOME", "from-file")]);
        let resolved = resolve_token_with(&fb, "HOME");
        let real = std::env::var("HOME").ok();
        if real.is_some() {
            assert_eq!(resolved, real);
        } else {
            assert_eq!(resolved.as_deref(), Some("from-file"));
        }
    }

    #[test]
    fn fallback_map_used_when_env_missing() {
        let fb = map(&[("KA_TEST_TOK_FILE_XYZ", "from-file")]);
        assert_eq!(
            resolve_token_with(&fb, "KA_TEST_TOK_FILE_XYZ").as_deref(),
            Some("from-file")
        );
    }

    #[test]
    fn missing_is_none() {
        assert!(resolve_token_with(&HashMap::new(), "KA_TEST_TOK_NONE_XYZ").is_none());
        assert!(resolve_token_with(&HashMap::new(), "").is_none());
    }

    #[test]
    fn command_spec_runs() {
        assert_eq!(
            resolve_token_with(&HashMap::new(), "!echo ka-test-token").unwrap(),
            "ka-test-token"
        );
    }

    /// Skip guard for machines without a working keyring backend.
    fn seed_keyring(var: &str, secret: &str) -> bool {
        let Ok(entry) = keyring::Entry::new("ka", var) else {
            return false;
        };
        entry.set_password(secret).is_ok()
    }

    #[test]
    fn keyring_entry_resolves_as_last_layer() {
        let var = "KA_TEST_KEYRING_XYZ_1";
        if !seed_keyring(var, "from-keyring") {
            eprintln!("no usable keyring backend; skipping");
            return;
        }
        // no env, empty map → the keyring answers
        assert_eq!(
            resolve_token_with(&HashMap::new(), var).as_deref(),
            Some("from-keyring")
        );
        assert!(key_is_set(var));
        let _ = keyring::Entry::new("ka", var).unwrap().delete_credential();
    }

    #[test]
    fn fallback_map_beats_keyring() {
        let var = "KA_TEST_KEYRING_XYZ_2";
        if !seed_keyring(var, "from-keyring") {
            eprintln!("no usable keyring backend; skipping");
            return;
        }
        let fb = map(&[(var, "from-map")]);
        assert_eq!(
            resolve_token_with(&fb, var).as_deref(),
            Some("from-map"),
            "dotenv layer must shadow the keyring"
        );
        let _ = keyring::Entry::new("ka", var).unwrap().delete_credential();
    }

    #[test]
    fn keyring_absence_is_transparent() {
        // a variable with no env, no map and no entry stays unset and
        // unresolved even though the keyring layer is consulted
        assert!(!key_is_set("KA_TEST_KEYRING_ABSENT_XYZ_3"));
        assert!(resolve_token_with(&HashMap::new(), "KA_TEST_KEYRING_ABSENT_XYZ_3").is_none());
    }

    #[test]
    fn command_spec_failure_is_none() {
        assert!(
            resolve_token_with(&HashMap::new(), "!definitely-not-a-real-command-xyz").is_none()
        );
    }
}
