//! The checkpoint hand: the model can snapshot the working tree with
//! the same machinery the surface's checkpoint command uses — a git
//! commit built via a temporary index that never touches the user's
//! index, stash, or HEAD. Taken checkpoints land in the shared session
//! log, so `/restore` (and the model, via a matching id) can restore
//! them exactly like surface-taken ones.

use std::path::Path;

use serde_json::{Value, json};

use super::{Clearance, Hand, HandContext, HandDef, ToolOutput};

/// The session's checkpoint log: (full commit id, timestamp). Shared
/// between the `Command::Checkpoint` arm, the restore arm, and this
/// hand — one list, every taker restorable.
pub type CheckpointLog = parking_lot::Mutex<Vec<(String, String)>>;

/// Take a checkpoint at `cwd` and record it in the session log.
/// Returns (full id, timestamp). THE shared entry point: the
/// `Command::Checkpoint` arm and the `checkpoint` hand both go through
/// here, so the snapshot machinery (temp-index git commit) and the
/// session bookkeeping are identical on both paths.
pub fn take_recorded(cwd: &Path, log: &CheckpointLog) -> Result<(String, String), String> {
    let id = crate::checkpoint::snapshot(cwd)?;
    let ts = ka_strand::now_rfc3339();
    log.lock().push((id.clone(), ts.clone()));
    Ok((id, ts))
}

/// The checkpoint tool.
pub struct CheckpointHand {
    log: std::sync::Arc<CheckpointLog>,
}

impl CheckpointHand {
    /// Hand over the shared session log (must be the engine's log so
    /// `/restore` sees hand-taken checkpoints).
    pub fn new(log: std::sync::Arc<CheckpointLog>) -> Self {
        Self { log }
    }
}

impl Hand for CheckpointHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "checkpoint".to_string(),
            description: "Snapshot the working tree before risky edits: a git commit built \
                through a temporary index — your real index, stash, and HEAD are never \
                touched. Arguments: {} (none). Returns the checkpoint id; either of us can \
                restore it later by id (`/restore <id>` or git checkout of the snapshot). \
                Needs a git work tree."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
            clearance: Clearance::Exec,
            read_only: false,
        }
    }

    fn execute<'a>(
        &'a self,
        _args: &'a Value,
        ctx: &'a HandContext,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            match take_recorded(&ctx.cwd, &self.log) {
                Ok((id, ts)) => {
                    let short = crate::checkpoint::short(&id);
                    ToolOutput::ok(format!(
                        "checkpoint {short} saved ({ts}) — full id {id}; restorable with \
                         /restore {short} or `git checkout {id} -- .`"
                    ))
                }
                Err(e) => ToolOutput::err(format!("checkpoint failed: {e}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn ctx() -> HandContext {
        HandContext {
            cwd: std::env::temp_dir(),
            ledger: std::sync::Arc::new(parking_lot::Mutex::new(super::super::Ledger::default())),
            spill: std::sync::Arc::new(super::super::Spill::new()),
            snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                super::super::snapshots::Snapshots::inert(),
            )),
            jobs: std::sync::Arc::new(super::super::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
            shell_env: Default::default(),
        }
    }

    /// A git repo with user identity configured (commit-tree needs it).
    fn git_workdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-ckpt-hand-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .env("GIT_AUTHOR_NAME", "ka")
                .env("GIT_AUTHOR_EMAIL", "ka@localhost")
                .env("GIT_COMMITTER_NAME", "ka")
                .env("GIT_COMMITTER_EMAIL", "ka@localhost")
                .current_dir(&dir)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        std::fs::write(dir.join("file.txt"), "original\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);
        dir
    }

    #[tokio::test]
    async fn checkpoint_hand_creates_restorable_snapshot() {
        let dir = git_workdir("ok");
        let log = std::sync::Arc::new(CheckpointLog::default());
        let hand = CheckpointHand::new(log.clone());
        let mut cx = ctx();
        cx.cwd = dir.clone();

        // mutate the tree, snapshot it via the hand, mutate again
        std::fs::write(dir.join("file.txt"), "mutated\n").unwrap();
        let out = hand.execute(&json!({}), &cx).await;
        assert!(!out.is_error, "{}", out.content);
        let (id, _ts) = log.lock()[0].clone();
        assert!(
            out.content.contains(crate::checkpoint::short(&id)),
            "{}",
            out.content
        );
        std::fs::write(dir.join("file.txt"), "mutated-again\n").unwrap();

        // the snapshot is a real commit: restore brings the snapshotted
        // content back (the user's index, stash, and HEAD untouched)
        crate::checkpoint::restore(&dir, &id).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "mutated\n",
            "restore must bring back the snapshotted content"
        );
        let head = std::string::String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(head.trim().len(), 40);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn checkpoint_hand_outside_a_repo_errors() {
        let dir = std::env::temp_dir().join(format!("ka-ckpt-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = std::sync::Arc::new(CheckpointLog::default());
        let hand = CheckpointHand::new(log.clone());
        let mut cx = ctx();
        cx.cwd = dir.clone();
        let out = hand.execute(&json!({}), &cx).await;
        assert!(out.is_error);
        assert!(out.content.contains("checkpoint failed"), "{}", out.content);
        assert!(log.lock().is_empty(), "failed checkpoints are not recorded");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
