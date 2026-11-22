//! Safe-mode contract (`ka --safe-mode` / `KA_SAFEMODE=1`): every
//! customization tier goes dark while built-ins, config, rules, and
//! auth stay. Isolated in its own test binary because the flag is a
//! process-global.

#![allow(clippy::unwrap_used, clippy::expect_used)]
use ka_engine::conventions;
use ka_engine::trust;

/// The bare-mode flag is a process-global and this binary's tests all
/// flip it — they run serialized.
static FLAG_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[test]
fn safe_mode_disables_every_customization_tier() {
    let _guard = FLAG_LOCK.lock();
    let dir = std::env::temp_dir().join(format!("ka-bare-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // a fully-loaded project: instructions, memory, skills, rules
    std::fs::create_dir_all(dir.join(".ka/skills/ship")).unwrap();
    std::fs::write(
        dir.join(".ka/skills/ship/SKILL.md"),
        "---\ndescription: ship it\n---\nbody",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join(".ka/rules")).unwrap();
    std::fs::write(dir.join(".ka/rules/style.md"), "always on rule").unwrap();
    std::fs::create_dir_all(dir.join(".ka/agents")).unwrap();
    std::fs::write(dir.join(".ka/agents/reviewer.md"), "you review").unwrap();
    std::fs::write(dir.join("AGENTS.md"), "project instructions").unwrap();
    std::fs::write(dir.join("MEMORY.md"), "project memory").unwrap();

    trust::test_support::with_trust_file(|_| {
        trust::approve(&dir);

        // everything loads in normal mode
        assert!(
            !conventions::discover_agents(&dir).is_empty(),
            "AGENTS.md loads"
        );
        assert!(
            !conventions::discover_memory(&dir).is_empty(),
            "MEMORY.md loads"
        );
        assert!(
            conventions::discover_skills(&dir)
                .iter()
                .any(|s| s.name == "ship"),
            "skills load"
        );
        assert!(!conventions::discover_rules(&dir).is_empty(), "rules load");
        assert!(
            !ka_engine::agents::AgentDef::discover(&dir).is_empty(),
            "markdown agents load"
        );

        // safe mode: all of it goes dark
        conventions::set_bare_mode(true);
        assert!(
            conventions::discover_agents(&dir).is_empty(),
            "safe mode hides AGENTS.md"
        );
        assert!(
            conventions::discover_memory(&dir).is_empty(),
            "safe mode hides MEMORY.md"
        );
        assert!(
            conventions::discover_skills(&dir).is_empty(),
            "safe mode hides skills"
        );
        assert!(
            conventions::discover_rules(&dir).is_empty(),
            "safe mode hides scoped rules"
        );
        // markdown agents are gated at the engine call site (the engine
        // skips AgentDef::discover in bare mode), not inside discovery —
        // so discovery still finds them here; that layering is the
        // contract, don't "fix" one side without the other
        assert!(
            !ka_engine::agents::AgentDef::discover(&dir).is_empty(),
            "agent discovery is engine-gated, not self-gated"
        );
        conventions::set_bare_mode(false);

        // back on: everything returns
        assert!(!conventions::discover_agents(&dir).is_empty());
    });

    // the [lsp] tier — spawned servers, navigation hands, write-through —
    // is a customization tier like any other: bare mode forces
    // Lsp::default(), so neither gate (enable / write_through) can fire
    let loaded = ka_engine::config::Config {
        lsp: ka_engine::config::Lsp {
            enable: Some(true),
            commands: Some(
                [("rust".to_string(), "rust-analyzer".to_string())]
                    .into_iter()
                    .collect(),
            ),
            write_through: Some(true),
        },
        ..ka_engine::config::Config::default()
    };
    conventions::set_bare_mode(true);
    let lsp = ka_engine::effective_lsp_cfg(&loaded);
    conventions::set_bare_mode(false);
    assert_ne!(
        lsp.enable,
        Some(true),
        "bare mode never spawns language servers"
    );
    assert_ne!(
        lsp.write_through,
        Some(true),
        "bare mode never registers write-through hands"
    );
    assert_ne!(
        ka_engine::effective_debug_cfg(&loaded).enable,
        Some(true),
        "bare mode never spawns debug adapters"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Feature toggles can never WIDEN a safe-mode session: `SetFeature
/// enabled=true` and `SetSandbox fs` are refused with a note, and no
/// FeaturesChanged announcement is made.
///
/// The guard is held across awaits on purpose — it serializes this
/// test against the sync one over the process-global flag — and cannot
/// deadlock: the other holder blocks on plain lock acquisition, not on
/// this task making progress.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn safe_mode_pins_feature_toggles_off() {
    let _guard = FLAG_LOCK.lock();
    use ka_protocol::{Command, Event};
    let dir = std::env::temp_dir().join(format!("ka-bare-tgl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let catalog = ka_dialect::Catalog::parse(
        "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
    )
    .unwrap();
    conventions::set_bare_mode(true);
    let mut handle = ka_engine::spawn_with(cfg, catalog);
    for cmd in [
        Command::SetFeature {
            spec: "agents".parse().unwrap(),
            enabled: true,
        },
        Command::SetSandbox {
            mode: "fs".to_string(),
        },
    ] {
        handle.commands.send(cmd).await.expect("engine alive");
    }
    let mut refused = 0usize;
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), handle.events.recv()).await {
            Ok(Some(e)) => {
                assert!(
                    !matches!(e, Event::FeaturesChanged { .. }),
                    "bare mode never announces a widening toggle: {e:?}"
                );
                if matches!(
                    &e,
                    Event::Note { message, .. } if message.contains("safe mode pins")
                ) {
                    refused += 1;
                }
                if refused == 2 {
                    break;
                }
            }
            _ => panic!("expected two refusals, saw {refused}"),
        }
    }
    conventions::set_bare_mode(false);
    let _ = std::fs::remove_dir_all(&dir);
}

/// CTX-06: the loop never arms in safe mode — an unattended repeat
/// executor is a customization-tier risk the bare surface pins off.
/// Set is refused with a note, no cycle ever fires.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn safe_mode_never_arms_the_loop() {
    let _guard = FLAG_LOCK.lock();
    use ka_protocol::{Command, Event};
    let dir = std::env::temp_dir().join(format!("ka-bare-loop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let catalog = ka_dialect::Catalog::parse(
        "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
    )
    .unwrap();
    conventions::set_bare_mode(true);
    let mut handle = ka_engine::spawn_with(cfg, catalog);
    handle
        .commands
        .send(Command::Loop {
            action: ka_protocol::LoopAction::Set {
                task: "run the checks".into(),
                interval_secs: None,
            },
        })
        .await
        .expect("engine alive");
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), handle.events.recv()).await {
            Ok(Some(Event::Note { message, .. })) => {
                if message.contains("loop unavailable in safe mode") {
                    break;
                }
            }
            Ok(Some(_)) => continue,
            _ => panic!("loop Set refusal never arrived"),
        }
    }
    // and no cycle ever fires: drain trailing events (Idle etc.) and
    // watch for turn events over a 1.5s quiet window
    let mut fired = false;
    for _ in 0..10 {
        match tokio::time::timeout(std::time::Duration::from_millis(150), handle.events.recv())
            .await
        {
            Ok(Some(Event::TurnFinished { .. })) => {
                fired = true;
                break;
            }
            Ok(Some(_)) => continue,
            _ => break,
        }
    }
    assert!(!fired, "loop fired despite safe mode");
    conventions::set_bare_mode(false);
    let _ = std::fs::remove_dir_all(&dir);
}
