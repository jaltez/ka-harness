//! Feature contracts: the documented, user-facing behavior of ka's
//! engine, encoded as tests so future changes cannot silently drop a
//! feature — a failing test here means the change was intentional and
//! the contract (and docs) must be updated with it.
//!
//! Contracts covered: the tool registry, the permission gate matrix
//! (modes × rules × hardstops × protected paths × read ledger), hook
//! blocking + steering, the verify lint loop, the config schema
//! surface, model selectors, glob/rule matching semantics, and the
//! background-delegate task lifecycle. Pure end-to-end engine commands
//! live in `engine_e2e.rs`; safe mode in `bare_mode_contract.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use ka_dialect::dialects::Catalog;
use ka_dialect::speaker::{SpeakFuture, SpeakRequest, Speaker, StreamEvent, ToolCall};
use ka_engine::hands::{self, Hand};
use ka_engine::voice::{GuardRuntime, Voice};
use ka_protocol::{Command, Event};

fn test_catalog() -> Catalog {
    // unreachable base_url: any accidental real-provider attempt fails fast
    Catalog::parse(
        "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
    )
    .unwrap()
}

/// A speaker that emits one scripted tool call per request round, then
/// a final text reply (the FakeSpeaker contract from voice's own tests).
struct Scripted {
    calls: parking_lot::Mutex<VecDeque<(&'static str, serde_json::Value)>>,
}

impl Scripted {
    fn calls(calls: Vec<(&'static str, serde_json::Value)>) -> Arc<dyn Speaker> {
        Arc::new(Scripted {
            calls: parking_lot::Mutex::new(calls.into()),
        })
    }
}

impl Speaker for Scripted {
    fn speak<'a>(
        &'a self,
        _req: SpeakRequest,
        out: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> SpeakFuture<'a> {
        Box::pin(async move {
            // bind outside the if-let: edition-2024 temporaries would
            // otherwise hold the parking_lot guard across the awaits
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

/// Drive one turn, auto-answering permission asks with `answer`
/// (1 = always, 2 = deny; None leaves asks unanswered — do not do that
/// in tests). Returns every event the turn emitted.
async fn run_turn(voice: &mut Voice, prompt: &str, answer: Option<usize>) -> Vec<Event> {
    // the engine points the snapshot journal at the strand on attach;
    // mirror that here so mutating hands behave as they do in a session
    voice.snapshot_sink().lock().set_strand("contract-test");
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<Command>(16);
    let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel::<Event>(256);
    let mut steers = Vec::new();
    let mut queue = VecDeque::new();
    let mut guards = GuardRuntime::default();
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
                    if let Event::Ask { id, .. } = &e {
                        if let Some(choice) = answer {
                            cmd_tx
                                .send(Command::Answer {
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
                // the turn finished; collect what is still buffered.
                // (A recv().await drain would deadlock: evt_tx lives in
                // this frame — turn only borrows it.)
                while let Ok(e) = evt_rx.try_recv() {
                    events.push(e);
                }
                break;
            }
        }
    }
    events
}

fn asks_of(events: &[Event]) -> Vec<&AskQuestion> {
    events
        .iter()
        .flat_map(|e| match e {
            Event::Ask { questions, .. } => questions.iter().collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

use ka_protocol::AskQuestion;

fn outputs_of<'a>(events: &'a [Event], tool: &str) -> Vec<&'a str> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::CallOutput {
                tool: t,
                excerpt,
                is_error,
                ..
            } if t == tool => Some((&**excerpt, *is_error)),
            _ => None,
        })
        .map(|(e, _)| e)
        .collect()
}

/// Gate-phase refusals (loop guard, loop rule) surface as CallFinished
/// ok:false with NO CallOutput — the message rides the tool result back
/// to the model, not the event stream.
fn refused_calls(events: &[Event], tool: &str) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, Event::CallFinished { tool: t, ok: false, .. } if t == tool))
        .count()
}

fn errored_outputs<'a>(events: &'a [Event], tool: &str) -> Vec<&'a str> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::CallOutput {
                tool: t,
                excerpt,
                is_error,
                ..
            } if t == tool && *is_error => Some(&**excerpt),
            _ => None,
        })
        .collect()
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-contract-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An fs policy the way production builds one: the bash hand streams
/// every command's output through the state dir (`Spill::new`), so
/// `allowlist_for` always keeps it writable — a policy without it
/// cannot even start a command under bubblewrap/firejail. The test
/// adds only that dir (not /tmp or the cache) so out-of-allowlist
/// writes under the temp dir still earn a grant ask.
fn fs_policy_for(allow: &[std::path::PathBuf]) -> ka_sandbox::Policy {
    let state = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })
        .ok();
    let mut dirs = allow.to_vec();
    if let Some(s) = state {
        if !dirs.contains(&s) {
            dirs.push(s);
        }
    }
    ka_sandbox::Policy::fs(dirs)
}

// ---------------------------------------------------------------- registry

/// The documented built-in tool set: names, clearance tiers, read-only
/// flags. Changing these changes the model-facing surface — deliberate
/// changes must update this test and the docs together.
#[test]
fn registry_core_tools_contract() {
    let registry = hands::registry_with_pathfinder(
        Arc::new(parking_lot::RwLock::new(
            hands::pathfinder::PathfinderSource::default(),
        )),
        ka_engine::hands::todo::slot(),
        Arc::new(hands::jobs::JobTable::new()),
    );
    let mut names: Vec<String> = registry.iter().map(|h| h.def().name).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "bash",
            "edit",
            "glob",
            "grep",
            "jobs",
            "pathfinder",
            "read",
            "todo",
            "write"
        ],
        "the built-in tool registry changed — update docs + this contract"
    );
    let by_name = |n: &str| {
        registry
            .iter()
            .find(|h| h.def().name == n)
            .unwrap_or_else(|| panic!("missing hand {n}"))
            .def()
    };
    assert_eq!(by_name("read").clearance, hands::Clearance::Read);
    assert_eq!(by_name("glob").clearance, hands::Clearance::Read);
    assert_eq!(by_name("grep").clearance, hands::Clearance::Read);
    assert_eq!(by_name("pathfinder").clearance, hands::Clearance::Read);
    assert_eq!(by_name("todo").clearance, hands::Clearance::Read);
    assert!(by_name("todo").read_only, "todo must auto-allow");
    assert_eq!(by_name("edit").clearance, hands::Clearance::Write);
    assert_eq!(by_name("write").clearance, hands::Clearance::Write);
    assert_eq!(by_name("bash").clearance, hands::Clearance::Exec);
    assert!(
        !by_name("bash").read_only,
        "bash is the exec primitive; it must never be marked read-only"
    );
}

/// Conditional hands exist with the documented names/tiers: web tools,
/// LSP navigation, MCP call-front, remember, tasks, delegate.
#[test]
fn conditional_hands_contract() {
    // web: fetch always, search only with a provider
    let fetch_only = hands::web::hands(None, false);
    assert_eq!(
        fetch_only.iter().map(|h| h.def().name).collect::<Vec<_>>(),
        vec!["web_fetch"]
    );
    let both = hands::web::hands(
        Some(ka_engine::config::SearchProvider {
            provider: "tavily".into(),
            api_key_env: "K".into(),
            base_url: None,
        }),
        false,
    );
    assert_eq!(
        both.iter().map(|h| h.def().name).collect::<Vec<_>>(),
        vec!["web_search", "web_fetch"]
    );

    // LSP navigation quad
    let lsp = hands::lsp_tools::hands(Arc::new(ka_engine::lsp::LspManager::new(
        Path::new("/tmp"),
        &ka_engine::config::Lsp {
            enable: Some(true),
            commands: None,
            write_through: None,
        },
    )));
    let mut lsp_names: Vec<String> = lsp.iter().map(|h| h.def().name).collect();
    lsp_names.sort();
    assert_eq!(
        lsp_names,
        vec!["definition", "diagnostics", "references", "symbols"]
    );
    for h in &lsp {
        let d = h.def();
        assert_eq!(
            d.clearance,
            hands::Clearance::Read,
            "LSP hands are read-only"
        );
        assert!(d.read_only);
    }

    // LSP write-through pair ([lsp] write_through = true only)
    let lsp_w = hands::lsp_write::hands(Arc::new(ka_engine::lsp::LspManager::new(
        Path::new("/tmp"),
        &ka_engine::config::Lsp {
            enable: Some(true),
            commands: None,
            write_through: Some(true),
        },
    )));
    let mut lsp_w_names: Vec<String> = lsp_w.iter().map(|h| h.def().name).collect();
    lsp_w_names.sort();
    assert_eq!(lsp_w_names, vec!["lsp_actions", "lsp_format", "lsp_rename"]);
    let rename = lsp_w.iter().find(|h| h.def().name == "lsp_rename").unwrap();
    assert_eq!(rename.def().clearance, hands::Clearance::Write);
    assert!(!rename.def().read_only);
    let format = lsp_w.iter().find(|h| h.def().name == "lsp_format").unwrap();
    assert_eq!(
        format.def().clearance,
        hands::Clearance::Write,
        "formatting rewrites the file"
    );
    assert!(!format.def().read_only);
    let actions = lsp_w
        .iter()
        .find(|h| h.def().name == "lsp_actions")
        .unwrap();
    let d = actions.def();
    assert_eq!(d.clearance, hands::Clearance::Read, "static tier = listing");
    assert!(!d.read_only);
    assert_eq!(
        actions.clearance_for(&serde_json::json!({"file": "a.rs", "line": 1})),
        hands::Clearance::Read,
        "listing reads"
    );
    assert_eq!(
        actions.clearance_for(&serde_json::json!({
            "file": "a.rs", "line": 1, "mode": "run", "pick": 1
        })),
        hands::Clearance::Write,
        "running a code action mutates"
    );

    // the engine's registration decision: write-through joins only under
    // write_through = true; the navigation set is identical either way
    let off = ka_engine::config::Lsp {
        enable: Some(true),
        commands: None,
        write_through: None,
    };
    let on = ka_engine::config::Lsp {
        enable: Some(true),
        commands: None,
        write_through: Some(true),
    };
    let nav = ka_engine::lsp_hands(
        Arc::new(ka_engine::lsp::LspManager::new(Path::new("/tmp"), &off)),
        &off,
    );
    let both = ka_engine::lsp_hands(
        Arc::new(ka_engine::lsp::LspManager::new(Path::new("/tmp"), &on)),
        &on,
    );
    assert_eq!(
        nav.len() + 3,
        both.len(),
        "write-through adds exactly three hands"
    );
    assert!(
        nav.iter()
            .all(|h| h.def().name != "lsp_rename" && h.def().name != "lsp_actions")
    );
    assert!(both.iter().any(|h| h.def().name == "lsp_rename"));
    assert!(both.iter().any(|h| h.def().name == "lsp_actions"));
    assert!(both.iter().any(|h| h.def().name == "lsp_format"));

    // the debug probe ([debug] enable = true only): control flow runs at
    // Exec, inspection reads, unknown actions fail closed at Exec
    let dbg = hands::debug::DebugHand::new(Arc::new(ka_engine::dap::DebugManager::new(None, None)));
    let d = dbg.def();
    assert_eq!(d.name, "debug");
    assert_eq!(d.clearance, hands::Clearance::Exec);
    assert!(!d.read_only);
    for action in [
        "start",
        "break",
        "clear",
        "continue",
        "next",
        "step_in",
        "step_out",
        "pause",
        "disconnect",
    ] {
        assert_eq!(
            dbg.clearance_for(&serde_json::json!({"action": action})),
            hands::Clearance::Exec,
            "{action} drives the debuggee"
        );
    }
    for action in [
        "sessions", "breaks", "threads", "stack", "vars", "eval", "output",
    ] {
        assert_eq!(
            dbg.clearance_for(&serde_json::json!({"action": action})),
            hands::Clearance::Read,
            "{action} only inspects"
        );
    }
    assert_eq!(
        dbg.clearance_for(&serde_json::json!({"action": "wat"})),
        hands::Clearance::Exec,
        "unknown actions fail closed"
    );

    // the tasks hand: listing/reading are Read; steering a running
    // agent and merging its branch mutate (Write); cancel stays Read
    let tasks_hand = hands::tasks::TasksHand::new(ka_engine::hands::tasks::AgentTaskTable::new());
    assert_eq!(tasks_hand.def().name, "tasks");
    assert_eq!(tasks_hand.def().clearance, hands::Clearance::Read);
    assert!(!tasks_hand.def().read_only, "send/merge mutate");
    assert_eq!(
        tasks_hand.clearance_for(&serde_json::json!({"action": "send"})),
        hands::Clearance::Write
    );
    assert_eq!(
        tasks_hand.clearance_for(&serde_json::json!({"action": "merge"})),
        hands::Clearance::Write
    );
    assert_eq!(
        tasks_hand.clearance_for(&serde_json::json!({"action": "list"})),
        hands::Clearance::Read
    );

    // MCP lazy front-hand
    let mcp = ka_engine::mcp::McpCallHand::new(
        Vec::new(),
        ka_engine::features::FeatureToggles::slot(Vec::new()),
    );
    let d = mcp.def();
    assert_eq!(d.name, "mcp_call");
    assert_eq!(d.clearance, hands::Clearance::Exec);

    // memory inbox + tasks
    let d = hands::memory::RememberHand.def();
    assert_eq!(d.name, "remember");
    assert_eq!(d.clearance, hands::Clearance::Write);
    let d = hands::tasks::TasksHand::new(hands::tasks::AgentTaskTable::new()).def();
    assert_eq!(d.name, "tasks");
    assert_eq!(d.clearance, hands::Clearance::Read);

    // delegate
    let (evt_tx, _evt_rx) = tokio::sync::mpsc::channel(16);
    let d = hands::delegate::DelegateHand::new(
        vec![ka_engine::agents::AgentDef {
            name: "a".into(),
            description: String::new(),
            system: "s".into(),
            max_steps: 4,
            isolate: false,
            model: None,
            effort: None,
            tools: None,
            output: None,
        }],
        Arc::new(parking_lot::RwLock::new(
            hands::pathfinder::PathfinderSource::default(),
        )),
        ka_protocol::Mode::Free,
        hands::tasks::AgentTaskTable::new(),
        evt_tx,
        ka_engine::features::FeatureToggles::slot(Vec::new()),
    )
    .def();
    assert_eq!(d.name, "delegate");
    assert_eq!(d.clearance, hands::Clearance::Read);
    assert!(
        d.description.contains("background: true"),
        "delegate must advertise the background escape hatch"
    );
}

// ------------------------------------------------------------ config/schema

/// Every documented config key must survive `ka config schema` — a key
/// disappearing from the schema is a breaking config change.
#[test]
fn config_schema_carries_every_documented_key() {
    let schema = ka_engine::config::Config::schema_json().unwrap();
    for key in [
        "model",
        "effort",
        "mode",
        "max_steps",
        "cwd",
        "rules",
        "hooks",
        "mcp",
        "permissions",
        "allow",
        "guards",
        "spend_usd",
        "context_pct",
        "fallback",
        "models",
        "update",
        "repo",
        "check",
        "context",
        "promote",
        "search",
        "provider",
        "api_key_env",
        "base_url",
        "sandbox",
        "lsp",
        "enable",
        "commands",
        "write_through",
        "debug",
        "enable",
        "adapters",
        "tui",
        "header_glyph",
        "bell",
        "notify",
        "tools",
        "bash",
        "background_after_ms",
        "read",
        "max_image_mb",
        "web",
        "allow_private_hosts",
        "discovery",
        "git",
        "auto_commit",
        "verify",
        "test",
        "lints",
        "pattern",
        "command",
        "roles",
        "default",
        "fast",
        // Phase 9 (2026-09-29): sandbox expansion, auto-review, loop rule
        "allow_write",
        "auto_review",
        // Phase 10: runtime feature toggles
        "features",
        "disable",
    ] {
        assert!(
            schema.contains(&format!("\"{key}\"")),
            "config schema lost documented key {key:?}"
        );
    }
    // the schema's hook-event enum carries every documented event
    for event in [
        "pre_tool_use",
        "post_tool_use",
        "stop",
        "turn_end",
        "session_start",
        "session_end",
        "user_prompt_submit",
        "pre_compact",
    ] {
        assert!(
            schema.contains(&format!("\"{event}\"")),
            "config schema lost documented hook event {event:?}"
        );
    }
}

/// Selector grammar: `vendor/model@effort`, colon-bearing local model
/// ids, effort-only override shape.
#[test]
fn selectors_parse_vendor_model_effort_and_colons() {
    let s = ka_dialect::parse_selector("anthropic/claude-sonnet-5@high").unwrap();
    assert_eq!(s.vendor, "anthropic");
    assert_eq!(s.model, "claude-sonnet-5");
    assert_eq!(s.effort.as_deref(), Some("high"));
    assert_eq!(s.model_id(), "anthropic/claude-sonnet-5");

    let s = ka_dialect::parse_selector("ollama/qwen3.5:9b").unwrap();
    assert_eq!(s.vendor, "ollama");
    assert_eq!(s.model, "qwen3.5:9b", "colons in model ids must survive");
    assert_eq!(s.model_id(), "ollama/qwen3.5:9b");

    // @ splits on the LAST occurrence
    let s = ka_dialect::parse_selector("vendor/m@o@high").unwrap();
    assert_eq!(s.model, "m@o");
    assert_eq!(s.effort.as_deref(), Some("high"));

    assert!(ka_dialect::parse_selector("no-slash").is_err());
}

/// ka glob semantics (rules, lint patterns, scoped rules): `*` crosses
/// directory separators, `?` matches one char.
#[test]
fn glob_semantics_star_crosses_directories() {
    assert!(ka_engine::voice::glob_match("*.rs", "src/deep/a.rs"));
    assert!(ka_engine::voice::glob_match("**/*.ts", "src/x/a.ts"));
    assert!(ka_engine::voice::glob_match(
        "cargo *",
        "cargo build --release"
    ));
    assert!(!ka_engine::voice::glob_match("cargo *", "rustc build"));
    assert!(ka_engine::voice::glob_match("a?c", "abc"));
    assert!(!ka_engine::voice::glob_match("a?c", "ac"));
}

/// Web-tool rules match hosts with domain semantics; other tools use
/// plain globs on the primary argument.
#[test]
fn web_domain_rule_semantics() {
    use ka_engine::voice::rule_pattern_matches;
    let url = "https://api.example.com/v1/x?y=1";
    assert!(
        rule_pattern_matches("example.com", "web_fetch", url),
        "apex covers subdomains"
    );
    assert!(rule_pattern_matches("*.example.com", "web_fetch", url));
    assert!(
        !rule_pattern_matches("*.example.com", "web_fetch", "https://example.com/"),
        "star rule is subdomains only"
    );
    assert!(
        !rule_pattern_matches("example.com", "web_fetch", "https://notexample.com/"),
        "suffix must be domain-bound"
    );
    assert!(rule_pattern_matches(
        "example.com",
        "web_search",
        "https://example.com"
    ));
    // non-web tools keep plain glob semantics
    assert!(rule_pattern_matches("cargo *", "bash", "cargo test"));
    assert!(!rule_pattern_matches("example.com", "bash", "anything"));
}

// ------------------------------------------------------------ gate matrix

/// free mode: writes run without an ask (the documented default tier).
#[tokio::test]
async fn free_mode_writes_without_ask() {
    let dir = tmp_dir("free-write");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "write",
                serde_json::json!({"path": "new.txt", "content": "hello"}),
            )]),
        );
    let events = run_turn(&mut voice, "make a file", None).await;
    assert!(
        asks_of(&events).is_empty(),
        "free mode must not ask for writes: {events:?}"
    );
    assert_eq!(errored_outputs(&events, "write").len(), 0, "{events:?}");
    assert!(
        dir.join("new.txt").exists(),
        "write hand executed: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// guarded mode: writes ask; a deny answer blocks the call without
/// executing it.
#[tokio::test]
async fn guarded_write_asks_and_deny_blocks() {
    let dir = tmp_dir("guarded-write");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Guarded, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "write",
                serde_json::json!({"path": "guarded.txt", "content": "x"}),
            )]),
        );
    let events = run_turn(&mut voice, "make a file", Some(2)).await;
    let asks = asks_of(&events);
    assert_eq!(asks.len(), 1, "guarded write must ask once: {events:?}");
    assert!(asks[0].text.contains("modify files"), "{}", asks[0].text);
    let errs = errored_outputs(&events, "write");
    assert!(
        errs.iter().any(|e| e.contains("permission denied by user")),
        "{errs:?}"
    );
    assert!(
        !dir.join("guarded.txt").exists(),
        "denied write must not execute"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Hardstops are unbypassable-by-mode: they ask even in free mode, and
/// a deny answer keeps the catastrophic command from ever running.
#[tokio::test]
async fn hardstop_asks_even_in_free() {
    let dir = tmp_dir("hardstop");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("bash", serde_json::json!({"command": "rm -rf /"}))]),
        );
    let events = run_turn(&mut voice, "clean the disk", Some(2)).await;
    let asks = asks_of(&events);
    assert_eq!(asks.len(), 1, "hardstop must ask in free mode: {events:?}");
    assert!(asks[0].text.contains("HARDSTOP"), "{}", asks[0].text);
    assert!(
        asks[0].text.contains("recursive delete"),
        "{}",
        asks[0].text
    );
    let errs = errored_outputs(&events, "bash");
    assert!(
        errs.iter().any(|e| e.contains("permission denied by user")),
        "{errs:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Protected paths ask even in free mode, even with an allow rule and
/// a session allowlist — the injected-prompt defense.
#[tokio::test]
async fn protected_path_asks_even_with_allow_rule() {
    let dir = tmp_dir("protected-ask");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "write",
                serde_json::json!({"path": ".git/hooks/pre-commit", "content": "#!/bin/sh"}),
            )]),
        );
    voice.set_rules(vec![ka_engine::config::Rule {
        tool: "write".into(),
        pattern: None,
        verdict: ka_engine::config::Verdict::Allow,
    }]);
    voice.set_allowed_tools(vec!["write".into()]);
    let events = run_turn(&mut voice, "add a hook", Some(2)).await;
    let asks = asks_of(&events);
    assert_eq!(asks.len(), 1, "protected path must ask: {events:?}");
    assert!(asks[0].text.contains("PROTECTED"), "{}", asks[0].text);
    assert!(asks[0].text.contains("git internals"), "{}", asks[0].text);
    assert!(
        !dir.join(".git/hooks/pre-commit").exists(),
        "denied protected write must not execute"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Plan mode matrix: writes inside .ka/plans/ run; everything else
/// denies with the documented message; exec asks.
#[tokio::test]
async fn plan_mode_matrix() {
    let dir = tmp_dir("plan-mode");
    std::fs::create_dir_all(dir.join(".ka/plans")).unwrap();

    // allowed: the plans directory
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Plan, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "write",
                serde_json::json!({"path": ".ka/plans/plan.md", "content": "# plan"}),
            )]),
        );
    let events = run_turn(&mut voice, "draft the plan", None).await;
    assert!(asks_of(&events).is_empty(), "{events:?}");
    assert_eq!(errored_outputs(&events, "write").len(), 0);
    assert!(dir.join(".ka/plans/plan.md").exists());
    drop(voice);

    // denied: anything outside the plans directory
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Plan, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "write",
                serde_json::json!({"path": "src/main.rs", "content": "x"}),
            )]),
        );
    let events = run_turn(&mut voice, "implement", None).await;
    let errs = errored_outputs(&events, "write");
    assert!(
        errs.iter().any(|e| e.contains("plan mode is read-only")),
        "{errs:?}"
    );
    assert!(!dir.join("src/main.rs").exists());
    drop(voice);

    // exec: read-only commands auto-allow (research), everything else asks
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Plan, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("bash", serde_json::json!({"command": "ls"}))]),
        );
    let events = run_turn(&mut voice, "look around", None).await;
    assert!(
        asks_of(&events).is_empty(),
        "read-only exec must auto-allow in plan mode: {events:?}"
    );
    drop(voice);
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Plan, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "bash",
                serde_json::json!({"command": "touch probe.txt"}),
            )]),
        );
    let events = run_turn(&mut voice, "make a file", Some(2)).await;
    let asks = asks_of(&events);
    assert_eq!(
        asks.len(),
        1,
        "non-readonly plan-mode exec must ask: {events:?}"
    );
    assert!(asks[0].text.contains("plan mode"), "{}", asks[0].text);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Rules: first match wins, before mode logic; verdicts allow/ask/deny.
#[tokio::test]
async fn rules_first_match_allow_ask_deny() {
    let dir = tmp_dir("rules");
    // allow rule: `echo *` runs ungated even in guarded mode
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Guarded, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "bash",
                serde_json::json!({"command": "echo rule-allow"}),
            )]),
        );
    voice.set_rules(vec![ka_engine::config::Rule {
        tool: "bash".into(),
        pattern: Some("echo *".into()),
        verdict: ka_engine::config::Verdict::Allow,
    }]);
    let events = run_turn(&mut voice, "greet", None).await;
    assert!(
        asks_of(&events).is_empty(),
        "allow rule must skip the ask: {events:?}"
    );
    assert!(
        outputs_of(&events, "bash")
            .iter()
            .any(|o| o.contains("rule-allow")),
        "{events:?}"
    );
    drop(voice);

    // deny rule: `git *` refuses without asking
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Guarded, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("bash", serde_json::json!({"command": "git status"}))]),
        );
    voice.set_rules(vec![ka_engine::config::Rule {
        tool: "bash".into(),
        pattern: Some("git *".into()),
        verdict: ka_engine::config::Verdict::Deny,
    }]);
    let events = run_turn(&mut voice, "check git", None).await;
    assert!(asks_of(&events).is_empty(), "deny must not ask: {events:?}");
    assert!(
        errored_outputs(&events, "bash")
            .iter()
            .any(|e| e.contains("denied by rule")),
        "{events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Session "always allow": answering an ask with choice 1 auto-allows
/// every later call of that tool for the session.
#[tokio::test]
async fn session_always_allow_skips_later_asks() {
    let dir = tmp_dir("always-allow");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Guarded, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![
                (
                    "write",
                    serde_json::json!({"path": "a.txt", "content": "1"}),
                ),
                (
                    "write",
                    serde_json::json!({"path": "b.txt", "content": "2"}),
                ),
            ]),
        );
    let events = run_turn(&mut voice, "two files", Some(1)).await;
    assert_eq!(
        asks_of(&events).len(),
        1,
        "exactly one ask: the second write rides the always-allow: {events:?}"
    );
    assert!(dir.join("a.txt").exists());
    assert!(dir.join("b.txt").exists());
    assert!(
        dir.join(".ka/ka.toml").exists(),
        "always-allow persists to the project layer"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The read ledger: edits refuse files that were never read, and files
/// changed since the read; read-then-edit succeeds.
#[tokio::test]
async fn read_ledger_contract() {
    let dir = tmp_dir("ledger");
    std::fs::write(dir.join("f.txt"), "alpha beta").unwrap();

    // unread edit refuses
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "edit",
                serde_json::json!({"path": "f.txt", "old": "alpha", "new": "ALPHA"}),
            )]),
        );
    let events = run_turn(&mut voice, "edit it", None).await;
    assert!(
        errored_outputs(&events, "edit")
            .iter()
            .any(|e| e.contains("has not been read yet")),
        "{events:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "alpha beta"
    );
    drop(voice);

    // read-then-edit succeeds
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![
                ("read", serde_json::json!({"path": "f.txt"})),
                (
                    "edit",
                    serde_json::json!({"path": "f.txt", "old": "alpha", "new": "ALPHA"}),
                ),
            ]),
        );
    let events = run_turn(&mut voice, "edit it", None).await;
    assert_eq!(errored_outputs(&events, "edit").len(), 0, "{events:?}");
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "ALPHA beta"
    );

    // changed-since-read refuses: the mutation lands between turns, the
    // ledger keeps the old stamp, and the next-turn edit is refused
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("read", serde_json::json!({"path": "f.txt"}))]),
        );
    let _ = run_turn(&mut voice, "read it", None).await;
    std::fs::write(dir.join("f.txt"), "CHANGED behind the read").unwrap();
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "edit",
                serde_json::json!({"path": "f.txt", "old": "CHANGED", "new": "EDITED"}),
            )]),
        );
    let events = run_turn(&mut voice, "edit it", None).await;
    assert!(
        errored_outputs(&events, "edit")
            .iter()
            .any(|e| e.contains("has not been read yet")),
        "a fresh voice has an empty ledger: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- hooks

/// Config hooks: pre_tool_use exit 2 blocks the call with the hook's
/// stderr as the reason — the ecosystem-compatible contract.
#[tokio::test]
async fn hook_exit2_blocks_pre_tool_use() {
    let dir = tmp_dir("hook-block");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("bash", serde_json::json!({"command": "echo hi"}))]),
        );
    voice.set_hooks(vec![ka_engine::config::Hook {
        event: ka_engine::config::HookEvent::PreToolUse,
        tool: Some("bash".into()),
        command: "echo no-way >&2; exit 2".into(),
    }]);
    let events = run_turn(&mut voice, "run it", None).await;
    assert!(
        errored_outputs(&events, "bash")
            .iter()
            .any(|e| e.contains("blocked by hook") && e.contains("no-way")),
        "{events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Hook steering: a clean-exit pre_tool_use hook emitting
/// `{"mode":"plan","note":"…"}` switches the permission mode mid-turn.
#[tokio::test]
async fn hook_stdout_steers_mode() {
    let dir = tmp_dir("hook-steer");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("bash", serde_json::json!({"command": "echo hi"}))]),
        );
    voice.set_hooks(vec![ka_engine::config::Hook {
        event: ka_engine::config::HookEvent::PreToolUse,
        tool: None,
        command: "printf '{\"mode\":\"plan\",\"note\":\"steered\"}'".into(),
    }]);
    let events = run_turn(&mut voice, "run it", None).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::ModeChanged {
                mode: ka_protocol::Mode::Plan
            }
        )),
        "steering must switch the mode: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("steered"))),
        "the steering note surfaces: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `post_tool_use` hook's `{"feedback": ...}` rides the tool result
/// the model reads — informational, never an error, capped — the
/// generalized [verify]-lint channel for any tool and any script.
#[tokio::test]
async fn hook_feedback_rides_the_tool_result() {
    let dir = tmp_dir("hook-feedback");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("bash", serde_json::json!({"command": "echo hi"}))]),
        );
    voice.set_hooks(vec![ka_engine::config::Hook {
        event: ka_engine::config::HookEvent::PostToolUse,
        tool: Some("bash".into()),
        command: r#"printf '{"feedback":"shellcheck: 2 findings (SC2086)"}'"#.into(),
    }]);
    let events = run_turn(&mut voice, "run it", None).await;
    // the model-facing result carries the feedback block…
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::CallOutput { excerpt, is_error: false, .. }
                if excerpt.contains("[hook feedback]")
                    && excerpt.contains("shellcheck: 2 findings")
        )),
        "feedback must ride the result, not an error: {events:?}"
    );
    // …and the call itself still finished clean
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::CallFinished { tool, ok: true, .. } if tool == "bash")),
        "the poked call stays a success: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- verify

/// [verify] lints ride successful edit results as informational
/// context (never errors), and the edited-file list is tracked for the
/// test loop.
#[tokio::test]
async fn verify_lint_rides_edit_result() {
    let dir = tmp_dir("verify-lint");
    std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![
                ("read", serde_json::json!({"path": "a.rs"})),
                ("edit", serde_json::json!({"path": "a.rs", "old": "fn main() {}", "new": "fn main() -> () {}"})),
            ]),
        );
    voice.set_verify(ka_engine::config::Verify {
        test: None,
        lints: vec![ka_engine::config::LintRule {
            pattern: "*.rs".into(),
            command: "echo LINT-FAIL >&2; exit 3 {file}".into(),
        }],
    });
    let events = run_turn(&mut voice, "refactor", None).await;
    let edit_outputs = outputs_of(&events, "edit");
    assert!(
        edit_outputs
            .iter()
            .any(|o| o.contains("<lint note") && o.contains("LINT-FAIL") && o.contains("exit 3")),
        "lint block must ride the edit result: {edit_outputs:?}"
    );
    // the tool result itself stays non-error
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::CallOutput { tool, is_error: true, .. } if tool == "edit")),
        "lint must never flip the tool error flag: {events:?}"
    );
    assert_eq!(voice.take_edited(), vec!["a.rs".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------- background delegates

/// delegate {background: true} returns immediately, registers a task,
/// and the runner's outcome lands in the table (here: a fast provider
/// failure, since the scripted speaker cannot reach the nested voice).
#[tokio::test]
async fn background_delegate_registers_and_reports() {
    let dir = tmp_dir("bg-delegate");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "delegate",
                serde_json::json!({
                    "agent": "scout",
                    "task": "look around",
                    "background": true
                }),
            )]),
        );
    let table = hands::tasks::AgentTaskTable::new();
    let (note_tx, _note_rx) = tokio::sync::mpsc::channel(16);
    {
        let slot = voice.pathfinder_slot();
        slot.write().catalog = test_catalog();
        slot.write().model = Some("test/m".into());
    }
    voice.push_hand(Arc::new(hands::delegate::DelegateHand::new(
        vec![ka_engine::agents::AgentDef {
            name: "scout".into(),
            description: "explores".into(),
            system: "you scout".into(),
            max_steps: 4,
            isolate: false,
            model: None,
            effort: None,
            tools: None,
            output: None,
        }],
        voice.pathfinder_slot(),
        ka_protocol::Mode::Free,
        table.clone(),
        note_tx,
        voice.feature_slot(),
    )));
    let events = run_turn(&mut voice, "delegate it", None).await;
    let outs = outputs_of(&events, "delegate");
    assert!(
        outs.iter()
            .any(|o| o.contains("background task t-1 started")),
        "background delegate returns immediately: {outs:?}"
    );
    // the task is visible and running; cancel really aborts the runner
    // (the nested voice retries provider errors, so the deterministic
    // lifecycle check is cancel — finish() is unit-tested in tasks.rs)
    let live = table.result(1).expect("task registered");
    assert!(live.contains("t-1") && live.contains("running"), "{live:?}");
    let cancelled = {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<Command>(16);
        let (evt_tx, evt_rx) = tokio::sync::mpsc::channel::<Event>(64);
        let tasks_hand = hands::tasks::TasksHand::new(table.clone());
        let out = tasks_hand
            .execute(
                &serde_json::json!({"action": "cancel", "id": 1}),
                &ka_engine::hands::HandContext {
                    cwd: dir.clone(),
                    ledger: Arc::new(parking_lot::Mutex::new(ka_engine::hands::Ledger::default())),
                    spill: Arc::new(ka_engine::hands::Spill::new()),
                    snapshots: Arc::new(parking_lot::Mutex::new(
                        ka_engine::hands::snapshots::Snapshots::inert(),
                    )),
                    jobs: Arc::new(ka_engine::hands::jobs::JobTable::new()),
                    bash_background_ms: 0,
                    max_image_mb: 5,
                    web_allow_private: false,
                    sandbox: ka_sandbox::Policy::Off,
                },
            )
            .await;
        drop((cmd_tx, cmd_rx, evt_tx, evt_rx));
        out
    };
    assert!(
        !cancelled.is_error && cancelled.content.contains("t-1 cancelled"),
        "{}",
        cancelled.content
    );
    assert!(
        table.result(1).unwrap().contains("cancelled"),
        "cancel sticks: {:?}",
        table.result(1)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- secrets

/// One-way secret redaction: values disappear, names stay, longest
/// value wins, non-secrets untouched.
#[test]
fn secrets_redact_pairs_contract() {
    let pairs = vec![
        (
            "ANTHROPIC_API_KEY".to_string(),
            "sk-ant-super-secret".to_string(),
        ),
        ("SHORT".to_string(), "abc".to_string()),
    ];
    let text = "key is sk-ant-super-secret and abc and normal text";
    let out = ka_engine::hands::secrets::redact_from(text, &pairs);
    assert!(!out.contains("sk-ant-super-secret"), "{out}");
    assert!(!out.contains("abc"), "{out}");
    assert!(
        out.contains("ANTHROPIC_API_KEY") || out.contains("[REDACTED"),
        "{out}"
    );
    assert!(out.contains("normal text"), "{out}");
    assert_eq!(
        ka_engine::hands::secrets::redact_from("nothing secret here", &pairs),
        "nothing secret here"
    );
}

// ------------------------------------------------------------- snapshots

/// Snapshot/undo: the write hand parks pre-mutation bytes; undo
/// restores the previous content; creation-undos delete.
#[tokio::test]
async fn snapshot_undo_contract() {
    let dir = tmp_dir("snapshots");
    std::fs::write(dir.join("existing.txt"), "original").unwrap();
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![
                ("read", serde_json::json!({"path": "existing.txt"})),
                ("edit", serde_json::json!({"path": "existing.txt", "old": "original", "new": "REWRITTEN"})),
                ("write", serde_json::json!({"path": "created.txt", "content": "fresh"})),
            ]),
        );
    let events = run_turn(&mut voice, "mutate", None).await;
    assert_eq!(errored_outputs(&events, "edit").len(), 0, "{events:?}");
    assert_eq!(
        std::fs::read_to_string(dir.join("existing.txt")).unwrap(),
        "REWRITTEN"
    );
    assert!(dir.join("created.txt").exists());

    // undo 1: the creation is removed
    let sink = voice.snapshot_sink();
    sink.lock().undo().unwrap();
    assert!(
        !dir.join("created.txt").exists(),
        "creation-undo must delete the file"
    );
    // undo 2: the edit is restored
    sink.lock().undo().unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("existing.txt")).unwrap(),
        "original",
        "edit-undo must restore the pre-mutation bytes"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------ Phase 9 (2026-09-29)

use ka_engine::config::{Hook, HookEvent as CfgHookEvent, Rule, Verdict};

/// Sandbox expansion (roadmap 9.1): a sandboxed bash command whose
/// redirect target sits outside the write allowlist earns exactly one
/// "sandbox grant" ask — allow runs it with the expanded per-call
/// policy, deny keeps the sandbox unchanged (the write fails), always
/// persists the granted write paths to the project `[sandbox]
/// allow_write` layer.
#[tokio::test]
async fn sandbox_expansion_grant_ask_allow_deny_always() {
    let dir = tmp_dir("sb-expand");
    let inside = dir.join("inside");
    let outside = dir.join("outside");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let target = outside.join("out.txt");
    let command = format!("echo granted > {}", target.display());

    // allow (this run): one grant-only ask, then execution with the
    // grant riding exactly this call
    let mut voice = Voice::new(test_catalog(), inside.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("bash", serde_json::json!({"command": command}))]),
        );
    voice.set_sandbox(fs_policy_for(std::slice::from_ref(&inside)));
    let events = run_turn(&mut voice, "go", Some(0)).await;
    let asks = asks_of(&events);
    assert_eq!(asks.len(), 1, "grant-only ask fires once in free mode");
    assert!(asks[0].text.contains("sandbox grant"), "{:?}", asks[0].text);
    assert!(
        asks[0].text.contains(&outside.display().to_string()),
        "the ask names the granted write path: {:?}",
        asks[0].text
    );
    // the grant really rides the call on hosts with an fs backend that
    // can execute it (landlock's trampoline points at this test binary,
    // so its execution proof is the sandbox crate's argv contracts)
    if matches!(
        ka_sandbox::detect_tool(),
        Some(ka_sandbox::Tool::Bubblewrap) | Some(ka_sandbox::Tool::Firejail)
    ) {
        let text = std::fs::read_to_string(&target).unwrap_or_default();
        assert!(text.contains("granted"), "allow must run the granted write");
    }
    // the granted write landed (or never ran) — clear it so the deny
    // case below starts from a clean slate
    let _ = std::fs::remove_file(&target);
    // a command inside the allowlist never asks
    let mut voice = Voice::new(test_catalog(), inside.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "bash",
                serde_json::json!({"command": format!("echo ok > {}", inside.join("in.txt").display())}),
            )]),
        );
    voice.set_sandbox(fs_policy_for(std::slice::from_ref(&inside)));
    let events = run_turn(&mut voice, "go", Some(0)).await;
    assert!(
        asks_of(&events).is_empty(),
        "no grant ask for in-allowlist writes"
    );

    // deny: the sandbox stays unchanged — the write cannot land
    let mut voice = Voice::new(test_catalog(), inside.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("bash", serde_json::json!({"command": command}))]),
        );
    voice.set_sandbox(fs_policy_for(std::slice::from_ref(&inside)));
    let events = run_turn(&mut voice, "go", Some(2)).await;
    assert_eq!(asks_of(&events).len(), 1);
    assert!(
        !target.exists(),
        "deny keeps the sandbox unchanged — the write must not land"
    );

    // always: the write paths persist to the project layer
    let mut voice = Voice::new(test_catalog(), inside.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("bash", serde_json::json!({"command": command}))]),
        );
    voice.set_sandbox(fs_policy_for(std::slice::from_ref(&inside)));
    let events = run_turn(&mut voice, "go", Some(1)).await;
    assert_eq!(asks_of(&events).len(), 1);
    let saved = std::fs::read_to_string(inside.join(".ka/ka.toml")).unwrap_or_default();
    assert!(
        saved.contains(&outside.display().to_string()),
        "always persists the granted write path to [sandbox] allow_write: {saved}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Hook powers (roadmap 9.2): `updated_input` patches the upcoming
/// call's arguments before execution.
#[tokio::test]
async fn hook_updated_input_patches_arguments() {
    let dir = tmp_dir("hook-patch");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "bash",
                serde_json::json!({"command": "echo original"}),
            )]),
        );
    voice.set_hooks(vec![Hook {
        event: CfgHookEvent::PreToolUse,
        tool: Some("bash".into()),
        command: r#"echo '{"updated_input":{"command":"echo patched-by-hook"}}'"#.to_string(),
    }]);
    let events = run_turn(&mut voice, "go", None).await;
    assert!(
        events.iter().any(|e| matches!(e, Event::Note { message } if message.contains("hook patched bash arguments (command)"))),
        "the patch surfaces as a note: {events:?}"
    );
    let outs = outputs_of(&events, "bash");
    assert!(
        outs.first().is_some_and(|o| o.contains("patched-by-hook")),
        "the patched command is what ran: {outs:?}"
    );
    assert!(
        outs.first().is_some_and(|o| !o.contains("original")),
        "the original command must not run: {outs:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A speaker whose call queue the test can extend between turns (one
/// scripted call per model round).
struct QueueSpeaker {
    calls: parking_lot::Mutex<VecDeque<(&'static str, serde_json::Value)>>,
}

impl Speaker for QueueSpeaker {
    fn speak<'a>(
        &'a self,
        _req: SpeakRequest,
        out: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> SpeakFuture<'a> {
        Box::pin(async move {
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

/// Hook powers (roadmap 9.2): `decision: "allow"` pre-approves the call
/// (no ask in guarded mode) — but hardstops outrank every hook.
#[tokio::test]
async fn hook_pre_approve_skips_the_ask_but_not_hardstops() {
    let dir = tmp_dir("hook-allow");
    let marker = dir.join("pre.txt");
    let queue = std::sync::Arc::new(QueueSpeaker {
        calls: parking_lot::Mutex::new(VecDeque::from([(
            "bash",
            serde_json::json!({"command": format!("touch {}", marker.display())}),
        )])),
    });
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Guarded, 5)
        .with_speaker(ka_dialect::Wire::OpenaiChat, queue.clone());
    voice.set_hooks(vec![Hook {
        event: CfgHookEvent::PreToolUse,
        tool: Some("bash".into()),
        command: r#"echo '{"decision":"allow"}'"#.to_string(),
    }]);
    let events = run_turn(&mut voice, "one", Some(2)).await;
    assert!(
        asks_of(&events).is_empty(),
        "pre-approve must skip the permission ask: {events:?}"
    );
    assert!(marker.exists(), "the pre-approved command ran");
    assert!(
        events.iter().any(
            |e| matches!(e, Event::Note { message } if message.contains("hook pre-approved `bash`"))
        ),
        "every pre-approve is a visible transcript record: {events:?}"
    );
    // the second turn brings a hardstop-class command: the pre-approve
    // must be overruled and the ask must still reach the human
    queue
        .calls
        .lock()
        .push_back(("bash", serde_json::json!({"command": ":(){ :|:& };:"})));
    let events = run_turn(&mut voice, "two", Some(2)).await;
    let asks = asks_of(&events);
    assert!(
        asks.first().is_some_and(|q| q.text.contains("HARDSTOP")),
        "hardstops still ask despite the pre-approve: {asks:?}"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, Event::Note { message } if message.contains("hook pre-approve ignored"))
        ),
        "the overruled pre-approve surfaces too: {events:?}"
    );
    assert!(!errored_outputs(&events, "bash").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Hook powers (roadmap 9.2, hardening): `decision: "allow"` may only
/// downgrade an ask. Deny-class gate verdicts — a config deny rule, a
/// plan-mode write outside .ka/plans/ — refuse despite the pre-approve.
#[tokio::test]
async fn hook_pre_approve_cannot_override_deny_verdicts() {
    // deny rule: the refusal stands, no ask, nothing runs
    let dir = tmp_dir("hook-allow-deny");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Guarded, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("bash", serde_json::json!({"command": "git status"}))]),
        );
    voice.set_rules(vec![ka_engine::config::Rule {
        tool: "bash".into(),
        pattern: Some("git *".into()),
        verdict: ka_engine::config::Verdict::Deny,
    }]);
    voice.set_hooks(vec![Hook {
        event: CfgHookEvent::PreToolUse,
        tool: Some("bash".into()),
        command: r#"echo '{"decision":"allow"}'"#.to_string(),
    }]);
    let events = run_turn(&mut voice, "check git", None).await;
    assert!(
        asks_of(&events).is_empty(),
        "the hook pre-approve must not turn a deny into an ask: {events:?}"
    );
    assert!(
        errored_outputs(&events, "bash")
            .iter()
            .any(|e| e.contains("denied by rule")),
        "the deny rule must outrank the hook pre-approve: {events:?}"
    );
    drop(voice);

    // plan mode: a write outside .ka/plans/ stays denied
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Plan, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "write",
                serde_json::json!({"path": "src/main.rs", "content": "x"}),
            )]),
        );
    voice.set_hooks(vec![Hook {
        event: CfgHookEvent::PreToolUse,
        tool: Some("write".into()),
        command: r#"echo '{"decision":"allow"}'"#.to_string(),
    }]);
    let events = run_turn(&mut voice, "implement", None).await;
    assert!(
        asks_of(&events).is_empty(),
        "the hook pre-approve must not turn the plan denial into an ask: {events:?}"
    );
    assert!(
        errored_outputs(&events, "write")
            .iter()
            .any(|e| e.contains("plan mode is read-only")),
        "the plan-mode write denial must outrank the hook pre-approve: {events:?}"
    );
    assert!(
        !dir.join("src/main.rs").exists(),
        "the denied write must not execute"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The loop-gate rule domain (`[[rules]] tool = "loop"`, roadmap 9.4):
/// `ask` poses a question instead of silently erroring, `deny` refuses
/// outright, `allow` disables the guard, and with no rule the shipped
/// 4-identical-calls trip is unchanged.
#[tokio::test]
async fn loop_rule_ask_deny_allow_and_default_trip() {
    let dir = tmp_dir("loop-rule");
    std::fs::write(dir.join("f.txt"), "content").unwrap();
    let reads = |n: usize| {
        (0..n)
            .map(|_| {
                (
                    "read",
                    serde_json::json!({"path": dir.join("f.txt").display().to_string()}),
                )
            })
            .collect::<Vec<_>>()
    };

    // ask: the 3rd identical call poses the loop question
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 10)
        .with_speaker(ka_dialect::Wire::OpenaiChat, Scripted::calls(reads(3)));
    voice.set_rules(vec![Rule {
        tool: "loop".into(),
        pattern: None,
        verdict: Verdict::Ask,
    }]);
    let events = run_turn(&mut voice, "go", Some(0)).await;
    let asks = asks_of(&events);
    assert_eq!(asks.len(), 1, "{events:?}");
    assert!(asks[0].text.contains("loop detected"), "{:?}", asks[0].text);
    assert!(
        errored_outputs(&events, "read").is_empty(),
        "allow-once lets the call proceed"
    );
    // ask + deny answer refuses the call
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 10)
        .with_speaker(ka_dialect::Wire::OpenaiChat, Scripted::calls(reads(3)));
    voice.set_rules(vec![Rule {
        tool: "loop".into(),
        pattern: None,
        verdict: Verdict::Ask,
    }]);
    let events = run_turn(&mut voice, "go", Some(2)).await;
    assert_eq!(refused_calls(&events, "read"), 1, "{events:?}");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::CallOutput { tool, .. } if tool == "read"))
            .count(),
        2,
        "only the first two identical reads ran: {events:?}"
    );

    // deny: refused outright at the threshold, no ask
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 10)
        .with_speaker(ka_dialect::Wire::OpenaiChat, Scripted::calls(reads(3)));
    voice.set_rules(vec![Rule {
        tool: "loop".into(),
        pattern: None,
        verdict: Verdict::Deny,
    }]);
    let events = run_turn(&mut voice, "go", None).await;
    assert!(asks_of(&events).is_empty(), "deny never asks: {events:?}");
    assert_eq!(refused_calls(&events, "read"), 1, "{events:?}");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::CallOutput { tool, .. } if tool == "read"))
            .count(),
        2,
        "the loop rule denies at the 3rd identical call: {events:?}"
    );

    // allow: the guard is off — identical calls stream through
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 10)
        .with_speaker(ka_dialect::Wire::OpenaiChat, Scripted::calls(reads(4)));
    voice.set_rules(vec![Rule {
        tool: "loop".into(),
        pattern: None,
        verdict: Verdict::Allow,
    }]);
    let events = run_turn(&mut voice, "go", None).await;
    assert!(
        errored_outputs(&events, "read").is_empty(),
        "loop allow never trips: {events:?}"
    );

    // no rule: the shipped trip at 4 identical calls is unchanged
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 10)
        .with_speaker(ka_dialect::Wire::OpenaiChat, Scripted::calls(reads(4)));
    let events = run_turn(&mut voice, "go", None).await;
    assert_eq!(
        refused_calls(&events, "read"),
        1,
        "the shipped 4-identical-calls trip still fires: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Speaker for the auto_review contract: tool-bearing requests pop the
/// scripted call queue; tool-less requests are reviewer probes answered
/// from `verdict`.
struct ReviewSpeaker {
    calls: parking_lot::Mutex<VecDeque<(&'static str, serde_json::Value)>>,
    verdict: &'static str,
}

impl Speaker for ReviewSpeaker {
    fn speak<'a>(
        &'a self,
        req: SpeakRequest,
        out: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> SpeakFuture<'a> {
        Box::pin(async move {
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

/// `[guards] auto_review` (roadmap 9.4): a confident reviewer allows
/// the obvious command without asking (and says so in a note); an
/// unconfident one falls through to the human — it can never deny.
#[tokio::test]
async fn auto_review_allows_confident_and_defers_everything_else() {
    let dir = tmp_dir("auto-review");
    // confident: no ask, the command ran, the auto-allow is visible
    let marker = dir.join("reviewed.txt");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Guarded, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            std::sync::Arc::new(ReviewSpeaker {
                calls: parking_lot::Mutex::new(VecDeque::from([(
                    "bash",
                    serde_json::json!({"command": format!("touch {}", marker.display())}),
                )])),
                verdict: "allow",
            }),
        );
    voice.set_auto_review(true, Some("test/m".to_string()));
    let events = run_turn(&mut voice, "go", Some(0)).await;
    assert!(
        asks_of(&events).is_empty(),
        "a confident review skips the ask: {events:?}"
    );
    assert!(marker.exists(), "the reviewed command ran");
    assert!(
        events.iter().any(|e| matches!(e, Event::Note { message } if message.contains("auto_review: allowed `touch"))),
        "every auto-allow is a visible record: {events:?}"
    );

    // unconfident: the ask still reaches the human (never auto-denied)
    let marker = dir.join("deferred.txt");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Guarded, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            std::sync::Arc::new(ReviewSpeaker {
                calls: parking_lot::Mutex::new(VecDeque::from([(
                    "bash",
                    serde_json::json!({"command": format!("touch {}", marker.display())}),
                )])),
                verdict: "ask",
            }),
        );
    voice.set_auto_review(true, Some("test/m".to_string()));
    let events = run_turn(&mut voice, "go", Some(0)).await;
    assert_eq!(
        asks_of(&events).len(),
        1,
        "an unconfident review defers to the human: {events:?}"
    );
    assert!(marker.exists(), "the human's allow runs the command");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Custom-command turn scoping (roadmap 9.7): `allowed-tools`
/// frontmatter rides the Prompt; tools outside the list are denied at
/// admit time even when the model calls them anyway.
#[tokio::test]
async fn turn_scoped_allowed_tools_restrict_the_turn() {
    let dir = tmp_dir("turn-scope");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            // the model tries a bash call the command did not allow
            Scripted::calls(vec![("bash", serde_json::json!({"command": "echo nope"}))]),
        );
    voice.set_turn_tools(Some(vec!["read".to_string()]));
    let events = run_turn(&mut voice, "go", None).await;
    let outs = errored_outputs(&events, "bash");
    assert!(
        outs.first()
            .is_some_and(|o| o.contains("not in this command's allowed tools")),
        "{events:?}"
    );
    // clearing restores the session toolset: the previously-refused
    // tool now runs (the engine clears the scope right after a scoped
    // turn; the engine-level end-to-end pin lives in engine_e2e's
    // prompt_scoping_allowed_tools_and_model_override)
    voice.set_turn_tools(None);
    let events = run_turn(&mut voice, "go again", None).await;
    let outs = errored_outputs(&events, "bash");
    assert!(
        outs.is_empty(),
        "scope must not leak into the next turn: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
