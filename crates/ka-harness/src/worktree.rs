//! `ka worktree add|list|remove` (GIT-05): a small wrapper over
//! `git worktree` that keeps every worktree under `<repo>/.worktrees/`.
//!
//! - `add <name> [branch]` creates `.worktrees/<name>`; without a
//!   branch argument it creates `ka/<name>` for it, with one it checks
//!   the named branch out.
//! - `list` prints `git worktree list --porcelain` verbatim.
//! - `remove <name>` removes `.worktrees/<name>` again; `--delete-branch`
//!   also deletes the branch the worktree had checked out (a plain `-d`,
//!   so git still refuses to drop an unmerged branch — ka never forces).
//!
//! The cwd must be inside a git repository; anything else is a clear
//! error, not a confusing git stderr dump.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Where ka-managed worktrees live, relative to the repo root.
pub(crate) const WORKTREES_DIR: &str = ".worktrees";

/// `ka worktree <cmd>`
#[derive(clap::Subcommand, Clone, Debug)]
pub enum WorktreeCmd {
    /// Create .worktrees/<name> on a new branch ka/<name> (or check out
    /// the given existing branch)
    Add {
        /// Worktree name (also the default branch name: ka/<name>)
        name: String,
        /// Check out this existing branch instead of creating ka/<name>
        #[arg(value_name = "BRANCH")]
        branch: Option<String>,
    },
    /// Print git worktree list --porcelain
    List,
    /// Remove .worktrees/<name> (and its branch with --delete-branch)
    Remove {
        /// Worktree name (see `ka worktree list`)
        name: String,
        /// Also delete the branch the worktree had checked out
        #[arg(long)]
        delete_branch: bool,
    },
}

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
}

/// Run `ka worktree <cmd>`; the cwd anchors the repository.
pub fn run(cmd: WorktreeCmd) -> Result<ExitCode, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let root = require_git_repo(&cwd)?;
    match cmd {
        WorktreeCmd::Add { name, branch } => {
            validate_name(&name)?;
            let wt = root.join(WORKTREES_DIR).join(&name);
            if wt.exists() {
                return Err(format!(
                    "{} already exists — remove it first (git worktree remove {})",
                    wt.display(),
                    wt.display()
                ));
            }
            match &branch {
                Some(b) => {
                    git(
                        &root,
                        &["worktree", "add", wt.to_string_lossy().as_ref(), b.as_str()],
                    )?;
                    println!("worktree {} on existing branch {b}", wt.display());
                }
                None => {
                    let new_branch = format!("ka/{name}");
                    git(
                        &root,
                        &[
                            "worktree",
                            "add",
                            "-b",
                            new_branch.as_str(),
                            wt.to_string_lossy().as_ref(),
                        ],
                    )?;
                    println!("worktree {} on new branch {new_branch}", wt.display());
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        WorktreeCmd::List => {
            print!("{}", git(&root, &["worktree", "list", "--porcelain"])?);
            Ok(ExitCode::SUCCESS)
        }
        WorktreeCmd::Remove {
            name,
            delete_branch,
        } => {
            validate_name(&name)?;
            let wt = root.join(WORKTREES_DIR).join(&name);
            if !wt.exists() {
                return Err(format!(
                    "no worktree at {} (see `ka worktree list`)",
                    wt.display()
                ));
            }
            // resolve the checked-out branch before the worktree is gone
            let branch = parse_porcelain(&git(&root, &["worktree", "list", "--porcelain"])?)
                .into_iter()
                .find(|e| crate::same_dir(&e.path, &wt))
                .and_then(|e| e.branch);
            git(
                &root,
                &["worktree", "remove", wt.to_string_lossy().as_ref()],
            )?;
            println!("removed worktree {}", wt.display());
            if delete_branch {
                let branch = branch.ok_or_else(|| {
                    "cannot --delete-branch: the worktree had no branch checked out".to_string()
                })?;
                git(&root, &["branch", "-d", branch.as_str()])?;
                println!("deleted branch {branch}");
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Resolve the repo root, failing with a clear message when the cwd is
/// not inside a git repository.
fn require_git_repo(cwd: &Path) -> Result<PathBuf, String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        let line = String::from_utf8_lossy(&out.stdout);
        Ok(PathBuf::from(line.trim()))
    } else {
        Err(format!(
            "not inside a git repository ({}) — `ka worktree` needs one",
            cwd.display()
        ))
    }
}

/// Run git in `repo`, return trimmed stdout; stderr becomes the error.
fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Parse `git worktree list --porcelain` blocks:
/// `worktree <path>` / `HEAD <sha>` / `branch refs/heads/<name>` (+ bare/detached).
pub(crate) fn parse_porcelain(text: &str) -> Vec<WorktreeEntry> {
    let mut entries: Vec<WorktreeEntry> = Vec::new();
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            entries.push(WorktreeEntry {
                path: PathBuf::from(p),
                head: None,
                branch: None,
            });
        } else if let Some(h) = line.strip_prefix("HEAD ") {
            if let Some(last) = entries.last_mut() {
                last.head = Some(h.to_string());
            }
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            if let Some(last) = entries.last_mut() {
                last.branch = Some(b.to_string());
            }
        }
    }
    entries
}

/// Worktree names are single path components under .worktrees/.
fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "worktree name {name:?} must be a single path component \
             (letters, digits, '-', '_', '.')"
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// A unique scratch directory per test.
    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ka-wt-{}-{n}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A real git repo with one commit (worktree add needs a HEAD).
    fn temp_repo(tag: &str) -> PathBuf {
        let dir = temp_dir(tag);
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&[
            "-c",
            "user.email=ka@test",
            "-c",
            "user.name=ka",
            "commit",
            "--allow-empty",
            "-qm",
            "init",
        ]);
        dir
    }

    fn porcelain_of(dir: &Path) -> Vec<WorktreeEntry> {
        parse_porcelain(&git(dir, &["worktree", "list", "--porcelain"]).unwrap())
    }

    #[test]
    fn porcelain_parses_paths_heads_and_branches() {
        let entries = parse_porcelain(
            "worktree /repo\nHEAD abc123\nbranch refs/heads/main\n\n\
             worktree /repo/.worktrees/fix\nHEAD def456\nbranch refs/heads/ka/fix\n\n",
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, Path::new("/repo"));
        assert_eq!(entries[0].head.as_deref(), Some("abc123"));
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(entries[1].branch.as_deref(), Some("ka/fix"));
        // detached/bare blocks parse without a branch
        let bare = parse_porcelain("worktree /b (bare)\n");
        assert_eq!(bare.len(), 1);
        assert_eq!(bare[0].branch, None);
    }

    #[test]
    fn names_must_be_single_components() {
        assert!(validate_name("fix").is_ok());
        assert!(validate_name("fix-2").is_ok());
        assert!(validate_name("../escape").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("").is_err());
        assert!(validate_name("..").is_err());
    }

    #[test]
    fn add_creates_worktree_on_ka_branch() {
        let repo = temp_repo("add");
        let wt = repo.join(".worktrees").join("fix");
        let out = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                "ka/fix",
                wt.to_string_lossy().as_ref(),
            ])
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(wt.join(".git").exists());
        let entries = porcelain_of(&repo);
        let mine = entries
            .iter()
            .find(|e| e.branch.as_deref() == Some("ka/fix"))
            .expect("worktree on ka/fix");
        assert!(crate::same_dir(&mine.path, &wt));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn remove_then_delete_branch() {
        let repo = temp_repo("remove");
        let wt = repo.join(".worktrees").join("tmp");
        let add = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                "ka/tmp",
                wt.to_string_lossy().as_ref(),
            ])
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            add.status.success(),
            "{}",
            String::from_utf8_lossy(&add.stderr)
        );
        // remove the worktree, then drop its branch
        let rm = std::process::Command::new("git")
            .args(["worktree", "remove", wt.to_string_lossy().as_ref()])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            rm.status.success(),
            "{}",
            String::from_utf8_lossy(&rm.stderr)
        );
        assert!(
            !porcelain_of(&repo)
                .iter()
                .any(|e| e.branch.as_deref() == Some("ka/tmp"))
        );
        let del = std::process::Command::new("git")
            .args(["branch", "-d", "ka/tmp"])
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            del.status.success(),
            "{}",
            String::from_utf8_lossy(&del.stderr)
        );
        let branches = git(&repo, &["branch", "--list", "ka/tmp"]).unwrap();
        assert!(branches.trim().is_empty(), "{branches:?}");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn non_repo_cwd_is_a_clear_error() {
        let dir = temp_dir("norepo");
        let err = require_git_repo(&dir).unwrap_err();
        assert!(err.contains("not inside a git repository"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
