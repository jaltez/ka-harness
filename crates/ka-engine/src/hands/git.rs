//! Read-only git awareness: branch + dirty list via the `git` binary. ka
//! never mutates the repository through this module.

use std::process::Command;

/// Snapshot of repository state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepoSnapshot {
    /// Current branch name (`HEAD` when detached, empty when not a repo).
    pub branch: String,
    /// Dirty (modified/untracked) paths, capped at 50.
    pub dirty: Vec<String>,
}

impl RepoSnapshot {
    /// Capture the snapshot for `cwd`. Non-repo → default (empty).
    pub fn capture(cwd: &std::path::Path) -> Self {
        let branch = Self::branch(cwd);
        let dirty = git_output(cwd, &["status", "--porcelain"])
            .map(|s| {
                s.lines()
                    .filter_map(|l| {
                        let path = l.get(3..)?.trim().to_string();
                        (!path.is_empty()).then_some(path)
                    })
                    .take(50)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Self { branch, dirty }
    }

    /// The current branch name (`HEAD` when detached, empty when not a
    /// repo). No status walk, for callers that need the branch alone.
    pub fn branch(cwd: &std::path::Path) -> String {
        git_output(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_default()
    }

    /// One-line summary for the system prompt.
    pub fn summary(&self) -> String {
        if self.branch.is_empty() {
            return "(not a git repository)".to_string();
        }
        format!(
            "git branch {}, {} dirty file(s){}",
            self.branch,
            self.dirty.len(),
            self.dirty
                .first()
                .map(|f| format!(" (e.g. {f})"))
                .unwrap_or_default()
        )
    }
}

fn git_output(cwd: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::path::PathBuf;

    fn temp_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-git-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            ["init", "-q", "-b", "main"].as_slice(),
            ["config", "user.email", "t@t"].as_slice(),
            ["config", "user.name", "t"].as_slice(),
            ["commit", "--allow-empty", "-q", "-m", "x"].as_slice(),
        ] {
            let st = Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap()
                .status;
            assert!(st.success(), "git {args:?} failed");
        }
        dir
    }

    #[test]
    fn captures_branch_and_dirty() {
        let dir = temp_repo("basic");
        let snap = RepoSnapshot::capture(&dir);
        assert_eq!(snap.branch, "main");
        assert!(snap.dirty.is_empty(), "{:?}", snap.dirty);

        std::fs::write(dir.join("new.txt"), "hi").unwrap();
        let snap = RepoSnapshot::capture(&dir);
        assert_eq!(snap.dirty, vec!["new.txt".to_string()]);
        assert!(snap.summary().contains("branch main"));
        assert_eq!(RepoSnapshot::branch(&dir), "main", "the branch alone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_repo_is_empty() {
        let dir = std::env::temp_dir().join(format!("ka-git-{}-none", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let snap = RepoSnapshot::capture(&dir);
        assert_eq!(snap.branch, "");
        assert!(snap.dirty.is_empty());
        assert!(snap.summary().contains("not a git repository"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
