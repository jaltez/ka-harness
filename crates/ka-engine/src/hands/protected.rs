//! Protected paths: file writes that always prompt, even in free mode
//! (the claude-code protected-paths idea). An injected prompt that
//! talks the model into editing `.git/hooks/pre-commit`,
//! `~/.bashrc`, or ka's own config must survive a human's eyes —
//! permission rules and modes do not lift this, only the ask's
//! "allow" does.
//!
//! Bash redirection gets the same treatment at the token level
//! ([`redirect_protected`]): `echo evil > ~/.bashrc` is a hardstop.

use std::path::{Component, Path, PathBuf};

/// Lexically normalize `..`/`.` without touching the filesystem (the
/// target may not exist yet; `canonicalize` would miss new files).
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Why a write to `raw` (as passed to edit/write, resolved against
/// `cwd`) is protected. `None` = ordinary path.
pub fn reason(cwd: &Path, raw: &str) -> Option<&'static str> {
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        std::env::var("HOME")
            .map(|h| Path::new(&h).join(rest))
            .unwrap_or_else(|_| PathBuf::from(raw))
    } else {
        PathBuf::from(raw)
    };
    let abs = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let path = normalize(&abs);
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();

    // git internals: hooks execute code, config rewrites remotes
    let git_internals = |p: &Path| -> bool {
        let rel = p.strip_prefix(cwd).unwrap_or(p);
        let s = rel.to_string_lossy().replace('\\', "/");
        // absolute paths outside cwd carry their own markers
        let full = p.to_string_lossy().replace('\\', "/");
        s.starts_with(".git/hooks/")
            || s == ".git/config"
            || s.starts_with(".git/modules/")
            || full.contains("/.git/hooks/")
            || full.ends_with("/.git/config")
            || full.contains("/.git/modules/")
    };
    if git_internals(&path) {
        return Some("git internals (.git/hooks/.git/config) can execute code or rewrite remotes");
    }
    // SSH material
    if !home.as_os_str().is_empty() && path.starts_with(home.join(".ssh")) {
        return Some("SSH keys and configuration");
    }
    // shell startup files + global git config
    let rc_names = [
        ".bashrc",
        ".zshrc",
        ".profile",
        ".bash_profile",
        ".zshenv",
        ".gitconfig",
    ];
    if path.parent() == Some(home.as_path())
        && rc_names
            .iter()
            .any(|n| Some(*n) == path.file_name().and_then(|f| f.to_str()))
    {
        return Some("shell startup files / global git config");
    }
    // ka's own configuration, credentials, and trust state — the
    // project layer anchors at the root project, matching
    // [`crate::config::save_project_permission`]
    let ka_project = crate::project_root(cwd).join(".ka/ka.toml");
    let ka_user = home.join(".config/ka/ka.toml");
    let ka_env = home.join(".config/ka/.env");
    let ka_trust = home.join(".local/state/ka/trust.json");
    if path == ka_project || path == ka_user {
        return Some("ka configuration (could grant itself permissions)");
    }
    if path == ka_env {
        return Some("ka credential file");
    }
    if path == ka_trust {
        return Some("ka trust store");
    }
    None
}

/// Whether a bash redirection token targets a protected path. Works on
/// the raw token (op prefix stripped by the caller's context is fine):
/// distinctive substrings after `~` expansion.
pub fn redirect_protected(token: &str) -> bool {
    let mut candidates = vec![token.to_string()];
    if let Some(rest) = token.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            candidates.push(format!("{home}/{rest}"));
        }
    }
    const MARKERS: &[&str] = &[
        ".git/hooks",
        ".git/config",
        ".git/modules",
        ".ssh/",
        ".bashrc",
        ".zshrc",
        ".profile",
        ".bash_profile",
        ".zshenv",
        ".gitconfig",
        "config/ka/ka.toml",
        "config/ka/.env",
        "ka/trust.json",
    ];
    candidates
        .iter()
        .any(|c| MARKERS.iter().any(|m| c.contains(m)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn home() -> PathBuf {
        std::env::var("HOME").map(PathBuf::from).unwrap()
    }

    #[test]
    fn git_internals_ssh_and_rc_files_are_protected() {
        let cwd = Path::new("/w/proj");
        let cases: &[(&str, Option<&str>)] = &[
            (".git/hooks/pre-commit", Some("git internals")),
            ("src/../.git/config", Some("git internals")),
            (".git/modules/x/config", Some("git internals")),
            ("/tmp/other/.git/hooks/pre-commit", Some("git internals")),
            ("/tmp/other/.git/config", Some("git internals")),
            ("src/main.rs", None),
            (".gitignore", None),
        ];
        for (raw, want) in cases {
            let got = reason(cwd, raw).map(|s| s.to_string());
            match want {
                Some(tag) => assert!(
                    got.as_deref().is_some_and(|g| g.contains(tag)),
                    "{raw}: got {got:?}"
                ),
                None => assert_eq!(got, None, "{raw}: got {got:?}"),
            }
        }
    }

    #[test]
    fn home_paths_expand() {
        let cwd = Path::new("/w/proj");
        assert!(
            reason(cwd, "~/.ssh/authorized_keys").is_some(),
            "~ expands to HOME"
        );
        assert!(reason(cwd, "~/.bashrc").is_some());
        assert!(reason(cwd, "~/.gitconfig").is_some());
        assert!(reason(cwd, "~/regular.txt").is_none());
        // ka's own config and credentials
        assert!(reason(cwd, ".ka/ka.toml").is_some());
        assert!(reason(cwd, "~/.config/ka/.env").is_some());
        let abs_user = home().join(".config/ka/ka.toml");
        assert!(reason(cwd, abs_user.to_str().unwrap_or("")).is_some());
    }

    #[test]
    fn redirect_tokens_are_caught() {
        assert!(redirect_protected(">~/.bashrc"));
        assert!(redirect_protected("~/.ssh/id_rsa.pub"));
        assert!(redirect_protected("$HOME/.gitconfig"));
        assert!(redirect_protected(".git/hooks/pre-commit"));
        assert!(!redirect_protected("/tmp/out.txt"));
        assert!(!redirect_protected("src/main.rs"));
    }
}
