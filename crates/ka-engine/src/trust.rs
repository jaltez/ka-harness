//! The project trust store: directories whose `.ka/` conventions (local
//! config, skills, hooks) ka may load. One shared JSON file —
//! `$XDG_STATE_HOME/ka/trust.json` (falling back to `~/.local/state`),
//! a JSON array of canonicalized directory paths — written when the user
//! approves a project and consulted before any project-scope `.ka/`
//! content is loaded.
//!
//! Path derivation matches the historical CLI implementation byte for
//! byte, so stores written by older versions read unchanged.

use std::path::{Path, PathBuf};

/// Resolve the trust store file from the environment, returning `None`
/// (fail closed) when neither `XDG_STATE_HOME` nor `HOME` is set —
/// never falling back to a world-writable temp directory.
fn resolve_trust_file() -> Option<PathBuf> {
    // gated like [`test_support`] itself, not `cfg(test)`: integration
    // suites link the lib WITHOUT `cfg(test)`, so a plain test gate
    // would leave their `with_trust_file` redirection inert and point
    // them at the real store
    #[cfg(any(test, feature = "test-util"))]
    if let Some(file) = test_support::current_file() {
        return Some(file);
    }
    let state_home = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })?;
    Some(trust_file_under(state_home))
}

/// The trust store file, derived from the environment. Returns an empty
/// path when no state home can be resolved (never `/tmp`).
pub fn trust_file() -> PathBuf {
    resolve_trust_file().unwrap_or_default()
}

/// Trust store path beneath an explicit state home.
fn trust_file_under(state_home: PathBuf) -> PathBuf {
    state_home.join("ka/trust.json")
}

/// Read the trust store; missing, unresolvable, or malformed file = empty store.
pub fn load_trust() -> Vec<PathBuf> {
    resolve_trust_file()
        .map(|file| load_trust_at(&file))
        .unwrap_or_default()
}

pub fn load_trust_at(file: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Persist the trust store (creating parent directories; best-effort).
/// Fails closed if no user state home is resolvable.
pub fn save_trust(dirs: &[PathBuf]) {
    if let Some(file) = resolve_trust_file() {
        save_trust_at(&file, dirs);
    }
}

pub fn save_trust_at(file: &Path, dirs: &[PathBuf]) {
    let Some(parent) = file.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(json) = serde_json::to_string_pretty(dirs) else {
        return;
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(".trust.json.tmp.{}.{nanos}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let Ok(mut f) = opts.open(&tmp) else {
        return;
    };
    use std::io::Write;
    if f.write_all(json.as_bytes()).is_ok() && f.sync_all().is_ok() {
        drop(f);
        if std::fs::rename(&tmp, file).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    } else {
        drop(f);
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Whether `cwd` is in `dirs` (canonicalized on both sides, falling back
/// to the literal path when canonicalization fails).
pub fn trusted_in(cwd: &Path, dirs: &[PathBuf]) -> bool {
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    dirs.contains(&canonical)
}

/// Whether the project-scope `.ka/` content of `cwd` may load. Both the
/// question and the approval below resolve against the project root
/// (nearest `.git` ancestor, else `cwd`): one approval covers sessions
/// launched from any directory inside the same project, and the store
/// entry matches regardless of where the trust prompt fired.
pub fn project_trusted(cwd: &Path) -> bool {
    trusted_in(&crate::project_root(cwd), &load_trust())
}

/// Record `cwd` as trusted in the store (canonicalized, deduplicated).
/// Approving a project root also folds away superseded launch-dir
/// entries from before root anchoring — except entries that are their
/// own repos (submodules, vendored checkouts), which stay approved.
pub fn approve(cwd: &Path) {
    let mut dirs = load_trust();
    approve_into(&mut dirs, &crate::project_root(cwd));
    save_trust(&dirs);
}

/// Record `cwd` as trusted in an explicit store file (tests).
pub fn approve_at(file: &Path, cwd: &Path) {
    let mut dirs = load_trust_at(file);
    approve_into(&mut dirs, cwd);
    save_trust_at(file, &dirs);
}

fn approve_into(dirs: &mut Vec<PathBuf>, cwd: &Path) {
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    dirs.retain(|d| {
        // keep the root itself, everything outside it, and nested repos
        d == &canonical || !d.starts_with(&canonical) || d.join(".git").exists()
    });
    if !dirs.contains(&canonical) {
        dirs.push(canonical);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::test_support::{uniq, with_trust_file};
    use super::*;

    #[test]
    fn approve_folds_superseded_launch_entries_but_keeps_nested_repos() {
        let dir = uniq("fold");
        let root = dir.join("proj");
        let launch = root.join("crates/ka");
        let nested = root.join("vendor/sub");
        std::fs::create_dir_all(&launch).unwrap();
        std::fs::create_dir_all(nested.join(".git")).unwrap();
        let file = dir.join("state/ka/trust.json");
        let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
        // pre-root-era entries: the old launch dir + a nested repo
        save_trust_at(&file, &[canon(&launch), canon(&nested)]);
        approve_at(&file, &root);
        let dirs = load_trust_at(&file);
        assert!(dirs.contains(&canon(&root)), "the root is approved");
        assert!(
            !dirs.contains(&canon(&launch)),
            "a superseded launch-dir entry folds away"
        );
        assert!(
            dirs.contains(&canon(&nested)),
            "a nested repo (its own .git) stays approved"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn approve_then_project_trusted_roundtrip() {
        with_trust_file(|file| {
            let proj = uniq("approve");
            assert!(!project_trusted(&proj), "fresh store: untrusted");
            approve(&proj);
            assert!(project_trusted(&proj), "approved project is trusted");
            // the store was written through the shared save path
            assert!(file.is_file());
            // a second approve does not duplicate the entry
            approve(&proj);
            assert_eq!(load_trust().len(), 1);
            // canonicalized: a symlink to the same directory also trusts
            #[cfg(unix)]
            {
                let link = proj.with_extension("link");
                std::os::unix::fs::symlink(&proj, &link).unwrap();
                assert!(project_trusted(&link));
            }
        });
    }

    #[test]
    fn legacy_store_format_still_reads() {
        with_trust_file(|file| {
            let proj = uniq("legacy");
            // byte-format fixture: what the old CLI save_trust wrote
            // (serde_json::to_string_pretty of a Vec<PathBuf>)
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(
                file,
                format!(
                    "[\n  {}\n]",
                    serde_json::json!(proj.to_string_lossy().into_owned())
                ),
            )
            .unwrap();
            assert!(project_trusted(&proj), "pre-existing store entries trust");
        });
    }

    #[test]
    fn missing_or_malformed_store_is_untrusted() {
        with_trust_file(|file| {
            let proj = uniq("nostore");
            assert!(!file.exists());
            assert!(!project_trusted(&proj));
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "not json").unwrap();
            assert!(!project_trusted(&proj));
        });
    }

    #[test]
    fn approve_at_matches_shared_format() {
        with_trust_file(|file| {
            let proj = uniq("approve-at");
            approve_at(file, &proj);
            assert!(project_trusted(&proj));
            assert!(trusted_in(&proj, &load_trust_at(file)));
        });
    }
}

/// Shared with sibling test modules (conventions, fshooks): a test-only
/// redirection of the trust store path. The workspace forbids `unsafe`,
/// so the environment itself is never mutated — [`trust_file`] consults
/// this override when compiled for tests.
#[cfg(any(test, feature = "test-util"))]
pub mod test_support {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::{Path, PathBuf};

    static TRUST_FILE: parking_lot::Mutex<Option<PathBuf>> = parking_lot::Mutex::new(None);

    static FILE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    pub fn current_file() -> Option<PathBuf> {
        TRUST_FILE.lock().clone()
    }

    pub fn uniq(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ka-trust-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Runs `f` with the trust store redirected to a fresh temp file.
    /// Serialized: the override is process-global.
    pub fn with_trust_file<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _lock = FILE_LOCK.lock();
        let dir = uniq("store");
        let file = dir.join("state/ka/trust.json");
        *TRUST_FILE.lock() = Some(file.clone());
        let out = f(&file);
        *TRUST_FILE.lock() = None;
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    /// Redirect the trust store until the returned guard drops — usable
    /// across `.await` points in async tests. Holds [`FILE_LOCK`] for the
    /// guard's lifetime, so it is serialized against `with_trust_file`
    /// and other guards.
    pub fn trust_guard() -> (PathBuf, TrustGuard) {
        let lock = FILE_LOCK.lock();
        let dir = uniq("store");
        let file = dir.join("state/ka/trust.json");
        *TRUST_FILE.lock() = Some(file.clone());
        (
            file,
            TrustGuard {
                dir: Some(dir),
                _lock: Some(lock),
            },
        )
    }

    pub struct TrustGuard {
        dir: Option<PathBuf>,
        _lock: Option<parking_lot::MutexGuard<'static, ()>>,
    }

    impl Drop for TrustGuard {
        fn drop(&mut self) {
            *TRUST_FILE.lock() = None;
            if let Some(dir) = self.dir.take() {
                let _ = std::fs::remove_dir_all(dir);
            }
            drop(self._lock.take());
        }
    }
}
