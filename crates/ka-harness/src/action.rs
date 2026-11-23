//! `ka action init` (AUTO-01): vendor a GitHub Actions workflow that
//! runs ka read-only against PRs. The workflow (a single vendored
//! template, compiled in via `include_str!`) triggers on
//! `workflow_dispatch` and on a PR labeled `ka`, checks the PR out at
//! its pinned HEAD SHA, installs the released `ka` binary (sha256
//! verified against the release's `.sha256` asset), runs
//! `ka run --mode readonly --print text "<prompt from the PR body>"`,
//! and posts the answer as a PR comment. Its only secret surface is
//! `KA_MODEL` plus the `KA_*_API_KEY` provider keys.
//!
//! The command never overwrites an existing workflow unless `--force`
//! is passed: the vendored file is a starting point users are expected
//! to edit, and silently regenerating it would discard those edits.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The vendored workflow, compiled into the binary so `ka action init`
/// works offline and can never drift from the shipped template.
pub(crate) const WORKFLOW_TEMPLATE: &str = include_str!("../templates/ka-workflow.yml");

/// Repo-relative destination of the vendored workflow.
pub(crate) const WORKFLOW_REL_PATH: &str = ".github/workflows/ka.yml";

/// `ka action <cmd>`
#[derive(clap::Subcommand, Clone, Debug)]
pub enum ActionCmd {
    /// Write .github/workflows/ka.yml from the vendored template
    /// (refuses to overwrite unless --force)
    Init {
        /// Overwrite an existing ka.yml
        #[arg(long)]
        force: bool,
    },
}

/// Run `ka action <cmd>` — resolves the repo root from the cwd, like
/// `ka init` does.
pub fn run(cmd: ActionCmd) -> Result<ExitCode, String> {
    match cmd {
        ActionCmd::Init { force } => {
            let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
            let root = ka_engine::project_root(&cwd);
            let path = write_workflow(&root, force)?;
            println!("wrote {}", path.display());
            println!(
                "commit it, then add the KA_MODEL / KA_*_API_KEY secrets \
                 (Settings → Secrets and variables → Actions); a PR labeled \
                 `ka` runs the agent read-only and comments its answer"
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Write the vendored workflow under `root` (the repo root). An existing
/// file is an error unless `force` — idempotence by refusal, the same
/// contract as `ka init` for AGENTS.md. Returns the written path.
pub(crate) fn write_workflow(root: &Path, force: bool) -> Result<PathBuf, String> {
    let target = root.join(WORKFLOW_REL_PATH);
    if target.exists() && !force {
        return Err(format!(
            "{} already exists; refusing to overwrite (pass --force to replace it)",
            target.display()
        ));
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::write(&target, WORKFLOW_TEMPLATE)
        .map_err(|e| format!("write {}: {e}", target.display()))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Unique scratch root per test (process id + counter, the same
    /// shape the other harness modules use).
    fn temp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ka-action-{}-{n}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// AUTO-01 (a)–(e): the vendored workflow carries every contract
    /// section — dispatch + `ka` label trigger, pinned PR checkout,
    /// checksum-verified release install, the readonly run with the
    /// body as prompt — and exposes only KA_MODEL / KA_*_API_KEY
    /// secrets.
    #[test]
    fn template_carries_every_contract_section() {
        let t = WORKFLOW_TEMPLATE;
        assert!(t.contains("workflow_dispatch"), "dispatch trigger");
        assert!(t.contains("pull_request_target"), "label trigger");
        assert!(
            t.contains("github.event.label.name == 'ka'"),
            "label gate on `ka`"
        );
        assert!(
            t.contains("github.event.pull_request.head.sha"),
            "checkout pins the PR HEAD SHA"
        );
        assert!(
            t.contains("actions/checkout@08c6903cd8c0fde910a37f88322edcfb5dd907a8"),
            "the checkout action itself is SHA-pinned"
        );
        assert!(
            t.contains("https://api.github.com/repos/jaltez/ka-harness/releases/latest"),
            "install from the latest release of jaltez/ka-harness"
        );
        assert!(
            t.contains("sha256sum --check"),
            "release install is checksum verified"
        );
        assert!(
            t.contains("ka run --mode readonly --print text"),
            "the run is readonly and prints text"
        );
        assert!(
            t.contains("KA_PROMPT:"),
            "the body reaches the run as an env var (no shell injection)"
        );
        assert!(
            t.contains("gh pr comment"),
            "the answer is posted as a comment"
        );
        // (e): every `secrets.*` reference is KA_MODEL or a KA_*_API_KEY
        for chunk in t.split("secrets.") {
            let mut name = chunk.chars();
            let mut collected = String::new();
            for c in name.by_ref() {
                if c.is_alphanumeric() || c == '_' {
                    collected.push(c);
                } else {
                    break;
                }
            }
            if collected.is_empty() {
                continue;
            }
            assert!(
                collected == "KA_MODEL" || collected.ends_with("_API_KEY"),
                "template references secrets.{collected} — only KA_MODEL \
                 and KA_*_API_KEY may appear"
            );
        }
    }

    #[test]
    fn writes_template_then_refuses_then_force_overwrites() {
        let root = temp_root("write");
        let path = write_workflow(&root, false).unwrap();
        assert_eq!(path, root.join(".github/workflows/ka.yml"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), WORKFLOW_TEMPLATE);

        // idempotence by refusal: a second plain run errors and leaves
        // the file byte-identical (user edits survive)
        std::fs::write(&path, "# my edits\n").unwrap();
        let err = write_workflow(&root, false).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert!(err.contains("--force"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# my edits\n");

        // --force regenerates from the template
        write_workflow(&root, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), WORKFLOW_TEMPLATE);
        let _ = std::fs::remove_dir_all(&root);
    }
}
