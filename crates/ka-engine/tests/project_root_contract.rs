//! Project-root anchoring contract: ka's project-scope `.ka/` layer and
//! everything ka generates into it live at the root project — the
//! nearest `.git` ancestor of the launch directory (else the launch dir
//! itself) — so a session started in a subdirectory shares one `.ka`
//! with the rest of the repo. Documented in README ("Config"); needs
//! `test-util` for the trust-store redirection.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use ka_engine::hands::{self, Hand, HandContext};

/// A git-rooted tree: `root/.git` marker plus a nested launch dir.
fn git_tree(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("ka-root-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let deep = root.join("deep").join("nested");
    std::fs::create_dir_all(&deep).unwrap();
    (root, deep)
}

fn ctx_for(cwd: &Path) -> HandContext {
    HandContext {
        cwd: cwd.to_path_buf(),
        ledger: Arc::new(parking_lot::Mutex::new(hands::Ledger::default())),
        spill: Arc::new(hands::Spill::new()),
        snapshots: Arc::new(parking_lot::Mutex::new(hands::snapshots::Snapshots::open(
            cwd,
        ))),
        jobs: Arc::new(hands::jobs::JobTable::new()),
        bash_background_ms: 0,
        max_image_mb: 0,
        web_allow_private: false,
        sandbox: ka_sandbox::Policy::Off,
        shell_env: Default::default(),
    }
}

/// Generated state — the always-allow permission save and the remember
/// inbox — lands in the root project's `.ka/`, never in the launch
/// subdirectory's.
#[tokio::test]
async fn generated_state_lands_in_git_root_ka() {
    let (root, deep) = git_tree("gen");

    // the exact save path the "always" ask option takes
    let saved = ka_engine::config::save_project_permission(&deep, "bash");
    assert_eq!(
        saved.as_deref(),
        Some(root.join(".ka/ka.toml").as_path()),
        "always-allow persists at the root project"
    );
    assert!(root.join(".ka/ka.toml").is_file());
    assert!(!deep.join(".ka").exists(), "no .ka in the launch subdir");

    // remember stages into the one project-level inbox
    let ctx = ctx_for(&deep);
    let out = hands::memory::RememberHand
        .execute(
            &serde_json::json!({"note": "prefer parking_lot locks"}),
            &ctx,
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    let inbox = std::fs::read_to_string(root.join(".ka/memory/inbox.md")).unwrap();
    assert!(inbox.contains("prefer parking_lot locks"), "{inbox}");
    assert!(
        !deep.join(".ka").exists(),
        "still no .ka in the launch subdir"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The trust store and project-scope discovery address the root project:
/// approving from a subdirectory unlocks skills and rules that live in
/// the root's `.ka/`.
#[test]
fn trust_and_discovery_anchor_at_git_root() {
    let (root, deep) = git_tree("trust");
    let ship = root.join(".ka/skills/ship");
    std::fs::create_dir_all(&ship).unwrap();
    std::fs::write(
        ship.join("SKILL.md"),
        "---\ndescription: ship it\n---\nbody",
    )
    .unwrap();
    let rules = root.join(".ka/rules");
    std::fs::create_dir_all(&rules).unwrap();
    std::fs::write(rules.join("style.md"), "always-on rule").unwrap();

    ka_engine::trust::test_support::with_trust_file(|_| {
        // untrusted: nothing PROJECT-scope loads, even though it exists
        // (user-scope skills still load, ungated)
        assert!(!ka_engine::trust::project_trusted(&deep));
        let names: Vec<String> = ka_engine::conventions::discover_skills(&deep)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(
            !names.contains(&"ship".to_string()),
            "untrusted project skill must not load: {names:?}"
        );

        // approval fired from the launch subdir addresses the root
        ka_engine::trust::approve(&deep);
        assert!(ka_engine::trust::project_trusted(&deep));
        assert!(
            ka_engine::trust::project_trusted(&root),
            "one approval covers every directory in the project"
        );
        let names: Vec<String> = ka_engine::conventions::discover_skills(&deep)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(
            names.iter().any(|n| n == "ship"),
            "root skills load from a subdir launch: {names:?}"
        );
        let rules: Vec<String> = ka_engine::conventions::discover_rules(&deep)
            .into_iter()
            .map(|r| r.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            rules.iter().any(|n| n == "style.md"),
            "root rules load from a subdir launch: {rules:?}"
        );
    });

    let _ = std::fs::remove_dir_all(&root);
}

/// Without a `.git` marker anywhere, the launch dir itself stays the
/// root — non-git projects keep launch-dir behavior.
#[test]
fn unmarked_directory_keeps_launch_dir_root() {
    let dir = std::env::temp_dir().join(format!("ka-nogit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let deep = dir.join("deep");
    std::fs::create_dir_all(&deep).unwrap();

    let saved = ka_engine::config::save_project_permission(&deep, "bash");
    assert_eq!(
        saved.as_deref(),
        Some(deep.join(".ka/ka.toml").as_path()),
        "no git root: the launch dir is the project"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
