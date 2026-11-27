//! ka engine crate: the turn machine and layered configuration. No I/O
//! beyond the queues; surfaces live elsewhere, wires live in ka-dialect.

pub mod agents;
mod canned;
pub mod checkpoint;
pub mod child_policy;
pub mod config;
pub mod conventions;
pub mod dap;
mod diff;
mod engine;
pub mod features;
mod fshooks;
pub mod hands;
pub mod lsp;
pub mod mcp;
pub mod mcp_cache;
pub mod mcp_oauth;
pub mod readers;
pub mod trust;
pub mod voice;
pub mod wire;

pub use config::{Config, ConfigError};
pub use engine::{
    EngineHandle, StrandChoice, effective_debug_cfg, effective_lsp_cfg, lsp_hands, read_waypoint,
    spawn, spawn_full, spawn_with, spawn_with_speaker,
};
// Contract-test-only spawn (team_contracts.rs is the sole consumer):
// hidden from production builds like `trust::test_support`. The
// `team_contracts` suite declares `required-features = ["test-util"]`.
#[cfg(any(test, feature = "test-util"))]
pub use engine::spawn_with_team_speakers;

use std::path::{Path, PathBuf};

/// The project root for ka's project-scope `.ka/` layer and everything
/// ka generates into it: the nearest ancestor of `cwd` (cwd included)
/// carrying a `.git` marker — a directory in a normal clone, a file in a
/// worktree or submodule. The walk mirrors AGENTS.md discovery: it stops
/// at the home directory and at `/`, and without any marker `cwd` itself
/// is the root, so non-git directories keep launch-dir behavior.
pub fn project_root(cwd: &Path) -> PathBuf {
    let home = std::env::var("HOME").map(PathBuf::from).ok();
    project_root_in(cwd, |p| p.join(".git").exists(), home.as_deref())
}

/// Canonical-form directory equality with a literal fallback (paths that
/// no longer exist compare literally). `$HOME` may be spelled through a
/// symlink while the walk carries the physical path (or vice versa); a
/// raw comparison would let the walk slip past the bound and adopt a
/// stray `.git` above home, re-anchoring generated writes there.
fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// [`project_root`] with injectable marker probe and home (tests).
fn project_root_in(cwd: &Path, is_repo: impl Fn(&Path) -> bool, home: Option<&Path>) -> PathBuf {
    let mut cur = cwd.to_path_buf();
    loop {
        if is_repo(&cur) {
            return cur;
        }
        // bounds: the home directory (a stray ~/.git must not claim
        // every session under $HOME), the filesystem root, and the
        // relative-path degenerate `""`. Unmarked walks fall back to
        // the launch dir itself.
        if cur == Path::new("/") || cur == Path::new("") || home.is_some_and(|h| same_dir(h, &cur))
        {
            return cwd.to_path_buf();
        }
        match cur.parent() {
            Some(parent) => cur = parent.to_path_buf(),
            None => return cwd.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod project_root_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Fake tree: `repos` are the directories that "carry .git".
    fn root_of(cwd: &str, repos: &[&str], home: Option<&str>) -> PathBuf {
        project_root_in(
            Path::new(cwd),
            |p| repos.iter().any(|r| Path::new(r) == p),
            home.map(Path::new),
        )
    }

    #[test]
    fn nearest_git_marker_wins() {
        let repos = ["/home/u/proj", "/home/u"];
        assert_eq!(
            root_of("/home/u/proj/crates/ka/src", &repos, Some("/home/u")),
            Path::new("/home/u/proj")
        );
    }

    #[test]
    fn cwd_itself_can_be_the_root() {
        let repos = ["/home/u/proj"];
        assert_eq!(
            root_of("/home/u/proj", &repos, Some("/home/u")),
            Path::new("/home/u/proj")
        );
    }

    #[test]
    fn unmarked_walk_falls_back_to_cwd() {
        assert_eq!(
            root_of("/home/u/notes/deep", &[], Some("/home/u")),
            Path::new("/home/u/notes/deep")
        );
    }

    #[test]
    fn home_bounds_the_walk() {
        // a repo marker "above" home is ignored: home is a bound
        assert_eq!(
            root_of("/home/u/notes", &["/repo"], Some("/home/u")),
            Path::new("/home/u/notes")
        );
        // home itself may be the root when marked
        assert_eq!(
            root_of("/home/u/notes", &["/home/u"], Some("/home/u")),
            Path::new("/home/u")
        );
    }

    /// The documented worktree/submodule shape: `.git` as a FILE. Uses
    /// the real probe against a temp tree.
    #[test]
    fn git_file_marker_is_a_root() {
        let dir = std::env::temp_dir().join(format!("ka-gitfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("deep")).unwrap();
        std::fs::write(dir.join(".git"), "gitdir: /elsewhere/.git\n").unwrap();
        assert_eq!(
            project_root_in(&dir.join("deep"), |p| p.join(".git").exists(), None),
            dir
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
