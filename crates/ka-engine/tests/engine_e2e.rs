//! End-to-end engine contracts over the real `spawn_full` loop: the
//! surface-facing commands every documented feature rides on. If one of
//! these breaks, a user-visible command broke — deliberate changes must
//! update this file and the docs together.

#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::time::Duration;

use ka_dialect::Catalog;
use ka_protocol::{Command, Event, Stop};

fn test_catalog() -> ka_dialect::Catalog {
    Catalog::parse(
        "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
    )
    .unwrap()
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-e2e-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Send a command and collect events until `done` matches (or the
/// stream closes). Most commands do NOT emit `Idle` — only the prompt
/// cycle does — so the caller states what completes this exchange.
/// Fails loudly on timeout so a stuck engine fails, never hangs.
async fn settle(
    handle: &mut ka_engine::EngineHandle,
    cmd: Command,
    done: impl Fn(&Event) -> bool,
) -> Vec<Event> {
    settle_answering(handle, cmd, &done, None).await
}

/// Send a command and collect events until `done` matches, then drain
/// until the engine goes quiet (300 ms without events). The quiet drain
/// is load-bearing: some arms emit trailing events (Idle), and sending
/// the next command while a turn is still live would let the voice's
/// mid-turn command select silently swallow it. Permission asks are
/// auto-answered with `answer` when given.
async fn settle_answering(
    handle: &mut ka_engine::EngineHandle,
    cmd: Command,
    done: &dyn Fn(&Event) -> bool,
    answer: Option<usize>,
) -> Vec<Event> {
    handle.commands.send(cmd).await.expect("engine alive");
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(15), handle.events.recv()).await {
            Ok(Some(e)) => {
                if let (Event::Ask { id, .. }, Some(choice)) = (&e, answer) {
                    handle
                        .commands
                        .send(Command::Answer {
                            question: id.clone(),
                            choice,
                        })
                        .await
                        .ok();
                }
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
    // quiet drain: the engine idles only after the arm fully returned
    loop {
        match tokio::time::timeout(Duration::from_millis(300), handle.events.recv()).await {
            Ok(Some(e)) => out.push(e),
            _ => return out,
        }
    }
}

/// The prompt cycle completes with `Idle` (the one event that always
/// terminates a prompt exchange).
fn on_idle(e: &Event) -> bool {
    matches!(e, Event::Idle)
}

use std::collections::VecDeque;

use ka_dialect::speaker::{
    SpeakFuture, SpeakRequest, Speaker, StreamEvent, ToolCall, TurnMessage, TurnRole,
};

/// A speaker that emits one scripted tool call per request round and a
/// final text reply once the queue is empty, recording every request —
/// so tests can assert what the model actually received (e.g. the
/// [verify] fix-round prompt).
struct Scripted {
    calls: parking_lot::Mutex<VecDeque<(&'static str, serde_json::Value)>>,
    texts: parking_lot::Mutex<VecDeque<String>>,
    requests: parking_lot::Mutex<Vec<Vec<TurnMessage>>>,
    models: parking_lot::Mutex<Vec<String>>,
    efforts: parking_lot::Mutex<Vec<Option<String>>>,
    tool_counts: parking_lot::Mutex<Vec<usize>>,
}

impl Scripted {
    fn new(calls: Vec<(&'static str, serde_json::Value)>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            calls: parking_lot::Mutex::new(calls.into()),
            texts: parking_lot::Mutex::new(VecDeque::new()),
            requests: parking_lot::Mutex::new(Vec::new()),
            models: parking_lot::Mutex::new(Vec::new()),
            efforts: parking_lot::Mutex::new(Vec::new()),
            tool_counts: parking_lot::Mutex::new(Vec::new()),
        })
    }
}

impl Speaker for Scripted {
    fn speak<'a>(
        &'a self,
        req: SpeakRequest,
        out: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> SpeakFuture<'a> {
        Box::pin(async move {
            self.requests.lock().push(req.messages.clone());
            self.models.lock().push(req.model_id.clone());
            self.efforts.lock().push(req.effort.clone());
            self.tool_counts.lock().push(req.tools.len());
            let next = self.calls.lock().pop_front();
            // an empty tool name scripts a dry round: no call, the
            // round closes and the turn ends (auto-poke tests need to
            // end a turn with work still pending)
            if let Some((tool, args)) = next {
                if !tool.is_empty() {
                    out.send(StreamEvent::Call(ToolCall {
                        id: "c1".into(),
                        tool: tool.into(),
                        arguments: args,
                    }))
                    .await
                    .ok();
                }
            } else {
                let text = self
                    .texts
                    .lock()
                    .pop_front()
                    .unwrap_or_else(|| "done".into());
                out.send(StreamEvent::Text(text)).await.ok();
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

/// A canned prompt turn runs to completion: exactly one TurnFinished
/// (stop = Done) and one Idle — the Phase-0 contract every surface
/// builds on.
#[tokio::test]
async fn prompt_canned_turn_contract() {
    let dir = tmp_dir("prompt");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "hello".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    let finishes: Vec<&Stop> = events
        .iter()
        .filter_map(|e| match e {
            Event::TurnFinished { stop, .. } => Some(stop),
            _ => None,
        })
        .collect();
    assert_eq!(finishes, vec![&Stop::Done], "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::Idle)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `!` passthrough: the engine runs the user's shell command, reports
/// the output (redacted, capped), and stages it as context.
#[tokio::test]
async fn shell_passthrough_runs_and_reports() {
    let dir = tmp_dir("shell");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(
        &mut handle,
        Command::Shell {
            command: "echo contract-echo-9137".into(),
        },
        |e| matches!(e, Event::ShellOutput { .. }),
    )
    .await;
    match events
        .iter()
        .find(|e| matches!(e, Event::ShellOutput { .. }))
    {
        Some(Event::ShellOutput {
            command,
            output,
            note,
        }) => {
            assert_eq!(command, "echo contract-echo-9137");
            assert!(output.contains("contract-echo-9137"), "{output:?}");
            assert!(note.is_none(), "clean exit carries no note: {note:?}");
        }
        other => panic!("expected ShellOutput, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Shell failures surface in the note field (non-zero exit and spawn
/// errors), never as a hard error event.
#[tokio::test]
async fn shell_passthrough_failure_notes() {
    let dir = tmp_dir("shell-fail");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(
        &mut handle,
        Command::Shell {
            command: "exit 7".into(),
        },
        |e| matches!(e, Event::ShellOutput { .. }),
    )
    .await;
    match events
        .iter()
        .find(|e| matches!(e, Event::ShellOutput { .. }))
    {
        Some(Event::ShellOutput { output, note, .. }) => {
            assert_eq!(note.as_deref(), Some("exit 7"), "{events:?}");
            assert!(output.is_empty(), "{output:?}");
        }
        other => panic!("expected ShellOutput, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// /tasks snapshot: empty state reports cleanly; the command never
/// errors.
#[tokio::test]
async fn list_tasks_reports_snapshot() {
    let dir = tmp_dir("tasks");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(&mut handle, Command::ListTasks, |e| {
        matches!(e, Event::Tasks { .. })
    })
    .await;
    match events.iter().find(|e| matches!(e, Event::Tasks { .. })) {
        Some(Event::Tasks { rows }) => {
            assert!(
                rows.iter().any(|r| r.contains("no background")),
                "empty state is a row, not an error: {rows:?}"
            );
        }
        other => panic!("expected Tasks, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// /tasks pager: an unknown task id errors with the documented message
/// (the picker stays open, the engine stays responsive).
#[tokio::test]
async fn task_detail_unknown_id_errors() {
    let dir = tmp_dir("task-detail");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(&mut handle, Command::TaskDetail { id: 99 }, |e| {
        matches!(e, Event::Error { .. })
    })
    .await;
    match events.iter().find(|e| matches!(e, Event::Error { .. })) {
        Some(Event::Error { message, .. }) => {
            assert!(message.contains("no such task: t-99"), "{events:?}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// /debug overlay: with no [debug] config the roster says the tier is
/// disabled and the engine still reaches Idle.
#[tokio::test]
async fn debug_roster_reports_disabled_tier() {
    let dir = tmp_dir("debug-roster");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(&mut handle, Command::DebugRoster, |e| {
        matches!(e, Event::DebugRoster { .. })
    })
    .await;
    match events
        .iter()
        .find(|e| matches!(e, Event::DebugRoster { .. }))
    {
        Some(Event::DebugRoster { rows }) => {
            assert!(
                rows.iter().any(|r| r.contains("debug tier disabled")),
                "{rows:?}"
            );
        }
        other => panic!("expected DebugRoster, got {other:?}"),
    }
    assert!(
        events.iter().any(|e| matches!(e, Event::Idle)),
        "the arm ends in Idle: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Rewind on an empty session errors with the documented message
/// instead of corrupting state.
#[tokio::test]
async fn rewind_on_empty_history_errors() {
    let dir = tmp_dir("rewind-empty");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(&mut handle, Command::Rewind { turns: 1 }, |e| {
        matches!(e, Event::Error { .. } | Event::Note { .. } | Event::Idle)
    })
    .await;
    assert!(
        events.iter().any(
            |e| matches!(e, Event::Error { message, .. } if message.contains("cannot rewind"))
        ),
        "{events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Undo with nothing parked is a clean note, never an error.
#[tokio::test]
async fn undo_without_snapshots_is_a_note() {
    let dir = tmp_dir("undo-empty");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(&mut handle, Command::UndoFile, |e| {
        matches!(e, Event::Note { .. } | Event::Error { .. })
    })
    .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("nothing to undo"))),
        "{events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Checkpoint/restore on a git repo: the checkpoint notes its id, a
/// working-tree change is reverted by restore, and the user's git
/// state (HEAD, index) is untouched.
#[tokio::test]
async fn checkpoint_and_restore_revert_working_tree() {
    let dir = tmp_dir("checkpoint");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap()
    };
    assert!(git(&["init", "-q"]).status.success());
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(dir.join("f.txt"), "v1\n").unwrap();
    git(&["add", "."]);
    assert!(git(&["commit", "-m", "init"]).status.success());
    let head_before = String::from_utf8_lossy(&git(&["rev-parse", "HEAD"]).stdout).to_string();

    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(&mut handle, Command::Checkpoint, |e| {
        matches!(e, Event::Note { .. } | Event::Error { .. })
    })
    .await;
    let note = events.iter().find_map(|e| match e {
        Event::Note { message } => Some(message.clone()),
        _ => None,
    });
    assert!(
        note.as_deref()
            .is_some_and(|n| n.contains("checkpoint") && n.contains("saved")),
        "{events:?}"
    );
    // the note carries the SHORT checkpoint id; restore by prefix later
    let short_id = note
        .as_deref()
        .and_then(|n| n.split_whitespace().nth(1))
        .expect("checkpoint id in note")
        .to_string();

    // mutate the working tree, then restore
    std::fs::write(dir.join("f.txt"), "v2 mutated\n").unwrap();
    std::fs::write(dir.join("extra.txt"), "untracked\n").unwrap();
    let events = settle_answering(
        &mut handle,
        Command::RestoreCheckpoint { id: short_id },
        &|e| matches!(e, Event::Note { .. } | Event::Error { .. }),
        Some(0),
    )
    .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("restored"))),
        "restore notes the restore: {events:?}"
    );
    assert!(
        std::fs::read_to_string(dir.join("f.txt"))
            .unwrap()
            .starts_with("v1"),
        "restore reverts the tracked file to the checkpointed content"
    );
    // git-checkout semantics: tracked files revert, untracked files
    // created after the checkpoint survive (they are the user's)
    assert!(
        dir.join("extra.txt").exists(),
        "restore must not delete untracked files"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Error { .. })),
        "restore must not error: {events:?}"
    );
    // engine checkpoints are working-tree snapshots via the temp git
    // index: the tracked file content returns to the checkpointed state
    // (exact semantics asserted in checkpoint.rs unit tests); here we
    // contract-check the user-visible outcome: the engine reported
    // success and HEAD is untouched.
    let head_after = String::from_utf8_lossy(&git(&["rev-parse", "HEAD"]).stdout).to_string();
    assert_eq!(head_before, head_after, "checkpoints never move HEAD");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Export an empty session refuses; after a turn, the markdown file
/// exists with content.
#[tokio::test]
async fn export_markdown_contract() {
    let dir = tmp_dir("export");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());

    let events = settle(
        &mut handle,
        Command::ExportMarkdown {
            out: None,
            html: false,
        },
        |e| matches!(e, Event::Note { .. } | Event::Error { .. }),
    )
    .await;
    assert!(
        events.iter().any(
            |e| matches!(e, Event::Error { message, .. } if message.contains("nothing to export"))
        ),
        "{events:?}"
    );

    // a canned turn records a user message; export then works
    let _ = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "record me".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    let out = dir.join("export.md");
    let events = settle(
        &mut handle,
        Command::ExportMarkdown {
            out: Some(out.clone()),
            html: false,
        },
        |e| matches!(e, Event::Note { .. } | Event::Error { .. }),
    )
    .await;
    assert!(
        !events.iter().any(|e| matches!(e, Event::Error { .. })),
        "{events:?}"
    );
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(
        text.contains("record me"),
        "prompt lands in the export: {text:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Switching to a fresh strand works and the engine keeps serving.
#[tokio::test]
async fn switch_strand_new_keeps_engine_alive() {
    let dir = tmp_dir("switch");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(
        &mut handle,
        Command::SwitchStrand { id: "new".into() },
        |e| matches!(e, Event::SessionInfo { .. } | Event::Error { .. }),
    )
    .await;
    assert!(
        !events.iter().any(|e| matches!(e, Event::Error { .. })),
        "{events:?}"
    );
    // engine still responsive: another command settles
    let events = settle(&mut handle, Command::ListTasks, |e| {
        matches!(e, Event::Tasks { .. })
    })
    .await;
    assert!(events.iter().any(|e| matches!(e, Event::Tasks { .. })));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Auto-commit off by default: a turn must not create commits.
#[tokio::test]
async fn auto_commit_off_by_default_leaves_repo_untouched() {
    let dir = tmp_dir("no-autocommit");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap()
    };
    assert!(git(&["init", "-q"]).status.success());
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(dir.join("f.txt"), "x\n").unwrap();
    git(&["add", "."]);
    assert!(git(&["commit", "-m", "init"]).status.success());

    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let _ = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "hello".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    let log = String::from_utf8_lossy(&git(&["log", "--oneline"]).stdout).to_string();
    assert_eq!(
        log.lines().count(),
        1,
        "no ka commit without [git] auto_commit: {log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------- [verify] test loop

/// The [verify] loop end-to-end: a turn that edits files runs the
/// configured test command; a FAILURE is fed back to the model as one
/// automatic fix turn (whose prompt carries the failure output), and no
/// second fix round fires.
#[tokio::test]
async fn verify_failure_feeds_back_exactly_one_fix_round() {
    let dir = tmp_dir("verify-fail");
    std::fs::write(dir.join("v.txt"), "alpha\n").unwrap();
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        // a model is required for the live (scripted) voice; without it
        // the engine runs the canned speaker and skips tools entirely
        model: Some("test/m".into()),
        verify: ka_engine::config::Verify {
            test: Some("echo VERIFY-FAIL-MARKER >&2; exit 3".into()),
            lints: Vec::new(),
        },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![
        ("read", serde_json::json!({"path": "v.txt"})),
        (
            "edit",
            serde_json::json!({"path": "v.txt", "old": "alpha", "new": "ALPHA"}),
        ),
    ]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "edit the file".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;

    let finishes = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::TurnFinished {
                    stop: Stop::Done,
                    ..
                }
            )
        })
        .count();
    assert_eq!(finishes, 2, "edit turn + fix turn: {events:?}");
    assert!(
        events.iter().any(|e| matches!(e, Event::Note { message }
            if message.contains("[verify]") && message.contains("feeding failures back"))),
        "the failure note surfaces: {events:?}"
    );
    // the fix turn's prompt carried the test output
    let fix_prompt = speaker
        .requests
        .lock()
        .iter()
        .rev()
        .find(|msgs| {
            msgs.iter()
                .any(|m| m.role == TurnRole::User && m.content.contains("VERIFY-FAIL-MARKER"))
        })
        .is_some();
    assert!(fix_prompt, "fix round receives the failure output");
    // exactly one fix round: no third cycle, no second [verify] run
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("passed"))),
        "{events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A passing [verify] test is announced and the turn count stays at one.
#[tokio::test]
async fn verify_pass_is_announced_without_fix_round() {
    let dir = tmp_dir("verify-pass");
    std::fs::write(dir.join("v.txt"), "alpha\n").unwrap();
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        // a model is required for the live (scripted) voice; without it
        // the engine runs the canned speaker and skips tools entirely
        model: Some("test/m".into()),
        verify: ka_engine::config::Verify {
            test: Some("true".into()),
            lints: Vec::new(),
        },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![
        ("read", serde_json::json!({"path": "v.txt"})),
        (
            "edit",
            serde_json::json!({"path": "v.txt", "old": "alpha", "new": "ALPHA"}),
        ),
    ]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker,
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "edit the file".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    let finishes = events
        .iter()
        .filter(|e| matches!(e, Event::TurnFinished { .. }))
        .count();
    assert_eq!(finishes, 1, "no fix round on a passing test: {events:?}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("`true` passed"))),
        "{events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Abort during a long [verify] test cancels it (no fix round) — the
/// engine stays responsive mid-test.
#[tokio::test]
async fn verify_test_is_abortable() {
    let dir = tmp_dir("verify-abort");
    std::fs::write(dir.join("v.txt"), "alpha\n").unwrap();
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        // a model is required for the live (scripted) voice; without it
        // the engine runs the canned speaker and skips tools entirely
        model: Some("test/m".into()),
        verify: ka_engine::config::Verify {
            test: Some("sleep 30".into()),
            lints: Vec::new(),
        },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![
        ("read", serde_json::json!({"path": "v.txt"})),
        (
            "edit",
            serde_json::json!({"path": "v.txt", "old": "alpha", "new": "ALPHA"}),
        ),
    ]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker,
    );
    handle
        .commands
        .send(Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "edit the file".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("engine alive");
    // wait for the edit turn to finish, then abort during the test run
    loop {
        match tokio::time::timeout(Duration::from_secs(15), handle.events.recv()).await {
            Ok(Some(Event::TurnFinished { .. })) => break,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("engine ended before the verify test"),
            Err(_) => panic!("edit turn did not finish within 15s"),
        }
    }
    handle
        .commands
        .send(Command::Abort)
        .await
        .expect("engine alive");
    let events = settle(&mut handle, Command::ListTasks, |e| {
        matches!(e, Event::Note { .. } | Event::Idle)
    })
    .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("cancelled"))),
        "the verify test reports cancellation: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("passed"))),
        "a cancelled test must not report passed: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Lifecycle hooks (roadmap 9.2): session_start, user_prompt_submit,
/// pre_compact, stop, and turn_end all fire over the real engine loop
/// with the documented stdin payloads' event names; session_end fires
/// when the command channel closes.
#[tokio::test]
async fn lifecycle_hooks_fire_across_the_engine_loop() {
    let dir = tmp_dir("hooks-lifecycle");
    let hook = |event: &str, name: &str| {
        format!(
            "[[hooks]]\nevent = \"{event}\"\ncommand = \"touch {}\"\n",
            dir.join(format!("{name}.txt")).display()
        )
    };
    let layer = format!(
        "model = \"test/m\"\ncwd = \"{}\"\n{}{}{}{}{}{}",
        dir.display(),
        hook("session_start", "start"),
        hook("user_prompt_submit", "prompt"),
        hook("pre_compact", "compact"),
        hook("stop", "stop"),
        hook("turn_end", "turnend"),
        hook("session_end", "end"),
    );
    let cfg = ka_engine::config::Config::parse_layer(&layer, "test").unwrap();
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        Scripted::new(vec![]), // text-only turns
    );
    // one prompt cycle fires session_start + user_prompt_submit + stop
    // + turn_end
    settle(
        &mut handle,
        Command::Prompt {
            text: "hi".into(),
            schema: None,
            images: vec![],
            allowed_tools: None,
            model: None,
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    for name in ["start", "prompt", "stop", "turnend"] {
        assert!(
            dir.join(format!("{name}.txt")).exists(),
            "{name} hook must fire over the engine loop"
        );
    }
    // a compact fires pre_compact before the digest runs
    settle(&mut handle, Command::Compact { focus: None }, on_idle).await;
    assert!(
        dir.join("compact.txt").exists(),
        "pre_compact hook must fire"
    );
    // dropping the command channel ends the session → session_end
    // (keep draining events so the engine's final sends never block)
    let ka_engine::EngineHandle { commands, events } = handle;
    drop(commands);
    let mut events = events;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !dir.join("end.txt").exists() && std::time::Instant::now() < deadline {
        let _ = tokio::time::timeout(Duration::from_millis(100), events.recv()).await;
    }
    assert!(
        dir.join("end.txt").exists(),
        "session_end hook must fire at shutdown"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `Command::Prompt`'s per-turn scoping fields are real contract:
/// `allowed_tools` restricts the turn's toolset (a call outside the
/// list is refused with ok:false and no CallOutput), `model` overrides
/// the session selector for that turn (reaching the wire request), an
/// invalid override selector surfaces as a Note, and the scope clears
/// when the turn ends.
#[tokio::test]
async fn prompt_scoping_allowed_tools_and_model_override() {
    let dir = tmp_dir("prompt-scope");
    std::fs::write(dir.join("s.txt"), "content\n").unwrap();
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        ..Default::default()
    };
    let speaker = Scripted::new(vec![
        ("read", serde_json::json!({"path": "s.txt"})),
        ("bash", serde_json::json!({"command": "echo nope"})),
    ]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: Some(vec!["read".into()]),
            model: Some("test/m@high".into()),
            text: "scope this turn".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;

    // the read call executed (ok:true); the bash call was refused by
    // the allowlist (ok:false, and no CallOutput ever for it)
    let finished: Vec<(String, bool)> = events
        .iter()
        .filter_map(|e| match e {
            Event::CallFinished { tool, ok, .. } => Some((tool.clone(), *ok)),
            _ => None,
        })
        .collect();
    assert!(
        finished.iter().any(|(t, ok)| t == "read" && *ok),
        "read allowed: {finished:?}"
    );
    assert!(
        finished.iter().any(|(t, ok)| t == "bash" && !*ok),
        "bash refused: {finished:?}"
    );
    // the refusal rides the tool result back to the model (the loop
    // gate's no-output shape does not apply to allowlist refusals)
    assert!(
        events.iter().any(
            |e| matches!(e, Event::CallOutput { tool, excerpt, is_error: true, .. }
            if tool == "bash" && excerpt.contains("allowed tools"))
        ),
        "{events:?}"
    );
    // the per-turn model override reached the wire request: the
    // @high suffix is observable on the wire as the request's effort
    // (the model id itself is the same as the session default, so
    // asserting on it alone would be vacuous)
    assert_eq!(
        speaker.efforts.lock().first().cloned().flatten(),
        Some("high".to_string()),
        "the @high override must reach SpeakRequest.effort"
    );
    // the turn still completes cleanly
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TurnFinished {
                stop: Stop::Done,
                ..
            }
        )),
        "{events:?}"
    );

    // the follow-up turn completes unscoped...
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "next turn unscoped".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TurnFinished {
                stop: Stop::Done,
                ..
            }
        )),
        "{events:?}"
    );
    // ...and the offered toolsets prove the scope cleared: the scoped
    // turn's rounds saw exactly the allowlisted tool, the follow-up
    // turn is offered the whole session toolset again
    assert!(
        speaker.efforts.lock().last().is_none_or(|e| e.is_none()),
        "the override must not leak into the next turn"
    );
    let counts = speaker.tool_counts.lock().clone();
    assert_eq!(
        counts.first(),
        Some(&1),
        "scoped turn sees only [read]: {counts:?}"
    );
    assert!(
        counts.last().is_some_and(|c| *c > 1),
        "follow-up turn sees the full toolset: {counts:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A per-turn model override that is not a valid selector is refused
/// with a visible note — never silently dropped.
#[tokio::test]
async fn prompt_model_override_rejects_invalid_selector() {
    let dir = tmp_dir("prompt-bad-selector");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        ..Default::default()
    };
    let speaker = Scripted::new(vec![]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker,
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: Some("bogus-no-slash".into()),
            text: "hello".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    assert!(
        events.iter().any(
            |e| matches!(e, Event::Note { message } if message.contains("not a valid selector"))
        ),
        "invalid override surfaces a note: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------- [pokes] auto-poke

fn todo_args(items: &[(&str, &str)]) -> serde_json::Value {
    serde_json::json!({ "items": items
        .iter()
        .map(|(text, state)| serde_json::json!({"text": text, "state": state}))
        .collect::<Vec<_>>() })
}

fn count(events: &[Event], f: fn(&Event) -> bool) -> usize {
    events.iter().filter(|e| f(e)).count()
}

fn on_turn_started(e: &Event) -> bool {
    matches!(e, Event::TurnStarted { .. })
}

fn on_poke_note(e: &Event) -> bool {
    matches!(e, Event::Note { message } if message.contains("auto-poke"))
}

/// A turn that settles with pending todos earns exactly one
/// continuation round whose prompt names the pending items; once the
/// list is empty the loop stops (no third turn).
#[tokio::test]
async fn auto_poke_continues_until_todos_done() {
    let dir = tmp_dir("poke-happy");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        ..Default::default()
    };
    let speaker = Scripted::new(vec![
        ("todo", todo_args(&[("wire the pump", "pending")])),
        ("", serde_json::Value::Null), // dry round: turn 1 ends, item pending
        ("todo", todo_args(&[("wire the pump", "done")])), // the poke turn finishes it
    ]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "wire the pump".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    assert_eq!(count(&events, on_turn_started), 2, "{events:?}");
    assert_eq!(count(&events, on_poke_note), 1, "{events:?}");
    // the poke prompt reached the model as a user message naming the item
    let requests = speaker.requests.lock();
    let poked = requests.iter().any(|round| {
        round.iter().any(|m| {
            m.content.contains("(ka auto-poke 1/3)") && m.content.contains("wire the pump")
        })
    });
    assert!(poked, "poke prompt must reach the model: {requests:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// An unchanged pending set is no progress: exactly one poke fires,
/// then the loop stops instead of burning the remaining rounds.
#[tokio::test]
async fn auto_poke_stops_when_the_pending_set_does_not_move() {
    let dir = tmp_dir("poke-stuck");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        ..Default::default()
    };
    // both turns leave the SAME two items pending
    let items = todo_args(&[("a", "pending"), ("b", "pending")]);
    let speaker = Scripted::new(vec![("todo", items.clone()), ("todo", items)]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker,
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "do a and b".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    assert_eq!(count(&events, on_turn_started), 2, "{events:?}");
    assert_eq!(count(&events, on_poke_note), 1, "{events:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `[pokes] max = 0` disables the loop entirely.
#[tokio::test]
async fn auto_poke_disabled_at_zero() {
    let dir = tmp_dir("poke-off");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        pokes: ka_engine::config::Pokes { max: Some(0) },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![("todo", todo_args(&[("x", "pending")]))]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker,
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "x".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    assert_eq!(count(&events, on_turn_started), 1, "{events:?}");
    assert_eq!(count(&events, on_poke_note), 0, "{events:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A turn that ends on a non-Done stop (here: an error) never pokes —
/// wire retries have already run; the loop must not add rounds.
#[tokio::test]
async fn auto_poke_never_follows_a_failed_turn() {
    struct TodoThenErrorSpeaker {
        round: std::sync::atomic::AtomicU32,
    }
    impl Speaker for TodoThenErrorSpeaker {
        fn speak<'a>(
            &'a self,
            _req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                let n = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    // round 0 emits a todo call and finishes Done; the
                    // call executes and the NEXT round finishes Error —
                    // one turn, ending hard-failed with a pending item
                    out.send(StreamEvent::Call(ToolCall {
                        id: "c1".into(),
                        tool: "todo".into(),
                        arguments: todo_args(&[("pending work", "pending")]),
                    }))
                    .await
                    .ok();
                    out.send(StreamEvent::Finished {
                        stop: Stop::Done,
                        usage: ka_protocol::Usage::default(),
                    })
                    .await
                    .ok();
                } else {
                    out.send(StreamEvent::Finished {
                        stop: Stop::Error,
                        usage: ka_protocol::Usage::default(),
                    })
                    .await
                    .ok();
                }
            })
        }
    }
    let dir = tmp_dir("poke-error");
    // the turn assigns a pending todo mid-flight and its next model
    // round fails hard (a stream Finished{Error}, not a retryable wire
    // failure) — the error stop alone must bar the poke
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        std::sync::Arc::new(TodoThenErrorSpeaker {
            round: std::sync::atomic::AtomicU32::new(0),
        }),
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "anything".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    assert_eq!(count(&events, on_turn_started), 1, "{events:?}");
    assert_eq!(count(&events, on_poke_note), 0, "{events:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The bound itself: a model that keeps assigning NEW pending items
/// (progress every round) still earns exactly `max` poke rounds.
#[tokio::test]
async fn auto_poke_stops_at_max_rounds() {
    let dir = tmp_dir("poke-max");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        ..Default::default()
    };
    // each turn: assign a fresh pending item, then run dry — the turn
    // ends with work pending and the list keeps MOVING, so only the
    // bound can stop the loop
    let script = vec![
        ("todo", todo_args(&[("task-1", "pending")])),
        ("", serde_json::Value::Null),
        ("todo", todo_args(&[("task-2", "pending")])),
        ("", serde_json::Value::Null),
        ("todo", todo_args(&[("task-3", "pending")])),
        ("", serde_json::Value::Null),
        ("todo", todo_args(&[("task-4", "pending")])),
        ("", serde_json::Value::Null),
    ];
    let speaker = Scripted::new(script);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker,
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "work".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    // 1 user turn + exactly 3 pokes (the default max)
    assert_eq!(count(&events, on_turn_started), 4, "{events:?}");
    assert_eq!(count(&events, on_poke_note), 3, "{events:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Readonly mode never pokes — research is not execution.
#[tokio::test]
async fn auto_poke_never_fires_in_plan_mode() {
    let dir = tmp_dir("poke-plan");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        mode: Some(ka_protocol::Mode::Readonly),
        ..Default::default()
    };
    let speaker = Scripted::new(vec![
        ("todo", todo_args(&[("research-thing", "pending")])),
        ("", serde_json::Value::Null),
    ]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker,
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "research it".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    assert_eq!(count(&events, on_turn_started), 1, "{events:?}");
    assert_eq!(count(&events, on_poke_note), 0, "{events:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------- /name + /transfer

/// `/name fox` appends a Name record and `ka --session fox` resolves:
/// `resolve_name` finds the strand by its alias; invalid names refuse
/// with a usage note and write nothing.
#[tokio::test]
async fn set_name_persists_and_resolves() {
    let dir = tmp_dir("set-name");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let speaker = Scripted::new(vec![]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker,
    );
    let _ = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "hello world".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    let events = settle(
        &mut handle,
        Command::SetName { name: "fox".into() },
        on_idle,
    )
    .await;
    assert!(
        events.iter().any(
            |e| matches!(e, Event::Note { message } if message.contains("session name set: fox"))
        ),
        "name ack surfaces: {events:?}"
    );
    match ka_strand::resolve_name(&dir, "fox").unwrap() {
        ka_strand::IdMatch::Unique(s) => assert_eq!(s.name.as_deref(), Some("fox")),
        other => panic!("name must resolve after SetName: {other:?}"),
    }
    // invalid name: usage note, nothing written
    let events = settle(
        &mut handle,
        Command::SetName {
            name: "two words".into(),
        },
        on_idle,
    )
    .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("usage: /name"))),
        "invalid name refuses with usage: {events:?}"
    );
    assert_eq!(
        ka_strand::list(&dir).unwrap().len(),
        1,
        "no extra strand; the invalid name wrote nothing"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `/transfer` digests the live context and attaches a FRESH child
/// strand carrying only the digest: new session id, parent link, a
/// Digest record, and a "(transfer)" title.
#[tokio::test]
async fn transfer_digests_into_a_fresh_child_strand() {
    let dir = tmp_dir("transfer");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        ..Default::default()
    };
    let speaker = Scripted::new(vec![]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker,
    );
    let first = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "wire the pump".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    let old_id = first
        .iter()
        .find_map(|e| match e {
            Event::SessionInfo { id } => Some(id.clone()),
            _ => None,
        })
        .expect("SessionInfo after spawn");
    let events = settle(
        &mut handle,
        Command::TransferStrand { focus: None },
        on_idle,
    )
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Note { message } if message.contains("transferred to fresh session")
        )),
        "transfer ack surfaces: {events:?}"
    );
    let new_id = events
        .iter()
        .find_map(|e| match e {
            Event::SessionInfo { id } => Some(id.clone()),
            _ => None,
        })
        .expect("SessionInfo after transfer");
    assert_ne!(old_id, new_id, "transfer switches the live strand");
    // the child strand: parent link + digest-only carry + transfer title
    let listed = ka_strand::list(&dir).unwrap();
    let child = listed
        .iter()
        .find(|s| s.id == new_id)
        .expect("child strand listed");
    assert_eq!(child.parent.as_deref(), Some(old_id.as_str()));
    assert!(child.title.contains("(transfer)"), "{}", child.title);
    assert_eq!(child.messages, 0, "no message records ride over");
    // the digest IS the carried memory — exactly one Digest record
    let digest_records = ka_strand::StrandFile::open(&child.path)
        .unwrap()
        .records()
        .iter()
        .filter(|r| matches!(r, ka_strand::Record::Digest { .. }))
        .count();
    assert_eq!(digest_records, 1, "digest must ride into the child strand");
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------- MCP tool cache

/// A cache hit registers the server's tools immediately — even when
/// the server cannot spawn at all — so startup never blocks and the
/// tool list stays stable; the (failing) verification is background
/// and silent, the watchdog owns retries.
#[tokio::test]
async fn mcp_cache_registers_tools_without_waiting_on_the_server() {
    let _guard = ka_engine::mcp_cache::test_support::cache_guard();
    let dir = tmp_dir("mcp-cache");
    let server = ka_engine::mcp::McpServerConfig {
        name: "demo".into(),
        command: Some("/nonexistent-ka-mcp-server".into()),
        args: Vec::new(),
        env: std::collections::HashMap::new(),
        url: None,
        headers: Vec::new(),
    };
    // legacy-shape cache: a dotted name written before sanitization —
    // load must re-derive `demo__fetch`.
    ka_engine::mcp_cache::store_tools(
        &server,
        &[ka_engine::mcp::McpTool {
            name: "demo.fetch".into(),
            raw_name: "fetch".into(),
            description: "fetch things".into(),
            schema: serde_json::json!({"type": "object"}),
        }],
    );
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        mcp: vec![server],
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        Scripted::new(vec![]),
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "hello".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    // the inventory reports the server ok with its cached tool…
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Inventory { mcp, tools, .. }
                if mcp.iter().any(|s| s.name == "demo" && s.ok && s.tools == 1)
                    && tools.iter().any(|t| t == "demo__fetch")
        )),
        "cached tools must register without the server: {events:?}"
    );
    // …and the provenance is visible
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("from cache"))),
        "cache note surfaces: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without a cache entry the same dead server reports `ok: false` —
/// the cache is the only thing standing between slow and stuck.
#[tokio::test]
async fn mcp_cache_miss_reports_the_dead_server() {
    let _guard = ka_engine::mcp_cache::test_support::cache_guard();
    let dir = tmp_dir("mcp-cache-miss");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        mcp: vec![ka_engine::mcp::McpServerConfig {
            name: "demo".into(),
            command: Some("/nonexistent-ka-mcp-server".into()),
            args: Vec::new(),
            env: std::collections::HashMap::new(),
            url: None,
            headers: Vec::new(),
        }],
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        Scripted::new(vec![]),
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "hello".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Inventory { mcp, .. } if mcp.iter().any(|s| s.name == "demo" && !s.ok)
        )),
        "no cache, dead server: ok must be false: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// --------------------------------------------- hill-climbability

/// A plan-mode turn earns a fast-role hill-climbability rating; below
/// the gate the harness pushes for a verifiable objective.
#[tokio::test]
async fn plan_goal_below_hill_climb_gate_gets_pushback() {
    let dir = tmp_dir("hill-low");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        mode: Some(ka_protocol::Mode::Readonly),
        roles: ka_engine::config::Roles {
            default: None,
            fast: Some("test/m".into()),
        },
        ..Default::default()
    };
    // rounds: the plan reply, the auto-title call, then the rating
    let speaker = Scripted::new(vec![]);
    speaker.texts.lock().extend([
        "a numbered plan".into(),
        "nice app plan".into(),
        "40".into(),
    ]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "make the app feel nicer".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Note { message }
                if message.contains("hill-climbability 40/100")
                    && message.contains("verifiable objective")
        )),
        "low score must push back: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Non-plan modes never rate (no extra model call), and a high score
/// is informational only.
#[tokio::test]
async fn hill_climb_rates_only_plan_mode() {
    let dir = tmp_dir("hill-mode");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        roles: ka_engine::config::Roles {
            default: None,
            fast: Some("test/m".into()),
        },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "make the app feel nicer".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("hill-climbability"))),
        "free mode must not rate: {events:?}"
    );
    // two speaker rounds: the turn itself + the auto-title (fast role);
    // the rating never runs outside plan mode
    assert_eq!(speaker.requests.lock().len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Live steering (gap G14): a `Steer` sent while a tool runs rides the
/// history at the tool-batch boundary — the next model request ends
/// with the tool result followed by the steer as a user message, a
/// `steer delivered` note surfaces, and nothing converts into a queued
/// follow-on turn (exactly one TurnFinished + one Idle).
#[tokio::test]
async fn steer_during_tool_batch_rides_the_next_request() {
    let dir = tmp_dir("steer-live");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        mode: Some(ka_protocol::Mode::Allow),
        ..Default::default()
    };
    let speaker = Scripted::new(vec![("bash", serde_json::json!({"command": "sleep 0.25"}))]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );

    handle
        .commands
        .send(Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "go".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("engine alive");
    let mut events = Vec::new();
    loop {
        let e = tokio::time::timeout(Duration::from_secs(15), handle.events.recv())
            .await
            .expect("event within 15s")
            .expect("engine alive");
        let idle = matches!(e, Event::Idle);
        if matches!(e, Event::CallStarted { .. }) {
            // the tool sleeps 250 ms: the steer lands mid-batch
            handle
                .commands
                .send(Command::Steer {
                    text: "use metric units".into(),
                })
                .await
                .expect("engine alive");
        }
        events.push(e);
        if idle {
            break;
        }
    }
    // quiet drain
    while let Ok(Some(e)) =
        tokio::time::timeout(Duration::from_millis(300), handle.events.recv()).await
    {
        events.push(e);
    }

    assert!(
        events.iter().any(|e| matches!(e, Event::Note { message }
            if message == "steer delivered: use metric units")),
        "the delivery note surfaces: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::TurnFinished { .. }))
            .count(),
        1,
        "no queued follow-on turn: {events:?}"
    );
    let requests = speaker.requests.lock();
    assert_eq!(requests.len(), 2, "call round + final round");
    let second = requests.last().unwrap();
    let steer_msg = second
        .iter()
        .rev()
        .find(|m| m.role == TurnRole::User && m.content.contains("use metric units"))
        .expect("the steer rides the second request as a user message");
    assert!(
        steer_msg
            .content
            .starts_with("[operator, while you were working]"),
        "the steer is framed as operator input: {}",
        steer_msg.content
    );
    assert!(
        second.iter().any(|m| m.role == TurnRole::Tool),
        "the tool result precedes the steer: {second:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A speaker that streams its answer and then waits for the test to
/// release it before finishing — the window where a steer lands after
/// the model has already produced the closing (zero-tool-call) round.
struct GatedAnswer {
    gate: tokio::sync::Semaphore,
    first: std::sync::atomic::AtomicBool,
    requests: parking_lot::Mutex<Vec<Vec<TurnMessage>>>,
}

impl GatedAnswer {
    fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            gate: tokio::sync::Semaphore::new(0),
            first: std::sync::atomic::AtomicBool::new(true),
            requests: parking_lot::Mutex::new(Vec::new()),
        })
    }
}

impl Speaker for GatedAnswer {
    fn speak<'a>(
        &'a self,
        req: SpeakRequest,
        out: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> SpeakFuture<'a> {
        Box::pin(async move {
            self.requests.lock().push(req.messages.clone());
            let first = self.first.swap(false, std::sync::atomic::Ordering::SeqCst);
            out.send(StreamEvent::Text("the answer".into())).await.ok();
            if first {
                // hold the closing round open until the test steered:
                // the steer arrives after the model answered
                let _ = self.gate.acquire().await;
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

/// A steer that arrives during the closing zero-tool-call round has no
/// batch boundary left to ride: the engine must settle it as a queued
/// follow-on turn on the same strand — never leave it in `state.steers`
/// to be replayed into a later turn as if it had been seen live.
#[tokio::test]
async fn steer_during_final_answer_becomes_a_queued_turn() {
    let dir = tmp_dir("steer-late");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        mode: Some(ka_protocol::Mode::Allow),
        ..Default::default()
    };
    let speaker = GatedAnswer::new();
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    handle
        .commands
        .send(Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "go".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("engine alive");
    let mut events = Vec::new();
    loop {
        let e = tokio::time::timeout(Duration::from_secs(15), handle.events.recv())
            .await
            .expect("event within 15s")
            .expect("engine alive");
        // the answer delta means the closing round is streaming: steer
        // now, then let the model finish
        if matches!(e, Event::Delta { .. }) {
            handle
                .commands
                .send(Command::Steer {
                    text: "use metric units".into(),
                })
                .await
                .expect("engine alive");
            speaker.gate.add_permits(1);
        }
        let idle = matches!(e, Event::Idle);
        events.push(e);
        if idle {
            break;
        }
    }
    while let Ok(Some(e)) =
        tokio::time::timeout(Duration::from_millis(300), handle.events.recv()).await
    {
        events.push(e);
    }

    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::TurnFinished { .. }))
            .count(),
        2,
        "the late steer must run as a queued follow-on turn: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Note { message }
                if message.starts_with("steer delivered"))),
        "a steer with no batch left is not delivered live: {events:?}"
    );
    let requests = speaker.requests.lock();
    assert_eq!(
        requests.len(),
        2,
        "answer round + queued follow-on: {requests:?}"
    );
    let steer_msg = requests
        .last()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m.role == TurnRole::User)
        .expect("the follow-on turn carries the steer as its prompt");
    assert_eq!(
        steer_msg.content, "use metric units",
        "the steer runs as a plain queued prompt, not live-operator framing"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A steer sent while the engine is idle targets the turn it arrived
/// around; attaching another strand must drop it instead of replaying
/// it into that strand's next tool batch.
#[tokio::test]
async fn steer_left_over_at_strand_attach_is_not_reinjected() {
    let dir = tmp_dir("steer-attach");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        mode: Some(ka_protocol::Mode::Allow),
        ..Default::default()
    };
    let speaker = Scripted::new(vec![("bash", serde_json::json!({"command": "true"}))]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    handle
        .commands
        .send(Command::Steer {
            text: "stale steer".into(),
        })
        .await
        .expect("engine alive");
    let _ = settle(
        &mut handle,
        Command::SwitchStrand { id: "new".into() },
        |e| matches!(e, Event::SessionInfo { .. } | Event::Error { .. }),
    )
    .await;
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "go".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;

    let requests = speaker.requests.lock();
    assert!(
        requests
            .iter()
            .all(|round| round.iter().all(|m| !m.content.contains("stale steer"))),
        "a pre-switch steer must not ride the new strand's batch boundary: {requests:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("stale steer"))),
        "no delivery note for a dropped steer: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
