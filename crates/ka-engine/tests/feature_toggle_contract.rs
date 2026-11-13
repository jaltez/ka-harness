//! Runtime feature-toggle contracts (`--disable`/`[features]`/`/features`
//! → `Command::SetFeature`/`SetSandbox`): disabling hides a capability
//! from the model's tool list AND rejects stray calls; enabling restores
//! it (spawning configured MCP servers when needed); every change lands
//! as a strand `Change` snapshot that resume restores. If one of these
//! breaks, a user-visible toggle broke — deliberate changes must update
//! this file and the docs together.

#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::collections::VecDeque;
use std::time::Duration;

use ka_dialect::Catalog;
use ka_dialect::speaker::{SpeakFuture, SpeakRequest, Speaker, StreamEvent, ToolCall};
use ka_engine::trust;
use ka_protocol::{Command, Event};

fn test_catalog() -> ka_dialect::Catalog {
    Catalog::parse(
        "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
    )
    .unwrap()
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-tgl-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A speaker that records every request's tool names and system prompt,
/// emits one scripted tool call per round (when the queue is non-empty —
/// tests can re-arm it), and otherwise replies with final text.
struct Rec {
    tool_names: parking_lot::Mutex<Vec<Vec<String>>>,
    systems: parking_lot::Mutex<Vec<String>>,
    calls: parking_lot::Mutex<VecDeque<(&'static str, serde_json::Value)>>,
}

impl Rec {
    fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            tool_names: parking_lot::Mutex::new(Vec::new()),
            systems: parking_lot::Mutex::new(Vec::new()),
            calls: parking_lot::Mutex::new(VecDeque::new()),
        })
    }

    fn arm(&self, calls: Vec<(&'static str, serde_json::Value)>) {
        *self.calls.lock() = calls.into();
    }
}

impl Speaker for Rec {
    fn speak<'a>(
        &'a self,
        req: SpeakRequest,
        out: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> SpeakFuture<'a> {
        Box::pin(async move {
            self.tool_names
                .lock()
                .push(req.tools.iter().map(|t| t.name.clone()).collect());
            self.systems.lock().push(req.system.clone());
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
            out.send(StreamEvent::Finished {
                stop: ka_protocol::Stop::Done,
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

/// Send a command and collect events until `done` matches, then drain
/// until the engine goes quiet (300 ms).
async fn settle(
    handle: &mut ka_engine::EngineHandle,
    cmd: Command,
    done: impl Fn(&Event) -> bool,
) -> Vec<Event> {
    handle.commands.send(cmd).await.expect("engine alive");
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(15), handle.events.recv()).await {
            Ok(Some(e)) => {
                let complete = done(&e);
                out.push(e);
                if complete {
                    break;
                }
            }
            Ok(None) => return out,
            Err(_) => panic!("engine event did not arrive within 15s; got {out:?}"),
        }
    }
    loop {
        match tokio::time::timeout(Duration::from_millis(300), handle.events.recv()).await {
            Ok(Some(e)) => out.push(e),
            _ => return out,
        }
    }
}

fn on_idle(e: &Event) -> bool {
    matches!(e, Event::Idle)
}

fn prompt(text: &str) -> Command {
    Command::Prompt {
        text: text.into(),
        schema: None,
        images: Vec::new(),
        allowed_tools: None,
        model: Some("test/m".into()),
        skills: Vec::new(),
    }
}

fn set_feature(spec: &str, enabled: bool) -> Command {
    Command::SetFeature {
        spec: spec.parse().unwrap(),
        enabled,
    }
}

fn write_agent(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join(".ka/agents")).unwrap();
    std::fs::write(
        dir.join(".ka/agents/scout.md"),
        "---\ndescription: scouts\n---\nyou scout",
    )
    .unwrap();
}

fn engine(
    dir: &std::path::Path,
    speaker: std::sync::Arc<Rec>,
    strand: ka_engine::StrandChoice,
) -> ka_engine::EngineHandle {
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        strand,
        ka_dialect::Wire::OpenaiChat,
        speaker,
    )
}

/// The whole agents arc: visible at start, hidden (and stray calls
/// rejected by name) after `SetFeature off`, restored after `on`, and
/// the whole thing survives a strand resume.
#[tokio::test]
async fn agents_toggle_hides_rejects_and_restores() {
    let dir = tmp_dir("agents");
    write_agent(&dir);
    // project agent dirs are trust-gated (same as rules/skills/hooks):
    // hold a redirected store open across the awaits, then approve
    let (_store, _guard) = trust::test_support::trust_guard();
    trust::approve(&dir);
    let rec = Rec::new();
    let mut handle = engine(&dir, rec.clone(), ka_engine::StrandChoice::New);

    // baseline: the delegate + tasks hands are offered
    settle(&mut handle, prompt("hi"), on_idle).await;
    assert!(
        rec.tool_names
            .lock()
            .last()
            .unwrap()
            .contains(&"delegate".to_string()),
        "delegate offered at start: {:?}",
        rec.tool_names.lock()
    );

    // toggle off: announced once, inventory refreshed without the hands
    let events = settle(&mut handle, set_feature("agents", false), on_idle).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::FeaturesChanged { .. }))
            .count(),
        1,
        "one FeaturesChanged: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::FeaturesChanged { disabled, .. } if disabled == &vec!["agents".to_string()]
        )),
        "snapshot names agents: {events:?}"
    );
    let inventory: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::Inventory { .. }))
        .collect();
    assert_eq!(inventory.len(), 1, "inventory re-emitted: {events:?}");
    match inventory[0] {
        Event::Inventory {
            tools, disabled, ..
        } => {
            assert!(!tools.contains(&"delegate".to_string()), "{tools:?}");
            assert!(!tools.contains(&"tasks".to_string()), "{tools:?}");
            assert_eq!(disabled, &vec!["agents".to_string()]);
        }
        _ => unreachable!(),
    }

    // next prompt: not offered, and a stray call is rejected naming the
    // toggle (fail closed against hallucinated names)
    rec.arm(vec![(
        "delegate",
        serde_json::json!({"agent": "scout", "task": "look around"}),
    )]);
    let events = settle(&mut handle, prompt("again"), on_idle).await;
    let names = rec.tool_names.lock().last().cloned().unwrap();
    assert!(!names.contains(&"delegate".to_string()), "{names:?}");
    assert!(!names.contains(&"tasks".to_string()), "{names:?}");
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::CallOutput { is_error: true, excerpt, .. }
                if excerpt.contains("disabled by a session feature toggle")
        )),
        "stray delegate rejected naming the toggle: {events:?}"
    );

    // toggle back on: hands return
    settle(&mut handle, set_feature("agents", true), on_idle).await;
    settle(&mut handle, prompt("once more"), on_idle).await;
    assert!(
        rec.tool_names
            .lock()
            .last()
            .unwrap()
            .contains(&"delegate".to_string()),
        "delegate back after re-enable"
    );

    // re-disable, drop the engine, resume the same strand: the recorded
    // snapshot restores the toggle at bootstrap
    settle(&mut handle, set_feature("agents", false), on_idle).await;
    let newest = ka_strand::list(&dir)
        .expect("strand list")
        .into_iter()
        .map(|s| s.path)
        .max()
        .expect("one strand for this cwd");
    let rec2 = Rec::new();
    let mut handle2 = engine(&dir, rec2.clone(), ka_engine::StrandChoice::Path(newest));
    let mut announced = false;
    while let Ok(Some(e)) =
        tokio::time::timeout(Duration::from_secs(5), handle2.events.recv()).await
    {
        if let Event::FeaturesChanged { disabled, .. } = &e {
            assert_eq!(disabled, &vec!["agents".to_string()]);
            announced = true;
        }
        if matches!(e, Event::Idle | Event::Replay { .. }) && announced {
            break;
        }
    }
    assert!(announced, "resume must announce the restored toggle");
    rec2.arm(vec![(
        "delegate",
        serde_json::json!({"agent": "scout", "task": "stray"}),
    )]);
    let events = settle(&mut handle2, prompt("resumed"), on_idle).await;
    assert!(
        !rec2
            .tool_names
            .lock()
            .last()
            .unwrap()
            .contains(&"delegate".to_string()),
        "resume keeps delegate hidden"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::CallOutput { is_error: true, excerpt, .. }
                if excerpt.contains("disabled by a session feature toggle")
        )),
        "stray delegate rejected on resume: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `[features] disable` in the config baseline hides at startup.
#[tokio::test]
async fn config_disable_baseline_hides_at_startup() {
    let dir = tmp_dir("baseline");
    write_agent(&dir);
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        features: ka_engine::config::Features {
            disable: vec!["agents".to_string()],
        },
        ..Default::default()
    };
    let rec = Rec::new();
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        rec.clone(),
    );
    let events = settle(&mut handle, prompt("hi"), on_idle).await;
    let inventory = events
        .iter()
        .find_map(|e| match e {
            Event::Inventory { tools, .. } => Some(tools.clone()),
            _ => None,
        })
        .expect("bootstrap inventory");
    assert!(
        !inventory.contains(&"delegate".to_string()),
        "config-disabled at startup: {inventory:?}"
    );
    assert!(
        !rec.tool_names
            .lock()
            .last()
            .unwrap()
            .contains(&"delegate".to_string()),
        "model never sees delegate"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `[features] disable` / `--disable` with an unknown spec is a hard
/// startup error naming the spec (`parse_specs` runs first thing in the
/// engine bootstrap; engine startup errors go to stderr, so the
/// validation itself is the contract here).
#[test]
fn config_disable_rejects_unknown_specs() {
    let ok = ka_engine::features::parse_specs(&[
        "agents".to_string(),
        "mcp:github".to_string(),
        "skill:pdf".to_string(),
        "tool:bash".to_string(),
        "agent:reviewer".to_string(),
    ]);
    assert_eq!(ok.expect("every grammar form parses").len(), 5);
    let err = ka_engine::features::parse_specs(&["bogus".to_string()])
        .expect_err("unknown specs must fail");
    assert!(err.contains("bogus"), "error names the spec: {err}");
    assert!(err.contains("agents"), "error carries the grammar: {err}");
}

/// `tool:<name>` hides one base tool; the model stops seeing it.
#[tokio::test]
async fn per_tool_toggle_hides_a_base_tool() {
    let dir = tmp_dir("tool");
    let rec = Rec::new();
    let mut handle = engine(&dir, rec.clone(), ka_engine::StrandChoice::New);
    settle(&mut handle, prompt("hi"), on_idle).await;
    assert!(
        rec.tool_names
            .lock()
            .last()
            .unwrap()
            .contains(&"bash".to_string()),
        "bash offered at start"
    );
    settle(&mut handle, set_feature("tool:bash", false), on_idle).await;
    settle(&mut handle, prompt("again"), on_idle).await;
    assert!(
        !rec.tool_names
            .lock()
            .last()
            .unwrap()
            .contains(&"bash".to_string()),
        "bash hidden after tool:bash off"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `skills` toggle drops the skills block from the system prompt;
/// `skill:<name>` drops one skill. (Skills are project-trust-gated, so
/// this runs inside an isolated trust store.)
#[test]
fn skills_toggle_drops_the_prompt_block() {
    let dir = tmp_dir("skills");
    let skill = dir.join(".ka/skills/toggle-demo-skill");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\ndescription: a demo skill\n---\nbody",
    )
    .unwrap();
    trust::test_support::with_trust_file(|_| {
        trust::approve(&dir);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let rec = Rec::new();
            let mut handle = engine(&dir, rec.clone(), ka_engine::StrandChoice::New);
            settle(&mut handle, prompt("hi"), on_idle).await;
            assert!(
                rec.systems
                    .lock()
                    .last()
                    .unwrap()
                    .contains("toggle-demo-skill"),
                "skill offered at start"
            );

            // whole tier off
            settle(&mut handle, set_feature("skills", false), on_idle).await;
            settle(&mut handle, prompt("again"), on_idle).await;
            assert!(
                !rec.systems
                    .lock()
                    .last()
                    .unwrap()
                    .contains("Available skills"),
                "skills block gone with `skills` off"
            );

            // tier on, one skill off
            settle(&mut handle, set_feature("skills", true), on_idle).await;
            settle(
                &mut handle,
                set_feature("skill:toggle-demo-skill", false),
                on_idle,
            )
            .await;
            settle(&mut handle, prompt("once more"), on_idle).await;
            assert!(
                !rec.systems
                    .lock()
                    .last()
                    .unwrap()
                    .contains("toggle-demo-skill"),
                "named skill gone with skill:<name> off"
            );
        });
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// `/skill:<name>` explicit invocation: the SKILL.md body rides the
/// invoking turn's system prompt only (per-turn scope, like
/// `allowed-tools`), and a disabled or unknown skill refuses
/// fail-closed — a Note naming the toggle, no turn run.
#[test]
fn skill_invocation_injects_the_body_per_turn() {
    let dir = tmp_dir("skill-invoke");
    let skill = dir.join(".ka/skills/invoke-demo-skill");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\ndescription: a demo skill\n---\nBODY-MARKER instructions",
    )
    .unwrap();
    trust::test_support::with_trust_file(|_| {
        trust::approve(&dir);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let rec = Rec::new();
            let mut handle = engine(&dir, rec.clone(), ka_engine::StrandChoice::New);
            let invoke = |text: &str, skills: Vec<&str>| Command::Prompt {
                text: text.into(),
                schema: None,
                images: Vec::new(),
                allowed_tools: None,
                model: Some("test/m".into()),
                skills: skills.into_iter().map(str::to_string).collect(),
            };

            // explicit invocation: the body rides this turn's system
            // prompt, src-tagged like the scoped-rules blocks
            settle(&mut handle, invoke("go", vec!["invoke-demo-skill"]), on_idle).await;
            let invoked = rec.systems.lock().last().unwrap().clone();
            assert!(
                invoked.contains("BODY-MARKER"),
                "skill body rides the turn: {invoked}"
            );
            assert!(
                invoked.contains("<skill src="),
                "body is src-tagged: {invoked}"
            );

            // per-turn scope: the next plain turn drops the body (the
            // session default — the one-line listing — is untouched)
            settle(&mut handle, prompt("plain"), on_idle).await;
            let plain = rec.systems.lock().last().unwrap().clone();
            assert!(
                !plain.contains("BODY-MARKER"),
                "scope ends with the turn: {plain}"
            );
            assert!(
                plain.contains("invoke-demo-skill"),
                "the listing still offers the skill: {plain}"
            );

            // fail closed: a disabled skill names the toggle, runs nothing
            let before = rec.systems.lock().len();
            settle(&mut handle, set_feature("skill:invoke-demo-skill", false), on_idle).await;
            let events = settle(&mut handle, invoke("go", vec!["invoke-demo-skill"]), on_idle).await;
            let note = events
                .iter()
                .filter_map(|e| match e {
                    Event::Note { message } => Some(message.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" | ");
            assert!(
                note.contains("skill:invoke-demo-skill"),
                "refusal names the toggle: {note}"
            );
            assert!(
                !events.iter().any(|e| matches!(e, Event::TurnStarted { .. })),
                "a refused invocation runs no turn: {events:?}"
            );
            assert_eq!(
                rec.systems.lock().len(),
                before,
                "no model call for a refused invocation"
            );

            // fail closed: an unknown skill refuses the same way
            settle(&mut handle, set_feature("skill:invoke-demo-skill", true), on_idle).await;
            let events = settle(&mut handle, invoke("go", vec!["ghost-skill"]), on_idle).await;
            assert!(
                events.iter().any(|e| matches!(
                    e,
                    Event::Note { message } if message.contains("ghost-skill") && message.contains("not found")
                )),
                "unknown skill named in the refusal: {events:?}"
            );
        });
    });
    let _ = std::fs::remove_dir_all(&dir);
}
/// exits 2 blocks a read while hooks are on, and stops blocking once
/// `hooks` is toggled off. (Hooks run only in a trusted project.)
#[test]
fn hooks_toggle_silences_config_hooks() {
    let dir = tmp_dir("hooks");
    std::fs::write(dir.join("f.txt"), "x").unwrap();
    let parsed = ka_engine::Config::parse_layer(
        "[[hooks]]\nevent = \"pre_tool_use\"\ncommand = \"sh -c 'exit 2'\"\n",
        "project",
    )
    .unwrap();
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..parsed
    };
    trust::test_support::with_trust_file(|_| {
        trust::approve(&dir);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let rec = Rec::new();
            let mut handle = ka_engine::spawn_with_speaker(
                cfg,
                test_catalog(),
                ka_engine::StrandChoice::New,
                ka_dialect::Wire::OpenaiChat,
                rec.clone(),
            );
            rec.arm(vec![("read", serde_json::json!({"path": "f.txt"}))]);
            let events = settle(&mut handle, prompt("read it"), on_idle).await;
            assert!(
                events.iter().any(|e| matches!(
                    e,
                    Event::CallOutput { is_error: true, excerpt, .. }
                        if excerpt.contains("blocked by hook")
                )),
                "hook blocks while enabled: {events:?}"
            );

            // toggle hooks off: the same call goes through
            settle(&mut handle, set_feature("hooks", false), on_idle).await;
            rec.arm(vec![("read", serde_json::json!({"path": "f.txt"}))]);
            let events = settle(&mut handle, prompt("read it again"), on_idle).await;
            assert!(
                !events.iter().any(|e| matches!(
                    e,
                    Event::CallOutput { is_error: true, excerpt, .. }
                        if excerpt.contains("blocked by hook")
                )),
                "hook silenced by toggle: {events:?}"
            );
            assert!(
                events.iter().any(|e| matches!(
                    e,
                    Event::CallOutput {
                        is_error: false,
                        ..
                    }
                )),
                "the read itself ran: {events:?}"
            );
        });
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// `/sandbox` swaps the live mode, records it on the strand, and
/// rejects unknown modes with a note.
#[tokio::test]
async fn sandbox_toggle_swaps_and_records() {
    let dir = tmp_dir("sandbox");
    let rec = Rec::new();
    let mut handle = engine(&dir, rec, ka_engine::StrandChoice::New);

    let events = settle(
        &mut handle,
        Command::SetSandbox { mode: "fs".into() },
        on_idle,
    )
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::FeaturesChanged { sandbox: Some(s), .. } if s == "fs"
        )),
        "fs announced: {events:?}"
    );
    // unknown mode: refused with a note, state untouched
    let events = settle(
        &mut handle,
        Command::SetSandbox {
            mode: "bogus".into(),
        },
        on_idle,
    )
    .await;
    assert!(
        events.iter().any(
            |e| matches!(e, Event::Note { message } if message.contains("unknown sandbox mode"))
        ),
        "bogus mode refused: {events:?}"
    );
    let events = settle(
        &mut handle,
        Command::SetSandbox { mode: "off".into() },
        on_idle,
    )
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::FeaturesChanged { sandbox: Some(s), .. } if s == "off"
        )),
        "off announced: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Enabling an MCP server disabled at startup attempts the spawn; the
/// toggle records regardless, and a failed spawn says so by name.
#[tokio::test]
async fn mcp_server_enable_attempts_spawn() {
    let dir = tmp_dir("mcp");
    let parsed = ka_engine::Config::parse_layer(
        "[[mcp]]\nname = \"ghost\"\ncommand = \"definitely-not-a-real-binary-ka-test\"\n\n[features]\ndisable = [\"mcp:ghost\"]\n",
        "project",
    )
    .unwrap();
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..parsed
    };
    let rec = Rec::new();
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        rec,
    );
    // bootstrap: ghost is listed but not spawned (ok = false), and the
    // disabled snapshot names it
    let mut bootstrap_disabled: Option<Vec<String>> = None;
    let mut bootstrap_mcp_ok: Option<bool> = None;
    while let Ok(Some(e)) = tokio::time::timeout(Duration::from_secs(5), handle.events.recv()).await
    {
        match &e {
            Event::Inventory { disabled, mcp, .. } => {
                bootstrap_disabled = Some(disabled.clone());
                bootstrap_mcp_ok = mcp.first().map(|s| s.ok);
            }
            Event::Idle | Event::Replay { .. } if bootstrap_disabled.is_some() => break,
            _ => {}
        }
    }
    assert_eq!(
        bootstrap_disabled,
        Some(vec!["mcp:ghost".to_string()]),
        "startup inventory names the disabled server"
    );
    assert_eq!(bootstrap_mcp_ok, Some(false), "ghost not spawned");

    // enable: the spawn is attempted and fails by name; the toggle lands
    let events = settle(&mut handle, set_feature("mcp:ghost", true), on_idle).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Error { message, .. } if message.contains("ghost")
        )),
        "spawn failure names the server: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::FeaturesChanged { disabled, .. } if disabled.is_empty()
        )),
        "toggle recorded: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
