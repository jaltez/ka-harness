//! Filesystem sandbox policy for child processes.
//!
//! `[sandbox] mode = "fs"` restricts bash children to: read everything,
//! write only the working directory, the OS temp dir, and XDG state/cache
//! dirs. Enforcement prefers an external sandbox tool that applies
//! `PR_SET_NO_NEW_PRIVS` semantics (bubblewrap, then firejail); when
//! neither tool exists, in-kernel landlock takes over via a self re-exec
//! trampoline — the hidden `ka ka-sandbox-exec <policy-json> -- <argv…>`
//! subcommand applies the ruleset to itself and execs the real command
//! (the workspace forbids `unsafe`, so rules cannot ride
//! `Command::pre_exec`). When none of the three is available the policy
//! FAILS CLOSED — the command refuses. Default mode is `off`, so
//! behavior is unchanged unless the user opts in.
//!
//! **Sandbox expansion (roadmap 9.1):** when a command deterministically
//! needs more than the policy allows (a redirection outside the write
//! allowlist, a network-touching program under a network-denying
//! backend, env assignments a clearing backend would strip), the engine
//! computes the exact missing [`Grants`] BEFORE running, asks once, and
//! re-runs with an expanded per-call policy. Grants are computed from
//! command analysis, never from failure output — nothing expands without
//! an explicit ask, and non-computable needs stay denied (fail-closed).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod trampoline;

pub use trampoline::TRAMPOLINE_ARG;

/// Sandbox mode (`[sandbox] mode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// No sandboxing (default).
    #[default]
    Off,
    /// Filesystem allowlist sandbox.
    Fs,
}

/// The resolved sandbox policy handed to the bash hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Policy {
    /// Run the command as-is.
    Off,
    /// Wrap the command in an fs-restricting launcher.
    Fs {
        /// Writable directories (cwd, XDG state/cache, /tmp, plus any
        /// `[sandbox] allow_write` entries and granted expansions).
        allow_write: Vec<PathBuf>,
        /// Network egress permitted (sandbox-expansion grant only; the
        /// base policy always denies where the backend can).
        allow_net: bool,
        /// Env var names passed through to the child (expansion grant;
        /// clearing backends re-set these from the parent environment).
        pass_env: Vec<String>,
    },
}

impl Policy {
    /// The stock `fs` policy for an allowlist (no network, no env).
    pub fn fs(allow_write: Vec<PathBuf>) -> Self {
        Policy::Fs {
            allow_write,
            allow_net: false,
            pass_env: Vec::new(),
        }
    }
}

/// One sandbox expansion: the exact extra permissions a command needs.
/// Computed deterministically from command analysis (see
/// [`missing_grants`]); applied only after an explicit user ask.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants {
    /// Directories the command must be able to write beneath.
    pub write_paths: Vec<PathBuf>,
    /// Network egress for this command.
    pub network: bool,
    /// Env vars to pass through to the child.
    pub env: Vec<String>,
}

impl Grants {
    /// Whether nothing is being asked for.
    pub fn is_empty(&self) -> bool {
        self.write_paths.is_empty() && !self.network && self.env.is_empty()
    }

    /// One-line human summary for ask surfaces.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.write_paths.is_empty() {
            let paths: Vec<String> = self
                .write_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            parts.push(format!("write {}", paths.join(", ")));
        }
        if self.network {
            parts.push("network".to_string());
        }
        if !self.env.is_empty() {
            parts.push(format!("env {}", self.env.join(",")));
        }
        parts.join(" · ")
    }

    /// Whether `self` covers everything `other` asks for. A stored write
    /// path covers a requested one only when it is equal to or a parent
    /// of it (`Path::starts_with` compares whole components) — a
    /// narrower stored directory never covers a broader request.
    pub fn covers(&self, other: &Grants) -> bool {
        other
            .write_paths
            .iter()
            .all(|p| self.write_paths.iter().any(|s| p.starts_with(s)))
            && (self.network || !other.network)
            && other.env.iter().all(|e| self.env.contains(e))
    }
}

/// What the active enforcement backend actually denies under `fs`.
/// Grant computation only offers what the backend would truly block —
/// no phantom asks on backends that allow the thing anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// Network egress is denied. bwrap's `--unshare-all` denies it
    /// outright; under landlock this is best-effort — restricting
    /// network needs Landlock ABI ≥ 4 and never covers UDP.
    pub network_denied: bool,
    /// The child environment is cleared (bwrap `--clearenv`, trampoline).
    pub env_cleared: bool,
}

/// Capabilities of the detected enforcement tool.
pub fn caps() -> Caps {
    match detect_tool() {
        Some(Tool::Firejail) => Caps {
            network_denied: false,
            env_cleared: false,
        },
        // None only matters under fs, where wrap_command fails closed;
        // report the stricter caps so grant computation stays honest.
        // Landlock's net denial is best-effort (ABI ≥ 4, no UDP), so
        // this flag may pose a grant ask the backend cannot fully
        // enforce — it never silently under-asks.
        Some(Tool::Bubblewrap) | Some(Tool::Landlock) | None => Caps {
            network_denied: true,
            env_cleared: true,
        },
    }
}

/// Directories never offered as write grants: granting a write beneath
/// one of these would effectively unsandbox the filesystem, so commands
/// needing them stay denied (fail-closed).
fn broad_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = [
        "/", "/bin", "/etc", "/home", "/lib", "/lib64", "/opt", "/root", "/sbin", "/srv", "/usr",
        "/var", "/Users",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home));
    }
    dirs
}

/// The only `/dev` entries the landlock backend grants writes to —
/// an enumerated surface matching what builds need, instead of the
/// whole host `/dev` (bwrap's `--dev` is a fresh tmpfs, so wholesale
/// `/dev` there was never the parity).
const DEV_NODES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/shm",
];

/// Resolve a writable-allowlist entry to its real on-disk target:
/// canonicalize the nearest existing ancestor, then re-append the
/// not-yet-created remainder (grant dirs may not exist yet). Both
/// enforcement backends bind/attach the resolved inode, so a symlink
/// spelling must never widen what the rule actually covers: if the
/// resolved path IS a broad root (`/`, `$HOME`, `/etc`, …) or sits
/// above one, the entry is refused — fail closed.
pub(crate) fn resolve_writable(p: &Path) -> Result<PathBuf, String> {
    let mut prefix = p.to_path_buf();
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let canonical = loop {
        match std::fs::canonicalize(&prefix) {
            Ok(c) => break Some(c),
            Err(_) => match (prefix.parent(), prefix.file_name()) {
                (Some(parent), Some(name)) => {
                    suffix.push(name.to_os_string());
                    prefix = parent.to_path_buf();
                }
                _ => break None,
            },
        }
    };
    let Some(mut resolved) = canonical else {
        return Err(format!(
            "sandbox: cannot resolve writable path {} — refusing",
            p.display()
        ));
    };
    for seg in suffix.into_iter().rev() {
        resolved.push(seg);
    }
    // `b.starts_with(&resolved)` covers both equality (resolved IS the
    // broad root) and resolved being a proper ancestor of it (a grant
    // at or above `$HOME`, `/`, … unsandboxes everything beneath).
    // Broad roots may themselves be symlink spellings ($HOME under an
    // automount), so canonicalized forms join the comparison — else a
    // grant resolving to the real home slips past the lexical entries.
    let broad = broad_dirs();
    let broad_resolved: Vec<PathBuf> = broad
        .iter()
        .filter_map(|b| std::fs::canonicalize(b).ok())
        .collect();
    if let Some(b) = broad
        .iter()
        .chain(broad_resolved.iter())
        .find(|b| b.starts_with(&resolved))
    {
        return Err(format!(
            "sandbox: writable path {} resolves to {}, which is or contains the broad \
             root {} — refusing",
            p.display(),
            resolved.display(),
            b.display()
        ));
    }
    Ok(resolved)
}

/// Lexically normalize a path (no filesystem access): resolve `.`/`..`,
/// make relative paths absolute against `cwd`, expand a leading `~` or
/// `~/` to `$HOME` (a `~user` prefix is left alone). Sandbox grant
/// matching is prefix-based and needs a canonical spelling even for
/// not-yet-existing targets.
fn normalize(p: &Path, cwd: &Path) -> PathBuf {
    let text = p.to_string_lossy();
    let expanded: PathBuf = if text == "~" {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| p.to_path_buf())
    } else if let Some(rest) = text.strip_prefix("~/") {
        std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|_| p.to_path_buf())
    } else {
        // `~user/…` is another user's home: mapping it under $HOME
        // would grant the wrong tree, so it stays unexpanded (and
        // therefore ungrantable — fail-closed)
        p.to_path_buf()
    };
    let abs = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(&expanded)
    };
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // never pop past the root: `/w/../../etc` must stay
                // absolute (`/etc`), not collapse to a relative path
                // that would bind the wrong subtree
                if out.parent().is_some() {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Compute what `command inputs` need beyond `policy`. Inputs come from
/// bash command analysis (redirection targets, env assignments,
/// network-tool detection); the result is deterministic for a given
/// command line AND parent environment — the env pass-through check
/// reads the live environment and broad-dir rejection reads `$HOME`.
/// Non-grantable needs (writes beneath broad dirs like `/` or `$HOME`
/// itself, env vars unset in the parent) are silently left out: the
/// command will fail rather than over-ask.
pub fn missing_grants(
    policy: &Policy,
    cwd: &Path,
    redirect_targets: &[String],
    env_vars: &[String],
    wants_net: bool,
) -> Grants {
    let Policy::Fs {
        allow_write,
        allow_net,
        pass_env,
    } = policy
    else {
        return Grants::default();
    };
    let caps = caps();
    let mut grants = Grants::default();
    let broad = broad_dirs();
    for target in redirect_targets {
        let lexical = normalize(Path::new(target), cwd);
        // enforcement binds/attaches the RESOLVED inode, so grant asks
        // must point at the resolved target, not the lexical spelling;
        // an unresolvable target stays denied (fail-closed)
        let resolved = resolve_writable(&lexical).ok();
        // Path::starts_with compares whole components, so `/tmpfoo`
        // is NOT inside `/tmp` — no string-prefix escape here
        let covered = |p: &Path| allow_write.iter().any(|a| p.starts_with(a));
        if resolved.as_ref().is_some_and(|r| covered(r)) || covered(&lexical) {
            continue;
        }
        // the target is a file the command writes; the sandbox grants
        // directories, so the grant is its parent (resolved spelling)
        let base = resolved.as_deref().unwrap_or(&lexical);
        let Some(parent) = base.parent() else {
            continue;
        };
        let lexical_parent = lexical.parent();
        // canonicalized broad spellings too: a redirect into the real
        // home behind a symlinked $HOME must stay denied
        let broad_resolved: Vec<PathBuf> = broad
            .iter()
            .filter_map(|b| std::fs::canonicalize(b).ok())
            .collect();
        let is_broad = |p: &Path| {
            broad.iter().any(|b| p == b.as_path())
                || broad_resolved.iter().any(|b| p == b.as_path())
        };
        if parent.as_os_str().is_empty() || is_broad(parent) || lexical_parent.is_some_and(is_broad)
        {
            continue; // not grantable without unsandboxing — stays denied
        }
        // hidden dirs directly under $HOME (.ssh, .gnupg, .config …)
        // are persistence/config territory: never offered as a write
        // grant, same refusal class as $HOME itself — checked on both
        // spellings so a symlinked $HOME cannot sneak one through
        if let Ok(home) = std::env::var("HOME") {
            let home_resolved = std::fs::canonicalize(&home).ok();
            let hidden = |p: &Path| {
                p.parent().is_some_and(|gp| {
                    gp == Path::new(&home) || home_resolved.as_deref() == Some(gp)
                }) && p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
            };
            if hidden(parent) || lexical_parent.is_some_and(hidden) {
                continue;
            }
        }
        if !grants
            .write_paths
            .iter()
            .any(|p: &PathBuf| p == parent || parent.starts_with(p))
        {
            grants.write_paths.push(parent.to_path_buf());
        }
    }
    if caps.env_cleared {
        for name in env_vars {
            if pass_env.contains(name) || grants.env.contains(name) {
                continue;
            }
            // only vars that actually carry a value in the parent env —
            // asking to pass through an unset var is noise
            if std::env::var(name).map(|v| !v.is_empty()).unwrap_or(false) {
                grants.env.push(name.clone());
            }
        }
    }
    grants.network = wants_net && caps.network_denied && !*allow_net;
    grants
}

/// Apply grants onto a policy (the per-call or live expansion).
pub fn apply_grants(policy: &Policy, grants: &Grants) -> Policy {
    match policy {
        Policy::Off => Policy::Off,
        Policy::Fs {
            allow_write,
            allow_net,
            pass_env,
        } => {
            let mut allow = allow_write.clone();
            for p in &grants.write_paths {
                if !allow.contains(p) {
                    allow.push(p.clone());
                }
            }
            let mut env = pass_env.clone();
            for e in &grants.env {
                if !env.contains(e) {
                    env.push(e.clone());
                }
            }
            Policy::Fs {
                allow_write: allow,
                allow_net: *allow_net || grants.network,
                pass_env: env,
            }
        }
    }
}

/// Sandbox configuration table (`[sandbox]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SandboxConfig {
    /// `"off"` (default) or `"fs"`.
    pub mode: Option<String>,
    /// Extra writable directories on top of the default allowlist
    /// (cwd, /tmp, XDG state/cache). `[sandbox] allow_write` in the
    /// project layer is where an "always" expansion grant persists.
    pub allow_write: Vec<String>,
}

/// The writable-directory allowlist for `cwd`.
pub fn allowlist_for(cwd: &Path) -> Vec<PathBuf> {
    let mut allow = vec![cwd.to_path_buf(), PathBuf::from("/tmp")];
    let state = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/state")));
    let cache = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".cache")));
    if let Ok(p) = state {
        allow.push(p);
    }
    if let Ok(p) = cache {
        allow.push(p);
    }
    allow
}

/// Resolve config → policy.
pub fn policy_from_config(cfg: &SandboxConfig, cwd: &Path) -> Result<Policy, String> {
    let mode = cfg.mode.as_deref().unwrap_or("off");
    match mode {
        "off" => Ok(Policy::Off),
        "fs" => {
            let mut allow = allowlist_for(cwd);
            for extra in &cfg.allow_write {
                let path = normalize(Path::new(extra), cwd);
                if !allow.contains(&path) {
                    allow.push(path);
                }
            }
            Ok(Policy::fs(allow))
        }
        other => Err(format!(
            "[sandbox] mode: unknown value {other:?} (expected \"off\" or \"fs\")"
        )),
    }
}

/// Which sandbox engine is available, if any. Detection order:
/// bubblewrap, then firejail, then in-kernel landlock (no external
/// binary needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Bubblewrap,
    Firejail,
    /// In-kernel LSM enforcement via the self re-exec trampoline.
    Landlock,
}

pub fn detect_tool() -> Option<Tool> {
    *DETECTED
}

/// The probes fork bwrap/firejail and create a landlock ruleset fd —
/// `wrap_command` runs per bash call, so resolve once per process.
static DETECTED: std::sync::LazyLock<Option<Tool>> = std::sync::LazyLock::new(|| {
    for (bin, tool) in [("bwrap", Tool::Bubblewrap), ("firejail", Tool::Firejail)] {
        if std::process::Command::new(bin)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return Some(tool);
        }
    }
    landlock_supported().then_some(Tool::Landlock)
});
/// Whether the kernel can create landlock rulesets. Probe only:
/// `Ruleset::create()` issues `landlock_create_ruleset(2)` and returns
/// an (immediately dropped) fd — nothing is enforced on the caller;
/// enforcement happens later inside the trampoline process. Preferred
/// over reading `/sys/kernel/security/lsm`, which is often hidden
/// (e.g. WSL2) even on landlock-capable kernels.
fn landlock_supported() -> bool {
    #[cfg(target_os = "linux")]
    {
        use landlock::{ABI, AccessFs, Ruleset, RulesetAttr};
        Ruleset::default()
            .handle_access(AccessFs::from_read(ABI::V1))
            .and_then(|r| r.create().map(|_| ()))
            .is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// The trampoline policy JSON: what `ka-sandbox-exec` enforces. Shipped
/// shape (an object with `writable`/`net`/`env`); only ever exchanged
/// between `wrap_command` and the hidden subcommand of the same binary.
#[derive(Debug, Serialize, Deserialize)]
struct TrampolinePolicy {
    writable: Vec<PathBuf>,
    net: bool,
    env: Vec<String>,
}

/// Build the wrapped argv for `command` under the given policy.
/// `Err` = fail closed (no enforcement tool available).
pub fn wrap_command(policy: &Policy, command: &str, cwd: &Path) -> Result<Vec<String>, String> {
    match policy {
        Policy::Off => Ok(vec!["sh".into(), "-c".into(), command.into()]),
        Policy::Fs {
            allow_write,
            allow_net,
            pass_env,
        } => {
            // both backends bind/attach the RESOLVED inode: resolve
            // every writable entry up front and refuse (fail closed)
            // any that lands on or above a broad root via symlink
            let resolved: Vec<PathBuf> = allow_write
                .iter()
                .map(|d| resolve_writable(d.as_path()))
                .collect::<Result<_, _>>()?;
            match detect_tool() {
                Some(Tool::Bubblewrap) => {
                    let mut argv = vec![
                        "bwrap".into(),
                        "--unshare-all".into(),
                        "--die-with-parent".into(),
                        "--new-session".into(),
                        "--ro-bind".into(),
                        "/".into(),
                        "/".into(),
                        "--dev".into(),
                        "/dev".into(),
                        "--proc".into(),
                        "/proc".into(),
                    ];
                    if *allow_net {
                        // ordered after --unshare-all: re-share the network
                        // stack for this command (expansion grant)
                        argv.push("--share-net".into());
                    }
                    for dir in &resolved {
                        argv.push("--bind".into());
                        argv.push(dir.to_string_lossy().into_owned());
                        argv.push(dir.to_string_lossy().into_owned());
                    }
                    argv.push("--clearenv".into());
                    // --setenv values ride bwrap's own argv, so they are
                    // visible in /proc/<pid>/cmdline to anyone who can read
                    // it — never expand the pass-through with secret-bearing
                    // env names
                    for name in pass_env {
                        if let Ok(value) = std::env::var(name) {
                            argv.push("--setenv".into());
                            argv.push(name.clone());
                            argv.push(value);
                        }
                    }
                    argv.push("sh".into());
                    argv.push("-c".into());
                    argv.push(command.into());
                    Ok(argv)
                }
                Some(Tool::Firejail) => {
                    // firejail keeps the environment and (without --net=none)
                    // the network: only the write allowlist applies
                    let mut argv = vec![
                        "firejail".into(),
                        "--quiet".into(),
                        "--private=.".into(),
                        "--read-only=/".into(),
                    ];
                    for dir in &resolved {
                        argv.push(format!("--whitelist={}", dir.display()));
                    }
                    argv.push("--".into());
                    argv.push("sh".into());
                    argv.push("-c".into());
                    argv.push(command.into());
                    let _ = cwd;
                    Ok(argv)
                }
                Some(Tool::Landlock) => {
                    // self re-exec trampoline: the sandboxed command becomes
                    // `<ka> ka-sandbox-exec <policy-json> -- <argv…>`;
                    // the hidden subcommand applies the landlock ruleset to
                    // itself, then execs the real argv (the workspace forbids
                    // `unsafe`, so no Command::pre_exec). The policy JSON is
                    // the resolved allow_write list plus an enumerated /dev
                    // surface (bwrap's --dev is a fresh tmpfs, so granting
                    // the host /dev wholesale was never the parity) plus
                    // the net/env expansion grants.
                    let mut writable = resolved;
                    writable.extend(
                        DEV_NODES
                            .iter()
                            .filter(|p| Path::new(p).exists())
                            .map(|p| PathBuf::from(*p)),
                    );
                    let policy = serde_json::to_string(&TrampolinePolicy {
                        writable,
                        net: *allow_net,
                        env: pass_env.clone(),
                    })
                    .map_err(|e| format!("sandbox policy: {e}"))?;
                    // readlink(/proc/self/exe) once per process, not per call
                    static EXE: std::sync::LazyLock<Result<PathBuf, String>> =
                        std::sync::LazyLock::new(|| {
                            std::env::current_exe().map_err(|e| format!("sandbox trampoline: {e}"))
                        });
                    let exe = EXE.as_ref().map_err(Clone::clone)?;
                    Ok(vec![
                        exe.to_string_lossy().into_owned(),
                        TRAMPOLINE_ARG.into(),
                        policy,
                        "--".into(),
                        "sh".into(),
                        "-c".into(),
                        command.into(),
                    ])
                }
                None => Err(
                    "sandbox: mode \"fs\" requires bubblewrap (bwrap), firejail, or kernel \
                     landlock on this host — refusing to run unsandboxed"
                        .into(),
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn mode_parses_and_fails_closed_on_unknown() {
        let off: SandboxConfig = serde_json::from_str(r#"{"mode":"off"}"#).unwrap();
        assert_eq!(
            policy_from_config(&off, Path::new("/tmp")).unwrap(),
            Policy::Off
        );
        let bad: SandboxConfig = serde_json::from_str(r#"{"mode":"yolo"}"#).unwrap();
        assert!(policy_from_config(&bad, Path::new("/tmp")).is_err());
    }

    #[test]
    fn allowlist_includes_cwd_tmp_and_xdg() {
        let cwd = Path::new("/work/proj");
        let allow = allowlist_for(cwd);
        assert!(allow.contains(&PathBuf::from("/work/proj")));
        assert!(allow.contains(&PathBuf::from("/tmp")));
    }

    #[test]
    fn off_policy_leaves_command_plain() {
        let p = policy_from_config(
            &serde_json::from_str(r#"{"mode":"off"}"#).unwrap(),
            Path::new("/tmp"),
        )
        .unwrap();
        let argv = wrap_command(&p, "echo hi", Path::new("/tmp")).unwrap();
        assert_eq!(argv, vec!["sh", "-c", "echo hi"]);
    }

    #[test]
    fn fs_policy_wraps_with_allowlist_or_fails_closed() {
        let p = Policy::fs(vec![PathBuf::from("/tmp")]);
        match wrap_command(&p, "make", Path::new("/tmp")) {
            Ok(argv) => {
                // whichever engine this host offers, it must wrap
                if argv[0] == "bwrap" || argv[0] == "firejail" {
                    assert!(argv.iter().any(|a| a == "make"), "{argv:?}");
                } else {
                    // landlock trampoline: <exe> ka-sandbox-exec <json> -- sh -c make
                    assert_eq!(argv[1], "ka-sandbox-exec", "{argv:?}");
                    let policy: TrampolinePolicy = serde_json::from_str(&argv[2]).unwrap();
                    assert!(policy.writable.contains(&PathBuf::from("/tmp")), "{argv:?}");
                    // enumerated /dev surface only — never the host /dev
                    // wholesale (no /dev/shm OOM, no device nodes)
                    assert!(
                        !policy.writable.contains(&PathBuf::from("/dev")),
                        "{argv:?}"
                    );
                    assert!(
                        !policy.writable.iter().any(|w| w.starts_with("/dev/dri")),
                        "{argv:?}"
                    );
                    for node in DEV_NODES.iter().filter(|p| Path::new(p).exists()) {
                        assert!(
                            policy.writable.contains(&PathBuf::from(*node)),
                            "{node} missing: {argv:?}"
                        );
                    }
                    // nothing else under /dev snuck in
                    assert!(
                        policy
                            .writable
                            .iter()
                            .filter(|w| w.starts_with("/dev"))
                            .all(|w| DEV_NODES.contains(&w.to_string_lossy().as_ref())),
                        "{argv:?}"
                    );
                    assert!(!policy.net, "base policy never grants network");
                    assert_eq!(argv[3], "--", "{argv:?}");
                    assert_eq!(argv[6], "make", "{argv:?}");
                }
            }
            Err(e) => assert!(e.contains("refusing"), "{e}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn allow_write_symlink_to_root_is_refused() {
        let dir = std::env::temp_dir().join(format!("ka-sb-link-root-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/", &link).unwrap();
        let p = Policy::fs(vec![link.clone()]);
        // whichever backend this host offers, the symlinked entry must
        // fail closed — naming the link — before any bind/rule is built
        let err = wrap_command(&p, "make", &dir).unwrap_err();
        assert!(err.contains("link"), "{err}");
        assert!(err.contains("refusing"), "{err}");
        std::fs::remove_file(&link).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    #[cfg(unix)]
    fn allow_write_symlink_within_granted_tree_still_resolves() {
        let dir = std::env::temp_dir().join(format!("ka-sb-link-proj-{}", std::process::id()));
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let alias = dir.join("alias");
        let _ = std::fs::remove_file(&alias);
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let resolved = std::fs::canonicalize(&real).unwrap();
        if crate::detect_tool().is_none() {
            return; // nothing to wrap with on this host
        }
        let p = Policy::fs(vec![alias.clone()]);
        let argv = wrap_command(&p, "make", &dir)
            .unwrap_or_else(|e| panic!("in-tree symlink must stay grantable: {e}"));
        if argv[0] == "bwrap" {
            let s = resolved.to_string_lossy();
            assert!(
                argv.windows(3)
                    .any(|w| w[0] == "--bind" && w[1] == s && w[2] == s),
                "{argv:?}"
            );
        } else if argv[0] == "firejail" {
            assert!(
                argv.iter()
                    .any(|a| a == &format!("--whitelist={}", resolved.display())),
                "{argv:?}"
            );
        } else {
            // landlock trampoline
            let pol: TrampolinePolicy = serde_json::from_str(&argv[2]).unwrap();
            assert!(pol.writable.contains(&resolved), "{pol:?}");
        }
        std::fs::remove_file(&alias).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn grants_compute_from_redirects_env_and_network() {
        let cwd = Path::new("/work/proj");
        let policy = Policy::fs(vec![PathBuf::from("/work/proj"), PathBuf::from("/tmp")]);
        // redirection inside the allowlist: no grant
        let g = missing_grants(&policy, cwd, &["out.txt".to_string()], &[], false);
        assert!(g.is_empty(), "{g:?}");
        // redirection outside: the parent dir is the grant
        let g = missing_grants(
            &policy,
            cwd,
            &["/opt/data/report.md".to_string()],
            &[],
            false,
        );
        assert_eq!(g.write_paths, vec![PathBuf::from("/opt/data")], "{g:?}");
        // a broad parent ($HOME, /, /etc...) is never offered — the
        // command stays denied rather than effectively unsandboxed
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/nobody".to_string());
        let g = missing_grants(&policy, cwd, &[format!("{home}/.bashrc")], &[], false);
        assert!(g.write_paths.is_empty(), "{g:?}");
        // env: offered only when the backend clears env and the var holds
        // a value in the parent (HOME is set wherever the suite runs)
        let g = missing_grants(&policy, cwd, &[], &["HOME".to_string()], false);
        assert_eq!(g.env, vec!["HOME".to_string()], "{g:?}");
        let g = missing_grants(&policy, cwd, &[], &["KA_SB_UNSET_VAR".to_string()], false);
        assert!(g.env.is_empty(), "{g:?}");
        // network: offered only when the backend denies it
        let g = missing_grants(&policy, cwd, &[], &[], true);
        let expected = caps().network_denied;
        assert_eq!(g.network, expected, "{g:?}");
    }

    #[test]
    fn grants_summary_and_covers() {
        let g = Grants {
            write_paths: vec![PathBuf::from("/opt/data")],
            network: true,
            env: vec!["FOO".to_string()],
        };
        assert_eq!(g.summary(), "write /opt/data · network · env FOO");
        let smaller = Grants {
            write_paths: vec![PathBuf::from("/opt/data/x.txt")],
            network: false,
            env: Vec::new(),
        };
        assert!(g.covers(&smaller));
        assert!(!smaller.covers(&g));
    }

    #[test]
    fn apply_grants_merges_into_policy() {
        let base = Policy::fs(vec![PathBuf::from("/tmp")]);
        let grants = Grants {
            write_paths: vec![PathBuf::from("/opt/data")],
            network: true,
            env: vec!["FOO".to_string()],
        };
        let expanded = apply_grants(&base, &grants);
        match &expanded {
            Policy::Fs {
                allow_write,
                allow_net,
                pass_env,
            } => {
                assert!(allow_write.contains(&PathBuf::from("/opt/data")));
                assert!(allow_write.contains(&PathBuf::from("/tmp")));
                assert!(*allow_net);
                assert_eq!(pass_env, &vec!["FOO".to_string()]);
            }
            other => panic!("expected Fs, got {other:?}"),
        }
        // already-granted needs compute back empty
        let again = missing_grants(
            &expanded,
            Path::new("/tmp"),
            &["/opt/data/x".to_string()],
            &[],
            true,
        );
        assert!(again.is_empty(), "{again:?}");
    }

    #[test]
    fn extra_allow_write_config_extends_the_allowlist() {
        let cfg: SandboxConfig =
            serde_json::from_str(r#"{"mode":"fs","allow_write":["/opt/cache"]}"#).unwrap();
        let policy = policy_from_config(&cfg, Path::new("/work/proj")).unwrap();
        match policy {
            Policy::Fs { allow_write, .. } => {
                assert!(allow_write.contains(&PathBuf::from("/work/proj")));
                assert!(allow_write.contains(&PathBuf::from("/opt/cache")));
            }
            Policy::Off => panic!("fs mode must resolve to Fs"),
        }
    }

    #[test]
    fn home_dotdirs_and_tilde_user_never_earn_write_grants() {
        // policy: everything writable, nothing allowed — every write
        // target becomes a candidate grant
        let policy = Policy::Fs {
            allow_write: vec![],
            allow_net: false,
            pass_env: vec![],
        };
        let cwd = Path::new("/w");
        let home = std::env::var("HOME").unwrap_or_default();
        // ~user must NOT expand to $HOME (round-1 pin): the target
        // stays cwd-relative
        let g = missing_grants(&policy, cwd, &["~root/f".to_string()], &[], false);
        if !home.is_empty() {
            assert!(
                !g.write_paths.iter().any(|p| p.starts_with(&home)),
                "~user must stay unexpanded: {:?}",
                g.write_paths
            );
        }
        let h = home.clone();
        if !h.is_empty() {
            let g = missing_grants(
                &policy,
                cwd,
                &[format!("{h}/.ssh/authorized_keys")],
                &[],
                false,
            );
            assert!(
                !g.write_paths
                    .iter()
                    .any(|p| p.starts_with(format!("{h}/.ssh"))),
                "hidden dirs under $HOME are never offered: {:?}",
                g.write_paths
            );
        }
    }

    #[test]
    fn covers_requires_stored_dir_equal_or_parent() {
        let data = Grants {
            write_paths: vec![PathBuf::from("/opt/data")],
            ..Default::default()
        };
        let opt = Grants {
            write_paths: vec![PathBuf::from("/opt")],
            ..Default::default()
        };
        // stored equal-or-parent covers the request…
        assert!(opt.covers(&data));
        assert!(data.covers(&Grants {
            write_paths: vec![PathBuf::from("/opt/data/sub/x")],
            ..Default::default()
        }));
        // …never the other way around: a narrower stored dir must not
        // cover a broader request
        assert!(!data.covers(&opt));
        // and never across a component boundary: /tmpfoo is not /tmp
        let tmp = Grants {
            write_paths: vec![PathBuf::from("/tmp")],
            ..Default::default()
        };
        assert!(!tmp.covers(&Grants {
            write_paths: vec![PathBuf::from("/tmpfoo")],
            ..Default::default()
        }));
    }

    #[test]
    fn allowlist_prefixes_match_whole_components() {
        let cwd = Path::new("/work/proj");
        let policy = Policy::fs(vec![PathBuf::from("/work/proj"), PathBuf::from("/tmp")]);
        // /tmpfoo is NOT inside /tmp: the write earns a grant ask
        let g = missing_grants(&policy, cwd, &["/tmpfoo/x".to_string()], &[], false);
        assert_eq!(g.write_paths, vec![PathBuf::from("/tmpfoo")], "{g:?}");
        // /tmp/sub IS inside /tmp: no grant
        let g = missing_grants(&policy, cwd, &["/tmp/sub/x".to_string()], &[], false);
        assert!(g.write_paths.is_empty(), "{g:?}");
        // normalize never pops past the root: results stay absolute so
        // path_beneath_rules bind the subtree the OS would resolve
        assert_eq!(
            normalize(Path::new("/w/../../etc"), cwd),
            PathBuf::from("/etc")
        );
        assert_eq!(normalize(Path::new("/w/.."), cwd), PathBuf::from("/"));
    }
}
