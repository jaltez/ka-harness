//! EXT-12 agent teams, end-to-end over the real engine loop: the
//! lifecycle spawn→send→status→read→dismiss over a scripted (SSE)
//! member speaker, multi-turn context accumulation through the member's
//! own strand, cap and grammar enforcement, and the /tasks roster rows.
//! If one of these breaks, a user-visible team action broke.

#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use ka_dialect::Wire;
use ka_dialect::speaker::{
    SpeakFuture, SpeakRequest, Speaker, StreamEvent, ToolCall, TurnMessage, TurnRole,
};
use ka_engine::hands::Hand as _;
use ka_engine::hands::teams::{MemberSpeakers, TEAM_NOTE_PREFIX, TeamTable, TeamsHand};
use ka_protocol::{Command, Event, Stop};

fn test_catalog() -> ka_dialect::Catalog {
    ka_dialect::Catalog::parse(
        "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
    )
    .unwrap()
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-team-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Strand storage redirect (the member strands must land in a temp
/// data root, not the developer's real one). Thread-local — a
/// `#[tokio::test]` runtime is current-thread, so the spawned member
/// engines share the override.
fn data_dir_guard(tag: &str) -> std::path::PathBuf {
    let dir = tmp_dir(&format!("data-{tag}"));
    ka_strand::set_data_dir_override(Some(dir.clone()));
    dir
}

/// A scripted member speaker: replies the next shared text (or "done"
/// once the queue runs dry) and records every request it receives —
/// the multi-turn accumulation probe.
#[derive(Default)]
struct MemberScript {
    texts: Arc<parking_lot::Mutex<VecDeque<String>>>,
    requests: parking_lot::Mutex<Vec<Vec<TurnMessage>>>,
}

impl Speaker for MemberScript {
    fn speak<'a>(
        &'a self,
        req: SpeakRequest,
        out: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> SpeakFuture<'a> {
        Box::pin(async move {
            self.requests.lock().push(req.messages.clone());
            let text = {
                let next = self.texts.lock().pop_front();
                next.unwrap_or_else(|| "done".into())
            };
            out.send(StreamEvent::Text(text)).await.ok();
            out.send(StreamEvent::Finished {
                stop: Stop::Done,
                usage: ka_protocol::Usage::default(),
            })
            .await
            .ok();
        })
    }
}

/// The member-speaker factory: every spawned member gets its own
/// `MemberScript`, all fed from one shared reply queue; the instances
/// stay reachable for request assertions.
struct Factory {
    texts: Arc<parking_lot::Mutex<VecDeque<String>>>,
    instances: parking_lot::Mutex<Vec<Arc<MemberScript>>>,
}

impl Factory {
    fn new(texts: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            texts: Arc::new(parking_lot::Mutex::new(
                texts.iter().map(|t| t.to_string()).collect(),
            )),
            instances: parking_lot::Mutex::new(Vec::new()),
        })
    }

    fn speakers(self: &Arc<Self>) -> MemberSpeakers {
        let inner = Arc::clone(self);
        Arc::new(move || {
            let s = Arc::new(MemberScript {
                texts: Arc::clone(&inner.texts),
                requests: parking_lot::Mutex::new(Vec::new()),
            });
            inner.instances.lock().push(Arc::clone(&s));
            (Wire::OpenaiChat, s as Arc<dyn Speaker>)
        })
    }
}

fn hand_ctx(cwd: &std::path::Path) -> ka_engine::hands::HandContext {
    ka_engine::hands::HandContext {
        cwd: cwd.to_path_buf(),
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
        shell_env: Default::default(),
    }
}

fn team_table(
    max: u32,
    cwd: &std::path::Path,
    factory: Option<MemberSpeakers>,
) -> (TeamTable, tokio::sync::mpsc::Receiver<Event>) {
    let strand = ka_strand::StrandFile::create(cwd, None).unwrap();
    let (events, rx) = tokio::sync::mpsc::channel(64);
    // the member template carries the test model: members are full
    // engine sessions and only model turns use the scripted speaker
    let template = ka_engine::Config {
        model: Some("test/m".into()),
        ..Default::default()
    };
    let table = TeamTable::new(
        max,
        cwd,
        test_catalog(),
        template,
        std::sync::Arc::new(parking_lot::RwLock::new(ka_protocol::Mode::Allow)),
        events,
        &strand,
        factory,
    );
    (table, rx)
}

fn hand(table: &TeamTable) -> TeamsHand {
    TeamsHand::new(table.clone())
}

/// The first `team · ` note off the parent channel (the deterministic
/// turn-completion signal), with a loud timeout so a stuck member
/// fails instead of hanging.
async fn next_team_note(rx: &mut tokio::sync::mpsc::Receiver<Event>) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(Event::Note { message })) if message.starts_with(TEAM_NOTE_PREFIX) => {
                return message;
            }
            Ok(Some(_)) => continue,
            Ok(None) => panic!("parent event channel closed before the team note"),
            Err(_) => panic!("no team note within 15s"),
        }
    }
}

/// Full lifecycle over a real member engine: spawn (opening prompt
/// runs immediately) → completion note → status(done) → read →
/// send (second turn) → read → dismiss → dead, send refused.
#[tokio::test]
async fn team_lifecycle_spawn_send_status_read_dismiss() {
    let dir = tmp_dir("lifecycle");
    let _data = data_dir_guard("lifecycle");
    let factory = Factory::new(&["Survey done: 3 crates.", "README summary ready."]);
    let (table, mut notes) = team_table(8, &dir, Some(factory.speakers()));
    let h = hand(&table);
    let ctx = hand_ctx(&dir);

    // spawn: the member session is named <stem>/team/<name>
    let out = h
        .execute(
            &serde_json::json!({
                "action": "spawn",
                "name": "scout",
                "prompt": "survey the repo"
            }),
            &ctx,
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("session/team/scout"),
        "spawn names the member session: {}",
        out.content
    );

    // the opening turn completes with exactly one completion note
    let note = next_team_note(&mut notes).await;
    assert_eq!(note, "team · scout — Survey done: 3 crates.");

    // status: one done row with the turn counted
    let status = h
        .execute(&serde_json::json!({"action": "status"}), &ctx)
        .await;
    assert!(
        status.content.contains("team scout  done     turns 1"),
        "{}",
        status.content
    );

    // read returns the reply and settles done → idle
    let read = h
        .execute(
            &serde_json::json!({"action": "read", "name": "scout"}),
            &ctx,
        )
        .await;
    assert!(
        read.content.contains("Survey done: 3 crates."),
        "{}",
        read.content
    );
    let status = h
        .execute(&serde_json::json!({"action": "status"}), &ctx)
        .await;
    assert!(
        status.content.contains("team scout  idle  "),
        "read settles the member to idle: {}",
        status.content
    );

    // send: a SECOND prompt into the SAME session
    let out = h
        .execute(
            &serde_json::json!({
                "action": "send",
                "name": "scout",
                "message": "now summarize the README"
            }),
            &ctx,
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    let note = next_team_note(&mut notes).await;
    assert_eq!(note, "team · scout — README summary ready.");
    let read = h
        .execute(
            &serde_json::json!({"action": "read", "name": "scout"}),
            &ctx,
        )
        .await;
    assert!(read.content.contains("README summary ready."));

    // multi-turn accumulation: the member's SECOND request carried the
    // first turn's reply as assistant context (strand-carried, not
    // process memory — a fresh engine per member)
    let requests = {
        let instances = factory.instances.lock();
        assert_eq!(instances.len(), 1, "one member, one speaker");
        instances[0].requests.lock().clone()
    };
    assert_eq!(requests.len(), 2, "two turns, two requests");
    let second = &requests[1];
    assert!(
        second
            .iter()
            .any(|m| m.role == TurnRole::Assistant && m.content.contains("Survey done: 3 crates")),
        "the second turn sees the first reply: {:?}",
        second
            .iter()
            .map(|m| (m.role, &m.content))
            .collect::<Vec<_>>()
    );
    drop(requests);

    // the member strand lives in the data dir under its session name
    let strands = ka_strand::list(&dir).unwrap();
    let member_strand = strands
        .iter()
        .find(|s| s.name.as_deref() == Some("session/team/scout"))
        .expect("member strand named session/team/scout");
    assert!(
        member_strand.messages >= 4,
        "user, assistant, user, assistant on the strand: {}",
        member_strand.messages
    );

    // dismiss: the session ends cleanly, the row reads dead, and a
    // further send refuses
    let out = h
        .execute(
            &serde_json::json!({"action": "dismiss", "name": "scout"}),
            &ctx,
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("session/team/scout"),
        "{}",
        out.content
    );
    let status = h
        .execute(&serde_json::json!({"action": "status"}), &ctx)
        .await;
    assert!(
        status.content.contains("team scout  dead  "),
        "{}",
        status.content
    );
    let out = h
        .execute(
            &serde_json::json!({"action": "send", "name": "scout", "message": "hello?"}),
            &ctx,
        )
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("dismissed"),
        "the refusal explains the member is gone: {}",
        out.content
    );
    // the read still serves the last reply after dismissal
    let read = h
        .execute(
            &serde_json::json!({"action": "read", "name": "scout"}),
            &ctx,
        )
        .await;
    assert!(read.content.contains("README summary ready."));

    // a dismissed name is respawneable: the dead row holds no name or
    // slot (dismiss → respawn cycle must not refuse with "already
    // exists" or a full table)
    let out = h
        .execute(
            &serde_json::json!({
                "action": "spawn",
                "name": "scout",
                "prompt": "second survey"
            }),
            &ctx,
        )
        .await;
    assert!(
        !out.is_error,
        "respawn after dismiss must work: {}",
        out.content
    );

    ka_strand::set_data_dir_override(None);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&_data);
}

/// Grammar, duplicates, and caps: invalid names refuse, a duplicate
/// name refuses, and a full table refuses with the config knob named.
#[tokio::test]
async fn team_grammar_duplicates_and_caps() {
    let dir = tmp_dir("caps");
    let _data = data_dir_guard("caps");
    let factory = Factory::new(&["first reply.", "second reply."]);
    let (table, mut notes) = team_table(1, &dir, Some(factory.speakers()));
    let h = hand(&table);
    let ctx = hand_ctx(&dir);

    // invalid names refuse on the grammar
    for bad in ["", "Scout", "with space", "under_score", &"x".repeat(33)] {
        let out = h
            .execute(
                &serde_json::json!({"action": "spawn", "name": bad, "prompt": "p"}),
                &ctx,
            )
            .await;
        assert!(out.is_error, "'{bad}' must refuse");
        assert!(
            out.content.contains("invalid name"),
            "'{bad}': {}",
            out.content
        );
    }

    // the cap: one live member fills the table
    let out = h
        .execute(
            &serde_json::json!({"action": "spawn", "name": "scout", "prompt": "work"}),
            &ctx,
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    let out = h
        .execute(
            &serde_json::json!({"action": "spawn", "name": "other", "prompt": "work"}),
            &ctx,
        )
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("team is full") && out.content.contains("team_max"),
        "{}",
        out.content
    );
    // duplicates refuse too
    let out = h
        .execute(
            &serde_json::json!({"action": "spawn", "name": "scout", "prompt": "work"}),
            &ctx,
        )
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("already exists"), "{}", out.content);
    let _ = next_team_note(&mut notes).await; // scout's opening turn lands

    ka_strand::set_data_dir_override(None);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&_data);
}

/// `[delegate] team_max = 0` disables teams with an informative error —
/// the hand answers, it does not vanish.
#[tokio::test]
async fn team_max_zero_disables_with_an_informative_error() {
    let dir = tmp_dir("zero");
    let _data = data_dir_guard("zero");
    let (table, _notes) = team_table(0, &dir, None);
    let h = hand(&table);
    let ctx = hand_ctx(&dir);
    let out = h
        .execute(
            &serde_json::json!({"action": "spawn", "name": "scout", "prompt": "work"}),
            &ctx,
        )
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("disabled") && out.content.contains("team_max"),
        "{}",
        out.content
    );
    // status still works (an empty table is not an error)
    let out = h
        .execute(&serde_json::json!({"action": "status"}), &ctx)
        .await;
    assert!(!out.is_error);
    assert!(out.content.contains("no team members"), "{}", out.content);

    ka_strand::set_data_dir_override(None);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&_data);
}

/// Engine-level wiring: the parent session carries the `team` hand, a
/// scripted call spawns a member, the completion note reaches the
/// parent transcript, and the /tasks roster lists the member row.
#[tokio::test]
async fn parent_engine_routes_team_actions_and_roster_rows() {
    let dir = tmp_dir("engine");
    let _data = data_dir_guard("engine");
    let factory = Factory::new(&["Survey done: 3 crates."]);

    // the parent's scripted turn: one team spawn call, then text
    let parent = Arc::new(ScriptedParent {
        calls: parking_lot::Mutex::new(VecDeque::from([(
            "team",
            serde_json::json!({
                "action": "spawn",
                "name": "scout",
                "prompt": "survey the repo"
            }),
        )])),
        texts: parking_lot::Mutex::new(VecDeque::from(["spawned the scout.".to_string()])),
    });
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with_team_speakers(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        Wire::OpenaiChat,
        parent.clone(),
        factory.speakers(),
    );
    handle
        .commands
        .send(Command::Prompt {
            text: "spawn a scout on the survey".into(),
            schema: None,
            images: Vec::new(),
            allowed_tools: None,
            model: None,
            skills: Vec::new(),
        })
        .await
        .expect("engine alive");

    // the parent turn ends with Idle; the member note follows async
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut saw_idle = false;
    let mut note = String::new();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), handle.events.recv()).await {
            Ok(Some(Event::Idle)) => saw_idle = true,
            Ok(Some(Event::Note { message })) if message.starts_with(TEAM_NOTE_PREFIX) => {
                note = message;
                break;
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    assert!(saw_idle, "the parent prompt cycle completed");
    assert_eq!(note, "team · scout — Survey done: 3 crates.");

    // the /tasks roster carries the member row (the TUI pill ignores it)
    handle
        .commands
        .send(Command::ListTasks)
        .await
        .expect("engine alive");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut rows: Vec<String> = Vec::new();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), handle.events.recv()).await {
            Ok(Some(Event::Tasks { rows: r })) => {
                rows = r;
                break;
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    assert!(
        rows.iter()
            .any(|r| r.starts_with("team scout ") && r.contains("done")),
        "member row on the roster: {rows:?}"
    );

    ka_strand::set_data_dir_override(None);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&_data);
}

/// The parent's scripted speaker: one tool call per request round, then
/// a text reply (the engine_e2e contract fixture, minimal).
struct ScriptedParent {
    calls: parking_lot::Mutex<VecDeque<(&'static str, serde_json::Value)>>,
    texts: parking_lot::Mutex<VecDeque<String>>,
}

impl Speaker for ScriptedParent {
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
                let text = {
                    let next = self.texts.lock().pop_front();
                    next.unwrap_or_else(|| "done".into())
                };
                out.send(StreamEvent::Text(text)).await.ok();
            }
            out.send(StreamEvent::Finished {
                stop: Stop::Done,
                usage: ka_protocol::Usage::default(),
            })
            .await
            .ok();
        })
    }
}
