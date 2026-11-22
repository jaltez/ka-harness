//! Batch-4 feature contracts, end-to-end over the real `spawn_full`
//! loop (mirroring `engine_e2e.rs`): SES-01 session diff, SES-09
//! context edit, CTX-01 compact-from, CTX-03 side question, CTX-05
//! goal mode, CTX-06 loop mode, CTX-08 prewalk, CTX-10 memory
//! consolidation. If one of these breaks, a user-visible batch-4
//! command broke — deliberate changes must update this file and the
//! docs together.

#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::collections::VecDeque;
use std::time::Duration;

use ka_dialect::speaker::{SpeakFuture, SpeakRequest, Speaker, StreamEvent, ToolCall, TurnRole};
use ka_protocol::{Command, Event, RecordId, Stop};

fn test_catalog() -> ka_dialect::Catalog {
    ka_dialect::Catalog::parse(
        "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
    )
    .unwrap()
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-l4-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Strand/snapshot storage redirect for tests that read strand files
/// or snapshot blobs. `set_data_dir_override` is thread-local, and a
/// `#[tokio::test]` runtime is current-thread — the spawned engine
/// task shares this thread, so the override reaches it. Each test
/// clears it when done.
fn data_dir_guard(tag: &str) -> std::path::PathBuf {
    let dir = tmp_dir(&format!("data-{tag}"));
    ka_strand::set_data_dir_override(Some(dir.clone()));
    dir
}

/// The strand file of the newest session for `cwd`.
fn latest_strand(cwd: &std::path::Path) -> ka_strand::StrandFile {
    let summary = ka_strand::latest(cwd)
        .expect("strand listing works")
        .expect("the session persisted a strand");
    ka_strand::StrandFile::open(&summary.path).expect("strand opens")
}

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
    // quiet drain: trailing events (Idle etc.) land before the next cmd
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

fn notes(events: &[Event]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Note { message } => Some(message.as_str()),
            _ => None,
        })
        .collect()
}

/// A speaker that emits one scripted tool call per request round, then
/// a final text reply, recording every request (mirrors engine_e2e).
struct Scripted {
    calls: parking_lot::Mutex<VecDeque<(&'static str, serde_json::Value)>>,
    texts: parking_lot::Mutex<VecDeque<String>>,
    requests: parking_lot::Mutex<Vec<Vec<ka_dialect::speaker::TurnMessage>>>,
}

impl Scripted {
    fn new(calls: Vec<(&'static str, serde_json::Value)>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            calls: parking_lot::Mutex::new(calls.into()),
            texts: parking_lot::Mutex::new(VecDeque::new()),
            requests: parking_lot::Mutex::new(Vec::new()),
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

fn user_texts(request: &[ka_dialect::speaker::TurnMessage]) -> Vec<String> {
    request
        .iter()
        .filter(|m| m.role == TurnRole::User)
        .map(|m| m.content.clone())
        .collect()
}

// --------------------------------------------- SES-01 session diff

/// `/diff` with no session mutations: the no-changes note.
#[tokio::test]
async fn session_diff_without_changes_notes_no_changes() {
    let dir = tmp_dir("diff-none");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(
        &mut handle,
        Command::SessionDiff,
        |e| matches!(e, Event::Note { message } if message.starts_with("session diff")),
    )
    .await;
    assert!(
        notes(&events).contains(&"session diff: no changes"),
        "{events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A session edit shows up as a unified diff: the snapshot journal's
/// pre-mutation original vs current disk, full diff on the spill
/// store, summary + pointer on the note.
#[tokio::test]
async fn session_diff_reports_mutations_with_spill_pointer() {
    let dir = tmp_dir("diff-edit");
    std::fs::write(dir.join("v.txt"), "alpha\nsecond\n").unwrap();
    let _data = data_dir_guard("diff-edit");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        ..Default::default()
    };
    // the edit hand refuses to touch a file the session never read:
    // round 1 reads it, round 2 edits
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
    settle(
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
    let events = settle(
        &mut handle,
        Command::SessionDiff,
        |e| matches!(e, Event::Note { message } if message.starts_with("session diff")),
    )
    .await;
    let all_notes = notes(&events);
    let note = all_notes
        .iter()
        .find(|n| n.starts_with("session diff"))
        .expect("diff note");
    assert!(
        note.starts_with("session diff: 1 file(s), 2 +/- lines — full: spill://"),
        "{note}"
    );
    // the excerpt carries the unified body with both sides
    assert!(note.contains("-alpha"), "{note}");
    assert!(note.contains("+ALPHA"), "{note}");
    assert!(note.contains("--- a/v.txt"), "{note}");
    let _ = std::fs::remove_dir_all(&dir);
}

// --------------------------------------------- SES-09 context edit

/// ContextEdit validates the target against the current window, then
/// applies the drop/replace live and records it on the strand.
#[tokio::test]
async fn context_edit_drops_and_replaces_window_messages() {
    let dir = tmp_dir("ctx-edit");
    let _data = data_dir_guard("ctx-edit");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        ..Default::default()
    };
    let speaker = Scripted::new(vec![]);
    speaker
        .texts
        .lock()
        .extend(["first reply".into(), "second reply".into()]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "hello secret marker".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    // resolve the user record id from the strand
    let records = latest_strand(&dir).records().to_vec();
    let user_id = records
        .iter()
        .find_map(|r| match r {
            ka_strand::Record::Message {
                id,
                role: ka_strand::Role::User,
                content,
                ..
            } if content.contains("secret marker") => Some(id.clone()),
            _ => None,
        })
        .expect("user record in strand");

    // unknown target: refused with a note, nothing recorded
    let events = settle(
        &mut handle,
        Command::ContextEdit {
            target: RecordId("rBogus".into()),
            new_content: None,
        },
        |e| matches!(e, Event::Note { message } if message.contains("context edit")),
    )
    .await;
    assert!(
        notes(&events)[0].contains("not in the current context window"),
        "{events:?}"
    );

    // drop: live window loses the message
    let events = settle(
        &mut handle,
        Command::ContextEdit {
            target: user_id.clone(),
            new_content: None,
        },
        |e| matches!(e, Event::Note { message } if message.contains("context edit")),
    )
    .await;
    assert!(
        notes(&events)[0].contains(&format!("context edit: {} dropped", user_id.0)),
        "{events:?}"
    );

    // the next request no longer carries the dropped prompt
    settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "second prompt".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    let requests: Vec<_> = speaker.requests.lock().iter().cloned().collect();
    assert!(
        !user_texts(&requests[requests.len() - 1])
            .iter()
            .any(|t| t.contains("secret marker")),
        "dropped message left the window: {requests:?}"
    );

    // replace: the strand keeps the append-only record; replay parity
    // comes from ka-strand's fold
    let records = latest_strand(&dir).records().to_vec();
    assert!(
        records.iter().any(
            |r| matches!(r, ka_strand::Record::ContextEdit { target, .. } if *target == user_id)
        ),
        "ContextEdit record appended: {records:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A replace edit reaches the next model request with the new content.
#[tokio::test]
async fn context_edit_replace_feeds_next_request() {
    let dir = tmp_dir("ctx-replace");
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
        speaker.clone(),
    );
    settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "original words".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    let records = latest_strand(&dir).records().to_vec();
    let user_id = records
        .iter()
        .find_map(|r| match r {
            ka_strand::Record::Message {
                id,
                role: ka_strand::Role::User,
                ..
            } => Some(id.clone()),
            _ => None,
        })
        .expect("user record");
    let events = settle(
        &mut handle,
        Command::ContextEdit {
            target: user_id,
            new_content: Some("REPLACED CONTENT".into()),
        },
        |e| matches!(e, Event::Note { message } if message.contains("context edit")),
    )
    .await;
    assert!(
        notes(&events)[0].contains("replaced (16 chars)"),
        "{events:?}"
    );
    settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "follow-up".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    let requests: Vec<_> = speaker.requests.lock().iter().cloned().collect();
    let last = requests.last().expect("at least one captured request");
    assert!(
        user_texts(last)
            .iter()
            .any(|t| t.contains("REPLACED CONTENT")),
        "replacement must ride the window: {last:?}"
    );
    assert!(
        !user_texts(last)
            .iter()
            .any(|t| t.contains("original words")),
        "old content must be gone: {last:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// --------------------------------------------- CTX-01 compact-from

/// CompactFrom rewinds AND keeps the abandoned branch as a
/// `x.ka.branch_summary` Custom record after the Rewind record, with
/// `focus` steering the ≤6-sentence summary.
#[tokio::test]
async fn compact_from_keeps_abandoned_branch_as_summary() {
    let dir = tmp_dir("compact-from");
    let _data = data_dir_guard("compact-from");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        roles: ka_engine::config::Roles {
            default: None,
            fast: Some("test/m".into()),
            extra: Default::default(),
        },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![]);
    speaker.texts.lock().extend([
        "did A".into(),
        "branch title".into(),
        "did B".into(),
        "kept summary sentence".into(),
    ]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    for prompt in ["work on A", "work on B"] {
        settle(
            &mut handle,
            Command::Prompt {
                allowed_tools: None,
                model: None,
                text: prompt.into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            },
            on_idle,
        )
        .await;
    }
    let events = settle(
        &mut handle,
        Command::CompactFrom {
            turns: 1,
            focus: Some("the API notes".into()),
        },
        |e| matches!(e, Event::Note { message } if message.starts_with("compact-from")),
    )
    .await;
    assert!(
        notes(&events)
            .iter()
            .any(|n| n
                .contains("compact-from: rewound 1 turn(s) — kept summary: kept summary sentence")),
        "{events:?}"
    );
    // the summary request steered by focus, capped at 6 sentences
    let requests: Vec<_> = speaker.requests.lock().iter().cloned().collect();
    let summary_req = requests.last().expect("summary request captured");
    let text = user_texts(summary_req).join("\n");
    assert!(text.contains("at most 6 sentences"), "{text}");
    assert!(
        text.contains("Focus the summary on: the API notes"),
        "{text}"
    );
    assert!(text.contains("work on B"), "tail view rides: {text}");
    assert!(
        !text.contains("work on A"),
        "only the rewound branch rides: {text}"
    );
    // strand order: the Custom record sits AFTER the Rewind marker
    let records = latest_strand(&dir).records().to_vec();
    let rewind_idx = records
        .iter()
        .position(|r| matches!(r, ka_strand::Record::Rewind { .. }))
        .expect("Rewind record");
    let summary_idx = records
        .iter()
        .position(|r| matches!(r, ka_strand::Record::Custom { ns, .. } if ns == ka_strand::BRANCH_SUMMARY_NS))
        .expect("branch summary record");
    assert!(
        summary_idx > rewind_idx,
        "summary after rewind: {records:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// kept == 0 (rewind to the boundary): Boundary record, no summary
/// call, terse note.
#[tokio::test]
async fn compact_from_to_boundary_skips_summary() {
    let dir = tmp_dir("compact-zero");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        roles: ka_engine::config::Roles {
            default: None,
            fast: Some("test/m".into()),
            extra: Default::default(),
        },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![]);
    speaker
        .texts
        .lock()
        .extend(["reply".into(), "title".into()]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "only turn".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    let before = speaker.requests.lock().len();
    let events = settle(
        &mut handle,
        Command::CompactFrom {
            turns: 1,
            focus: None,
        },
        |e| matches!(e, Event::Note { message } if message.starts_with("compact-from")),
    )
    .await;
    assert!(
        notes(&events).contains(&"compact-from: rewound 1 turn(s)".to_string().as_str()),
        "{events:?}"
    );
    // no summary call, no branch summary record, a Boundary instead
    assert_eq!(speaker.requests.lock().len(), before, "no fast call");
    let records = latest_strand(&dir).records().to_vec();
    assert!(
        records
            .iter()
            .any(|r| matches!(r, ka_strand::Record::Boundary { .. })),
        "{records:?}"
    );
    assert!(
        !records
            .iter()
            .any(|r| matches!(r, ka_strand::Record::Custom { ns, .. } if ns == ka_strand::BRANCH_SUMMARY_NS)),
        "{records:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// --------------------------------------------- CTX-03 side question

/// A side question rides the fast role alone: one request, one
/// message, no tools; the answer arrives as a `btw:` note and never
/// enters the main transcript.
#[tokio::test]
async fn side_question_rides_fast_role_only() {
    let dir = tmp_dir("side-q");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        roles: ka_engine::config::Roles {
            default: None,
            fast: Some("test/m".into()),
            extra: Default::default(),
        },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![]);
    speaker
        .texts
        .lock()
        .extend(["the answer".into(), "main reply".into()]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    let events = settle(
        &mut handle,
        Command::SideQuestion {
            text: "what is x?".into(),
        },
        |e| matches!(e, Event::Note { message } if message.starts_with("btw:")),
    )
    .await;
    assert!(notes(&events).contains(&"btw: the answer"), "{events:?}");
    // exactly one request, one user message (question + minimal hint),
    // zero tools
    {
        let requests = speaker.requests.lock();
        assert_eq!(requests.len(), 1, "no main-turn request yet: {requests:?}");
        assert_eq!(requests[0].len(), 1, "single-message request: {requests:?}");
        let text = &requests[0][0].content;
        assert!(text.contains("what is x?"), "{text}");
        assert!(text.contains("cwd"), "minimal session hint: {text}");
    }
    // the answer never enters the main transcript
    settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "main prompt".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    let requests = speaker.requests.lock();
    let main_turn = &requests[requests.len() - 1];
    assert!(
        !user_texts(main_turn)
            .iter()
            .any(|t| t.contains("the answer")),
        "side answer leaked into the transcript: {main_turn:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without a fast role the side question degrades to a note.
#[tokio::test]
async fn side_question_without_fast_role_unavailable() {
    let dir = tmp_dir("side-q-none");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(
        &mut handle,
        Command::SideQuestion {
            text: "anything".into(),
        },
        |e| matches!(e, Event::Note { .. }),
    )
    .await;
    assert!(
        notes(&events).contains(&"side question unavailable"),
        "{events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// --------------------------------------------- CTX-05 goal mode

/// The judge runs once on the settle of a Done turn; YES drops the
/// goal (no judge on the next settle).
#[tokio::test]
async fn goal_judge_yes_drops_goal() {
    let dir = tmp_dir("goal-yes");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        roles: ka_engine::config::Roles {
            default: None,
            fast: Some("test/m".into()),
            extra: Default::default(),
        },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![]);
    speaker
        .texts
        .lock()
        .extend(["working".into(), "YES".into()]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    let events = settle(
        &mut handle,
        Command::Goal {
            action: ka_protocol::GoalAction::Set {
                condition: "the build passes".into(),
                budget_usd: None,
            },
        },
        |e| matches!(e, Event::Note { message } if message.starts_with("goal set")),
    )
    .await;
    assert!(
        notes(&events).contains(&"goal set: the build passes"),
        "{events:?}"
    );
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
    assert!(
        notes(&events)
            .iter()
            .any(|n| n.contains("goal met: the build passes")),
        "met note: {events:?}"
    );
    // r0 turn, r1 judge (goal + YES-or-NO prompt), r2 auto-title
    let requests: Vec<_> = speaker.requests.lock().iter().cloned().collect();
    assert_eq!(requests.len(), 3, "{requests:?}");
    let judge = user_texts(&requests[1]).join("\n");
    assert!(judge.contains("<goal>"), "{judge}");
    assert!(judge.contains("the build passes"), "{judge}");
    assert!(judge.contains("YES or NO"), "{judge}");
    // goal dropped: the next turn earns no judge call
    settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "more".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    assert_eq!(speaker.requests.lock().len(), 4, "turn only, no judge");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A NO verdict keeps the goal armed and the auto-poke pulls toward
/// the condition: the poke prompt is goal-prefixed.
#[tokio::test]
async fn goal_no_transforms_the_auto_poke() {
    let dir = tmp_dir("goal-poke");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        roles: ka_engine::config::Roles {
            default: None,
            fast: Some("test/m".into()),
            extra: Default::default(),
        },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![(
        "todo",
        serde_json::json!({"items": [{"text": "pending item", "state": "pending"}]}),
    )]);
    speaker.texts.lock().extend([
        "partial".into(),
        "NO".into(),
        "title".into(),
        "poke reply".into(),
    ]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    settle(
        &mut handle,
        Command::Goal {
            action: ka_protocol::GoalAction::Set {
                condition: "all tests green".into(),
                budget_usd: None,
            },
        },
        |e| matches!(e, Event::Note { message } if message.starts_with("goal set")),
    )
    .await;
    settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "start".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    // the judge ran right after the turn settle; the poke turn is
    // goal-prefixed. Locate both by content — the turn's round count
    // is an implementation detail.
    let requests = speaker.requests.lock();
    let judge_idx = requests
        .iter()
        .position(|r| user_texts(r).iter().any(|t| t.contains("YES or NO")))
        .expect("judge request recorded");
    let judge = user_texts(&requests[judge_idx]).join("\n");
    assert!(judge.contains("<goal>"), "{judge}");
    assert!(judge.contains("all tests green"), "{judge}");
    assert!(
        !judge.contains("keep working"),
        "judge prompt carries no poke: {judge}"
    );
    let poke_idx = requests
        .iter()
        .position(|r| user_texts(r).iter().any(|t| t.contains("ka auto-poke")))
        .expect("poke turn recorded");
    let poke = user_texts(&requests[poke_idx]).join("\n");
    assert!(
        poke.contains("goal: all tests green — keep working toward it"),
        "{poke}"
    );
    assert!(poke.contains("ka auto-poke"), "{poke}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Budget exhaustion auto-pauses the goal at settle: the judge never
/// fires and the note names the ceiling.
#[tokio::test]
async fn goal_budget_exhaustion_pauses_without_judging() {
    let dir = tmp_dir("goal-budget");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        roles: ka_engine::config::Roles {
            default: None,
            fast: Some("test/m".into()),
            extra: Default::default(),
        },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![]);
    speaker
        .texts
        .lock()
        .extend(["reply".into(), "title".into()]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    settle(
        &mut handle,
        Command::Goal {
            action: ka_protocol::GoalAction::Set {
                condition: "done".into(),
                budget_usd: Some(0.0),
            },
        },
        |e| matches!(e, Event::Note { message } if message.starts_with("goal set")),
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
    assert!(
        notes(&events)
            .iter()
            .any(|n| n.starts_with("goal budget exhausted")),
        "{events:?}"
    );
    // r0 turn, r1 auto-title — the judge never ran
    assert_eq!(
        speaker.requests.lock().len(),
        2,
        "{requests:?}",
        requests = speaker.requests.lock()
    );
    // resume works; drop ends the lifecycle
    let events = settle(
        &mut handle,
        Command::Goal {
            action: ka_protocol::GoalAction::Resume,
        },
        |e| matches!(e, Event::Note { message } if message.starts_with("goal resumed")),
    )
    .await;
    assert!(
        notes(&events)
            .iter()
            .any(|n| n.contains("goal resumed: done")),
        "{events:?}"
    );
    let events = settle(
        &mut handle,
        Command::Goal {
            action: ka_protocol::GoalAction::Drop,
        },
        |e| matches!(e, Event::Note { message } if message.starts_with("goal dropped")),
    )
    .await;
    assert!(notes(&events).contains(&"goal dropped"), "{events:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// --------------------------------------------- CTX-06 loop mode

/// An armed loop fires the task as a normal prompt cycle right after
/// the arming command settles (None interval = as soon as the engine
/// settles), and Off stops the cycle.
#[tokio::test]
async fn loop_mode_fires_and_disarms() {
    let dir = tmp_dir("loop");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(
        &mut handle,
        Command::Loop {
            action: ka_protocol::LoopAction::Set {
                task: "run the checks".into(),
                interval_secs: None,
            },
        },
        |e| matches!(e, Event::Note { message } if message.starts_with("loop armed")),
    )
    .await;
    assert!(
        notes(&events).contains(&"loop armed: run the checks"),
        "{events:?}"
    );
    // the loop fired a full (canned) turn cycle by itself
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TurnFinished {
                stop: Stop::Done,
                ..
            }
        )),
        "loop fired a turn: {events:?}"
    );
    // Off: the cycle stops
    let events = settle(
        &mut handle,
        Command::Loop {
            action: ka_protocol::LoopAction::Off,
        },
        |e| matches!(e, Event::Note { message } if message.starts_with("loop off")),
    )
    .await;
    assert!(notes(&events).contains(&"loop off"), "{events:?}");
    // nothing fires afterwards: quiet for 1.5s
    let quiet = tokio::time::timeout(Duration::from_millis(1500), handle.events.recv()).await;
    assert!(
        matches!(quiet, Err(_) | Ok(None)),
        "unexpected event after loop off: {quiet:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// --------------------------------------------- CTX-08 prewalk

/// With `[context] prewalk` on, the first prompt of a fresh strand
/// runs the walk + fast-role brief first; the brief rides the context
/// as a system-note and the prewalk fires exactly once.
#[tokio::test]
async fn prewalk_runs_once_before_the_first_turn() {
    let dir = tmp_dir("prewalk");
    std::fs::write(dir.join("README.md"), "# demo project\n").unwrap();
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        roles: ka_engine::config::Roles {
            default: None,
            fast: Some("test/m".into()),
            extra: Default::default(),
        },
        context: ka_engine::config::Context {
            prewalk: Some(true),
            ..Default::default()
        },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![]);
    speaker.texts.lock().extend([
        "- it is a demo service\n- rust workspace".into(),
        "turn reply".into(),
        "session title".into(),
        "second turn reply".into(),
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
            text: "hello".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    assert!(notes(&events).contains(&"prewalk ready"), "{events:?}");
    // r0 prewalk (single message, zero tools, carries the scan),
    // r1 the real turn with the brief injected as a system-note,
    // r2 the auto-title
    let requests: Vec<_> = speaker.requests.lock().iter().cloned().collect();
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert_eq!(requests[0].len(), 1, "prewalk is one message");
    let prewalk_prompt = &requests[0][0].content;
    assert!(
        prewalk_prompt.contains("workspace-scan"),
        "{prewalk_prompt}"
    );
    assert!(
        prewalk_prompt.contains("README head"),
        "scan carries the readme head: {prewalk_prompt}"
    );
    let turn_user = user_texts(&requests[1]).join("\n");
    assert!(
        turn_user.contains("Session prewalk"),
        "brief rides the turn context: {turn_user}"
    );
    assert!(
        turn_user.contains("it is a demo service"),
        "the brief content rides: {turn_user}"
    );
    // second prompt: no second prewalk (turn round only)
    settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "again".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    assert_eq!(speaker.requests.lock().len(), 4, "no second prewalk");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Prewalk off: no orientation request, no note.
#[tokio::test]
async fn prewalk_off_by_default() {
    let dir = tmp_dir("prewalk-off");
    std::fs::write(dir.join("README.md"), "# demo\n").unwrap();
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        roles: ka_engine::config::Roles {
            default: None,
            fast: Some("test/m".into()),
            extra: Default::default(),
        },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![]);
    speaker
        .texts
        .lock()
        .extend(["reply".into(), "title".into()]);
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
            text: "hello".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        },
        on_idle,
    )
    .await;
    assert!(!notes(&events).iter().any(|n| n.contains("prewalk")));
    // r0 turn, r1 title — no orientation request in front
    let requests = speaker.requests.lock();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        !user_texts(&requests[0])
            .iter()
            .any(|t| t.contains("workspace-scan")),
        "no prewalk request: {requests:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// --------------------------------------------- CTX-10 memory consolidate

/// Consolidation: fast role rewrites MEMORY.md from the inbox, the
/// inbox empties, the note counts the line diff.
#[tokio::test]
async fn memory_consolidate_rewrites_memory_and_empties_inbox() {
    let dir = tmp_dir("mem-consolidate");
    let (_trust_file, _guard) = ka_engine::trust::test_support::trust_guard();
    ka_engine::trust::approve(&dir);
    std::fs::write(dir.join("MEMORY.md"), "old line\n").unwrap();
    std::fs::create_dir_all(dir.join(".ka/memory")).unwrap();
    std::fs::write(
        dir.join(".ka/memory/inbox.md"),
        "- [1] note one\n- [2] note two\n",
    )
    .unwrap();
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        roles: ka_engine::config::Roles {
            default: None,
            fast: Some("test/m".into()),
            extra: Default::default(),
        },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![]);
    speaker
        .texts
        .lock()
        .extend(["consolidated: note one\nconsolidated: note two".into()]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    let events = settle(
        &mut handle,
        Command::MemoryConsolidate,
        |e| matches!(e, Event::Note { message } if message.contains("memory consolidated")),
    )
    .await;
    // +2 (new lines) -1 (the replaced old line)
    assert!(
        notes(&events).contains(&"memory consolidated (+2 -1 lines)"),
        "{events:?}"
    );
    let memory = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    assert_eq!(memory, "consolidated: note one\nconsolidated: note two\n");
    let inbox = std::fs::read_to_string(dir.join(".ka/memory/inbox.md")).unwrap();
    assert!(inbox.trim().is_empty(), "inbox emptied: {inbox:?}");
    // the consolidation request carried both files
    let requests = speaker.requests.lock();
    let text = user_texts(&requests[requests.len() - 1]).join("\n");
    assert!(text.contains("note one"), "{text}");
    assert!(text.contains("old line"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Trust gate: an untrusted project consolidates nothing.
#[tokio::test]
async fn memory_consolidate_gates_on_trust() {
    let dir = tmp_dir("mem-untrusted");
    let (_trust_file, _guard) = ka_engine::trust::test_support::trust_guard();
    std::fs::write(dir.join("MEMORY.md"), "keep me\n").unwrap();
    std::fs::create_dir_all(dir.join(".ka/memory")).unwrap();
    std::fs::write(dir.join(".ka/memory/inbox.md"), "- [1] staged\n").unwrap();
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(
        &mut handle,
        Command::MemoryConsolidate,
        |e| matches!(e, Event::Note { message } if message.contains("memory")),
    )
    .await;
    assert!(notes(&events)[0].contains("not trusted"), "{events:?}");
    assert_eq!(
        std::fs::read_to_string(dir.join("MEMORY.md")).unwrap(),
        "keep me\n",
        "untrusted write refused"
    );
    assert!(
        !std::fs::read_to_string(dir.join(".ka/memory/inbox.md"))
            .unwrap()
            .trim()
            .is_empty(),
        "inbox untouched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Empty inbox: nothing to consolidate.
#[tokio::test]
async fn memory_consolidate_empty_inbox_notes() {
    let dir = tmp_dir("mem-empty");
    let (_trust_file, _guard) = ka_engine::trust::test_support::trust_guard();
    ka_engine::trust::approve(&dir);
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(
        &mut handle,
        Command::MemoryConsolidate,
        |e| matches!(e, Event::Note { message } if message.contains("memory")),
    )
    .await;
    assert!(notes(&events)[0].contains("inbox is empty"), "{events:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
