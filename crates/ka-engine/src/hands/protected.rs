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

/// One path component queued for resolution (owned: a symlink target is
/// spliced into the queue).
enum Part {
    Root,
    Prefix(std::ffi::OsString),
    Parent,
    Name(std::ffi::OsString),
}

/// Canonicalize `path` component by component: expand every symlink —
/// including a *dangling* one, whose target may not exist yet — and
/// apply `..` to the resolved prefix, never lexically across a link.
/// `realpath -m` semantics with a reader that cannot be fooled by a
/// link the write would follow (a link into `~/.ssh` or `.git/hooks`
/// must stay protected whether or not its target exists).
pub(crate) fn canonicalize_deepest(path: &Path) -> PathBuf {
    /// Same ceiling as Linux's `ELOOP`; past it we stop following and
    /// keep the literal remainder (a cycle cannot be resolved).
    const MAX_SYMLINK_HOPS: u32 = 40;
    fn push(todo: &mut Vec<Part>, p: &Path) {
        for comp in p.components() {
            match comp {
                Component::RootDir => todo.push(Part::Root),
                Component::CurDir => {}
                Component::ParentDir => todo.push(Part::Parent),
                Component::Prefix(pr) => todo.push(Part::Prefix(pr.as_os_str().to_os_string())),
                Component::Normal(name) => todo.push(Part::Name(name.to_os_string())),
            }
        }
    }
    let mut todo: Vec<Part> = Vec::new();
    push(&mut todo, path);
    todo.reverse();
    let mut out = PathBuf::new();
    let mut hops = 0u32;
    while let Some(part) = todo.pop() {
        match part {
            Part::Root => out.push(std::path::MAIN_SEPARATOR.to_string()),
            Part::Prefix(prefix) => out.push(prefix),
            Part::Parent => {
                out.pop();
            }
            Part::Name(name) => {
                out.push(&name);
                let is_link = std::fs::symlink_metadata(&out)
                    .map(|meta| meta.file_type().is_symlink())
                    .unwrap_or(false);
                if !is_link || hops >= MAX_SYMLINK_HOPS {
                    continue;
                }
                let Ok(target) = std::fs::read_link(&out) else {
                    continue;
                };
                hops += 1;
                out.pop();
                if target.is_absolute() {
                    out = PathBuf::new();
                }
                let mut spliced: Vec<Part> = Vec::new();
                push(&mut spliced, &target);
                for part in spliced.into_iter().rev() {
                    todo.push(part);
                }
            }
        }
    }
    out
}

fn norm_lower(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn path_eq_ci(a: &Path, b: &Path) -> bool {
    norm_lower(a) == norm_lower(b)
}

fn path_starts_with_dir_ci(path: &Path, dir: &Path) -> bool {
    let p = norm_lower(path);
    let d = norm_lower(dir);
    let d = d.trim_end_matches('/');
    if d.is_empty() {
        return false;
    }
    p == d || p.strip_prefix(d).is_some_and(|rest| rest.starts_with('/'))
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(|v| canonicalize_deepest(&PathBuf::from(v)))
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
    let path = canonicalize_deepest(&abs);
    let canon_cwd = canonicalize_deepest(cwd);
    let home = std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| canonicalize_deepest(&PathBuf::from(h)))
        .unwrap_or_default();

    let rel_lower = path
        .strip_prefix(&canon_cwd)
        .map(norm_lower)
        .unwrap_or_else(|_| norm_lower(&path));
    let full_lower = norm_lower(&path);

    // git internals: hooks execute code, config rewrites remotes
    if rel_lower.starts_with(".git/hooks/")
        || rel_lower == ".git/hooks"
        || rel_lower == ".git/config"
        || rel_lower.starts_with(".git/modules/")
        || full_lower.contains("/.git/hooks/")
        || full_lower.ends_with("/.git/hooks")
        || full_lower.ends_with("/.git/config")
        || full_lower.contains("/.git/modules/")
    {
        return Some("git internals (.git/hooks/.git/config) can execute code or rewrite remotes");
    }
    // CI workflows execute arbitrary code on push/PR
    if rel_lower.starts_with(".github/workflows/")
        || rel_lower == ".github/workflows"
        || full_lower.contains("/.github/workflows/")
        || full_lower.ends_with("/.github/workflows")
    {
        return Some("GitHub Actions workflow files can execute arbitrary code");
    }
    // SSH & GPG & cloud credentials
    if !home.as_os_str().is_empty() {
        if path_starts_with_dir_ci(&path, &home.join(".ssh")) {
            return Some("SSH keys and configuration");
        }
        if path_starts_with_dir_ci(&path, &home.join(".gnupg")) {
            return Some("GPG keys and configuration");
        }
        if path_starts_with_dir_ci(&path, &home.join(".aws")) {
            return Some("AWS credentials and configuration");
        }
        if path_eq_ci(&path, &home.join(".config/fish/config.fish")) {
            return Some("shell startup files / global git config");
        }
        // shell startup files + global git/package credentials
        let home_files = [
            ".bashrc",
            ".zshrc",
            ".profile",
            ".bash_profile",
            ".bash_logout",
            ".zshenv",
            ".zprofile",
            ".zlogin",
            ".gitconfig",
            ".netrc",
            ".npmrc",
        ];
        if path
            .parent()
            .is_some_and(|parent| path_eq_ci(parent, &home))
            && path
                .file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|name| home_files.iter().any(|n| name.eq_ignore_ascii_case(n)))
        {
            return Some("shell startup files / global git config");
        }
    }
    if let Some(xdg_cfg) = env_dir("XDG_CONFIG_HOME") {
        if path_eq_ci(&path, &xdg_cfg.join("fish/config.fish")) {
            return Some("shell startup files / global git config");
        }
    }

    // ka's own configuration, credentials, and trust state — the
    // project layer anchors at the root project, matching
    // [`crate::config::save_project_permission`]
    let ka_project = canonicalize_deepest(&crate::project_root(cwd).join(".ka/ka.toml"));
    if path_eq_ci(&path, &ka_project) || full_lower.ends_with("/.ka/ka.toml") {
        return Some("ka configuration (could grant itself permissions)");
    }

    let mut config_dirs = Vec::new();
    if let Some(xdg_cfg) = env_dir("XDG_CONFIG_HOME") {
        config_dirs.push(xdg_cfg.join("ka"));
    }
    if !home.as_os_str().is_empty() {
        config_dirs.push(home.join(".config/ka"));
    }
    if let Some(data_dir) = env_dir("KA_DATA_DIR") {
        config_dirs.push(data_dir.clone());
        if path_eq_ci(&path, &data_dir.join("trust.json")) {
            return Some("ka trust store");
        }
        if path_starts_with_dir_ci(&path, &data_dir.join("mcp-tokens")) {
            return Some("ka credential file");
        }
    }
    for dir in &config_dirs {
        if path_eq_ci(&path, &dir.join("ka.toml")) {
            return Some("ka configuration (could grant itself permissions)");
        }
        if path_eq_ci(&path, &dir.join(".env")) {
            return Some("ka credential file");
        }
    }

    let mut state_dirs = Vec::new();
    if let Some(xdg_state) = env_dir("XDG_STATE_HOME") {
        state_dirs.push(xdg_state.join("ka"));
    }
    if !home.as_os_str().is_empty() {
        state_dirs.push(home.join(".local/state/ka"));
    }
    for dir in &state_dirs {
        if path_eq_ci(&path, &dir.join("trust.json")) {
            return Some("ka trust store");
        }
    }
    None
}

/// Whether a bash redirection token targets a protected path. Works on
/// the raw token (op prefix stripped by the caller's context is fine):
/// distinctive substrings after `~` expansion (ASCII case-insensitive).
pub fn redirect_protected(token: &str) -> bool {
    let mut candidates = vec![token.to_ascii_lowercase().replace('\\', "/")];
    if let Some(rest) = token.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            candidates.push(
                format!("{home}/{rest}")
                    .to_ascii_lowercase()
                    .replace('\\', "/"),
            );
        }
    }
    const MARKERS: &[&str] = &[
        ".git/hooks",
        ".git/config",
        ".git/modules",
        ".github/workflows",
        ".ssh/",
        ".gnupg/",
        ".aws/",
        ".bashrc",
        ".zshrc",
        ".profile",
        ".bash_profile",
        ".bash_logout",
        ".zshenv",
        ".zprofile",
        ".zlogin",
        ".config/fish/config.fish",
        ".gitconfig",
        ".netrc",
        ".npmrc",
        ".ka/ka.toml",
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

    #[test]
    fn symlinks_resolve_even_when_the_target_is_missing() {
        let base = std::env::temp_dir().join(format!("ka-canon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ws = base.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let home = home();
        // A dangling link into ~/.ssh: the write would CREATE the target.
        let target = home
            .join(".ssh")
            .join(format!("ka-review-{}.key", std::process::id()));
        std::os::unix::fs::symlink(&target, ws.join("dang")).unwrap();
        assert!(
            reason(&ws, "dang").is_some(),
            "a dangling symlink into ~/.ssh must stay protected"
        );
        // `..` applies AFTER the link resolves, not lexically in the input:
        // hlink/../.bashrc is ~/.bashrc, not <cwd>/.bashrc.
        std::os::unix::fs::symlink(
            home.join(format!("ka-missing-{}", std::process::id())),
            ws.join("hlink"),
        )
        .unwrap();
        assert!(
            reason(&ws, "hlink/../.bashrc").is_some(),
            "link/../.bashrc resolves to ~/.bashrc, not <cwd>/.bashrc"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
