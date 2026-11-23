//! Safe-mode contract (`ka --safe-mode` / `KA_SAFEMODE=1`): every
//! customization tier goes dark while built-ins, config, rules, and
//! auth stay. Isolated in its own test binary because the flag is a
//! process-global.

#![allow(clippy::unwrap_used, clippy::expect_used)]
use ka_engine::conventions;
use ka_engine::hands::Hand as _;
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

/// Lote 7: the cron hand NEVER arms in safe mode — `create` refuses
/// (an unattended repeat executor is a customization-tier risk the
/// bare surface pins off, like the loop) and the engine's tick path is
/// guarded separately, so even a hypothetical entry could not fire.
#[tokio::test]
async fn safe_mode_never_arms_cron() {
    {
        let _guard = FLAG_LOCK.lock();
        conventions::set_bare_mode(true);
    }
    let dir = std::env::temp_dir().join(format!("ka-bare-cron-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let table = std::sync::Arc::new(ka_engine::hands::cron::CronTable::new());
    let hand = ka_engine::hands::cron::CronHand::new(table.clone());
    let ctx = ka_engine::hands::HandContext {
        cwd: dir.clone(),
        ledger: std::sync::Arc::new(parking_lot::Mutex::new(ka_engine::hands::Ledger::default())),
        spill: std::sync::Arc::new(ka_engine::hands::Spill::new()),
        snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
            ka_engine::hands::snapshots::Snapshots::inert(),
        )),
        jobs: std::sync::Arc::new(ka_engine::hands::jobs::JobTable::new()),
        bash_background_ms: 0,
        max_image_mb: 5,
        web_allow_private: false,
        sandbox: ka_sandbox::Policy::Off,
        shell_env: Default::default(),
    };
    conventions::set_bare_mode(true);
    let out = hand
        .execute(
            &serde_json::json!({
                "prompt": "run the checks",
                "interval_secs": 1
            }),
            &ctx,
        )
        .await;
    conventions::set_bare_mode(false);
    assert!(
        out.is_error,
        "cron create must refuse in safe mode: {}",
        out.content
    );
    assert!(
        out.content.contains("safe mode"),
        "the refusal names safe mode: {}",
        out.content
    );
    assert!(
        table.snapshot().is_empty(),
        "nothing was armed despite the refusal"
    );
    // outside safe mode the same create arms normally (the tool exists
    // — it is the BARE surface that pins it off)
    let out = hand
        .execute(
            &serde_json::json!({
                "prompt": "run the checks",
                "interval_secs": 60
            }),
            &ctx,
        )
        .await;
    assert!(
        !out.is_error,
        "cron works outside safe mode: {}",
        out.content
    );
    assert_eq!(table.snapshot().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// EXT-12: the team hand NEVER spawns members in safe mode —
/// session-spawning automation is a customization-tier risk the bare
/// surface pins off (the cron/monitor precedent). `team_spawn` refuses
/// with a safe-mode error, no member engine ever arms; the same hand
/// outside safe mode reaches its normal validation (an invalid name
/// fails on the grammar, not on safe mode — and nothing spawns).
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn safe_mode_never_spawns_team_members() {
    {
        let _guard = FLAG_LOCK.lock();
        conventions::set_bare_mode(true);
    }
    let dir = std::env::temp_dir().join(format!("ka-bare-team-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let strand = ka_strand::StrandFile::create(&dir, None).unwrap();
    let (events, _rx) = tokio::sync::mpsc::channel(16);
    let table = ka_engine::hands::teams::TeamTable::new(
        8,
        &dir,
        ka_dialect::Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
        )
        .unwrap(),
        ka_engine::Config::default(),
        std::sync::Arc::new(parking_lot::RwLock::new(ka_protocol::Mode::Allow)),
        events,
        &strand,
        None,
    );
    let hand = ka_engine::hands::teams::TeamsHand::new(table);
    let ctx = ka_engine::hands::HandContext {
        cwd: dir.clone(),
        ledger: std::sync::Arc::new(parking_lot::Mutex::new(ka_engine::hands::Ledger::default())),
        spill: std::sync::Arc::new(ka_engine::hands::Spill::new()),
        snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
            ka_engine::hands::snapshots::Snapshots::inert(),
        )),
        jobs: std::sync::Arc::new(ka_engine::hands::jobs::JobTable::new()),
        bash_background_ms: 0,
        max_image_mb: 5,
        web_allow_private: false,
        sandbox: ka_sandbox::Policy::Off,
        shell_env: Default::default(),
    };
    {
        let _guard = FLAG_LOCK.lock();
        conventions::set_bare_mode(true);
        let out = hand
            .execute(
                &serde_json::json!({
                    "action": "spawn",
                    "name": "scout",
                    "prompt": "survey the repo"
                }),
                &ctx,
            )
            .await;
        conventions::set_bare_mode(false);
        assert!(
            out.is_error && out.content.contains("safe mode"),
            "team spawn must refuse in safe mode: {}",
            out.content
        );
    }
    // outside safe mode the same hand reaches its grammar (nothing
    // spawns for an invalid name — the refusal is the name, not the mode).
    // Still under FLAG_LOCK: sibling tests flip the process-global bare
    // flag while holding it, and this execute reads the flag at its top.
    {
        let _guard = FLAG_LOCK.lock();
        let out = hand
            .execute(
                &serde_json::json!({
                    "action": "spawn",
                    "name": "Scout!",
                    "prompt": "survey the repo"
                }),
                &ctx,
            )
            .await;
        assert!(
            out.is_error && out.content.contains("invalid name"),
            "outside safe mode the refusal is the name grammar: {}",
            out.content
        );
    }
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
        match tokio::time::timeout(std::time::Duration::from_secs(10), handle.events.recv()).await {
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

// ---------------------------------------------------------- Lote 9 posture

/// A speaker that pops one scripted tool call per tool-bearing request
/// and answers tool-less (reviewer) probes from `verdict` — the same
/// contract as feature_contracts' ReviewSpeaker, local to this binary.
struct BareReviewSpeaker {
    calls: parking_lot::Mutex<std::collections::VecDeque<(&'static str, serde_json::Value)>>,
    verdict: &'static str,
}

impl ka_dialect::speaker::Speaker for BareReviewSpeaker {
    fn speak<'a>(
        &'a self,
        req: ka_dialect::speaker::SpeakRequest,
        out: tokio::sync::mpsc::Sender<ka_dialect::speaker::StreamEvent>,
    ) -> ka_dialect::speaker::SpeakFuture<'a> {
        Box::pin(async move {
            use ka_dialect::speaker::{StreamEvent, ToolCall};
            use ka_protocol::Stop;
            if req.tools.is_empty() {
                out.send(StreamEvent::Text(self.verdict.into())).await.ok();
            } else {
                let next = self.calls.lock().pop_front();
                if let Some((tool, args)) = next {
                    out.send(StreamEvent::Call(ToolCall {
                        id: "c1".into(),
                        tool: tool.into(),
                        arguments: args,
                    }))
                    .await
                    .ok();
                } else {
                    out.send(StreamEvent::Text("done".into())).await.ok();
                }
            }
            out.send(StreamEvent::Finished {
                stop: Stop::Done,
                usage: ka_protocol::Usage {
                    input: 1,
                    output: 1,
                    ..Default::default()
                },
            })
            .await
            .ok();
        })
    }
}

/// Drive one turn with a single scripted call (asks answered 0=allow /
/// 2=deny; None leaves them standing).
async fn run_bare_turn(
    voice: &mut ka_engine::voice::Voice,
    prompt: &str,
    answer: Option<usize>,
) -> Vec<ka_protocol::Event> {
    use std::collections::VecDeque;
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<ka_protocol::Command>(16);
    let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel::<ka_protocol::Event>(256);
    let mut steers = Vec::new();
    let mut queue = VecDeque::new();
    let mut guards = ka_engine::voice::GuardRuntime::default();
    let fut = voice.turn(
        "test/m",
        prompt.to_string(),
        &mut cmd_rx,
        &evt_tx,
        &mut steers,
        &mut queue,
        &mut guards,
        None,
        Vec::new(),
    );
    tokio::pin!(fut);
    let mut events = Vec::new();
    loop {
        tokio::select! {
            biased;
            evt = evt_rx.recv() => match evt {
                Some(e) => {
                    if let ka_protocol::Event::Ask { id, .. } = &e {
                        if let Some(choice) = answer {
                            cmd_tx
                                .send(ka_protocol::Command::Answer {
                                    question: id.clone(),
                                    choice,
                                })
                                .await
                                .ok();
                        }
                    }
                    events.push(e);
                }
                None => break,
            },
            _ = &mut fut => {
                while let Ok(e) = evt_rx.try_recv() {
                    events.push(e);
                }
                break;
            }
        }
    }
    events
}

fn bare_catalog() -> ka_dialect::Catalog {
    ka_dialect::Catalog::parse(
        "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
    )
    .unwrap()
}

/// Lote 9: the new permission features keep the bare-mode split — the
/// pure gate posture (`[permissions] prompt_policy`, rules-class
/// config) stays enforced, while the LLM tiers (`[guards]
/// auto_review`, the injection classifier) stay dark: an ask armed
/// with a confident reviewer still reaches the human, with no
/// auto_review note.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn safe_mode_keeps_llm_guards_dark_and_gate_posture_on() {
    use ka_engine::config::{AutoReviewTier, PromptPolicy};
    use ka_protocol::Event;

    let _guard = FLAG_LOCK.lock();
    let dir = std::env::temp_dir().join(format!("ka-bare-perms-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    conventions::set_bare_mode(true);

    // prompt_policy deny refuses the reviewable exec ask in safe mode
    let mut voice =
        ka_engine::voice::Voice::new(bare_catalog(), dir.clone(), ka_protocol::Mode::Ask, 5)
            .with_speaker(
                ka_dialect::Wire::OpenaiChat,
                std::sync::Arc::new(BareReviewSpeaker {
                    calls: parking_lot::Mutex::new(std::collections::VecDeque::from([(
                        "bash",
                        serde_json::json!({"command": "touch bare.txt"}),
                    )])),
                    verdict: "allow",
                }),
            );
    voice.set_prompt_policy(PromptPolicy::Deny);
    let events = run_bare_turn(&mut voice, "build", None).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::CallOutput { is_error: true, .. })),
        "prompt policy denies in safe mode too: {events:?}"
    );
    assert!(!dir.join("bare.txt").exists());
    drop(voice);

    // auto_review never consults the reviewer in safe mode: the ask
    // reaches the human (answered allow here), no auto_review note
    let marker = dir.join("human-bare.txt");
    let mut voice =
        ka_engine::voice::Voice::new(bare_catalog(), dir.clone(), ka_protocol::Mode::Ask, 5)
            .with_speaker(
                ka_dialect::Wire::OpenaiChat,
                std::sync::Arc::new(BareReviewSpeaker {
                    calls: parking_lot::Mutex::new(std::collections::VecDeque::from([(
                        "bash",
                        serde_json::json!({"command": format!("touch {}", marker.display())}),
                    )])),
                    verdict: "allow",
                }),
            );
    voice.set_auto_review(true, Some("test/m".to_string()), AutoReviewTier::ExecWrite);
    voice.set_injection_check(true);
    let events = run_bare_turn(&mut voice, "build", Some(0)).await;
    let asked = events.iter().any(|e| matches!(e, Event::Ask { .. }));
    assert!(
        asked,
        "bare mode never lets the reviewer decide: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Note { message, .. }
            if message.contains("auto_review"))),
        "no reviewer notes in safe mode: {events:?}"
    );
    assert!(marker.exists(), "the human's allow runs the command");
    conventions::set_bare_mode(false);
    let _ = std::fs::remove_dir_all(&dir);
}
