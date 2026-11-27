//! Child-posture contracts: every voice a parent spawns — pathfinder, a
//! delegate agent, a `/tan` job, a team member, and the calls a `code`
//! script routes — is born with the parent's `[[rules]]` AND its
//! tool-call hooks. A child that tries to `read` a file outside the
//! project is refused, and the file's content never reaches the child's
//! model. Each contract runs once per policy (`rule`, `hook`) plus a
//! `none` control that proves the child really would have read the file.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ka_dialect::dialects::Catalog;
use ka_dialect::speaker::{SpeakFuture, SpeakRequest, Speaker, StreamEvent, ToolCall};
use ka_engine::config::{Config, Hook, HookEvent, Rule, Verdict};
use ka_protocol::{Command, Event};

const SECRET: &str = "TOP-SECRET-CHILD-POLICY-TOKEN";
const HOOK_MARKER: &str = "policy-hook-veto";
/// The voice gate says `denied by rule for read`, the code router
/// `rule for read` — both carry this.
const RULE_MARKER: &str = "rule for read";

// ---------------------------------------------------------------- fixtures

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ka-child-policy-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A project plus a secret that sits OUTSIDE it (a sibling directory):
/// an absolute `read` is the only way to reach it.
struct Fixture {
    base: PathBuf,
    project: PathBuf,
    secret: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let base = tmp_dir(tag);
        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let secret = base.join("outside/token.txt");
        std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
        std::fs::write(&secret, SECRET).unwrap();
        Self {
            base,
            project,
            secret,
        }
    }

    fn secret_str(&self) -> String {
        self.secret.to_string_lossy().into_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Strand storage redirect: the sessions these tests spawn must land in
/// a temp data root, never the developer's real one. Thread-local — a
/// `#[tokio::test]` runtime is current-thread, so spawned engines share
/// the override. Created FIRST in a test so it drops LAST.
struct DataGuard(PathBuf);

impl DataGuard {
    fn new(tag: &str) -> Self {
        let dir = tmp_dir(&format!("data-{tag}"));
        ka_strand::set_data_dir_override(Some(dir.clone()));
        Self(dir)
    }
}

impl Drop for DataGuard {
    fn drop(&mut self) {
        ka_strand::set_data_dir_override(None);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ----------------------------------------------------------------- policies

#[derive(Clone, Copy, Debug, PartialEq)]
enum Policy {
    /// The control: no rule, no hook — the child reads the secret.
    None,
    /// `[[rules]]`: deny `read` of exactly this file.
    Rule,
    /// A `pre_tool_use` hook on `read` that exits 2.
    Hook,
}

const POLICIES: [Policy; 3] = [Policy::None, Policy::Rule, Policy::Hook];

fn config(fx: &Fixture, policy: Policy) -> Config {
    let mut cfg = Config {
        cwd: Some(fx.project.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        ..Default::default()
    };
    match policy {
        Policy::None => {}
        Policy::Rule => {
            cfg.rules = vec![Rule {
                tool: "read".into(),
                pattern: Some(fx.secret_str()),
                verdict: Verdict::Deny,
            }]
        }
        Policy::Hook => {
            cfg.hooks = vec![Hook {
                event: HookEvent::PreToolUse,
                tool: Some("read".into()),
                command: format!("echo {HOOK_MARKER} >&2; exit 2"),
                timeout_secs: None,
                async_: None,
                url: None,
            }]
        }
    }
    cfg
}

/// What the refusal must say (None for the control).
fn marker(policy: Policy) -> Option<&'static str> {
    match policy {
        Policy::None => None,
        Policy::Rule => Some(RULE_MARKER),
        Policy::Hook => Some(HOOK_MARKER),
    }
}

/// The contract over what a child model was shown: the child asked for
/// the secret (its second request carries the call and its result), and
/// either got the refusal (`rule`,
/// `hook`) or the file itself (`none` control — which proves the path
/// is live and the other two cases are not vacuous).
fn assert_outcome(kind: &str, policy: Policy, requests: &[String], secret_path: &str) {
    assert!(
        requests.len() >= 2,
        "{kind}/{policy:?}: the child never reached a second round: {requests:?}"
    );
    assert!(
        requests[1].contains(secret_path),
        "{kind}/{policy:?}: the child did not ask for the secret: {}",
        &requests[1][requests[1].len().saturating_sub(600)..]
    );
    match marker(policy) {
        Some(want) => {
            assert!(
                requests[1].contains(want),
                "{kind}/{policy:?}: the refusal `{want}` never reached the child: {}",
                &requests[1][requests[1].len().saturating_sub(800)..]
            );
            assert!(
                !requests.iter().any(|r| r.contains(SECRET)),
                "{kind}/{policy:?}: the secret reached the child model"
            );
        }
        None => assert!(
            requests[1].contains(SECRET),
            "{kind}/none control: the child should have read the secret: {}",
            &requests[1][requests[1].len().saturating_sub(800)..]
        ),
    }
}

// ---------------------------------------------------- scripted model voices

/// The parent's speaker: one scripted tool call per request round, then
/// a text reply.
struct Parent {
    calls: parking_lot::Mutex<VecDeque<(&'static str, serde_json::Value)>>,
}

impl Parent {
    fn speaker(calls: Vec<(&'static str, serde_json::Value)>) -> Arc<dyn Speaker> {
        Arc::new(Self {
            calls: parking_lot::Mutex::new(calls.into()),
        })
    }
}

impl Speaker for Parent {
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
                usage: ka_protocol::Usage::default(),
            })
            .await
            .ok();
        })
    }
}

/// A loopback OpenAI-chat SSE mock for the nested voices (pathfinder,
/// delegate, `/tan` speak over the catalog, not an injected speaker):
/// the first request gets a `read` call on the secret, every later one
/// a text reply. Request bodies are recorded — the proof of what the
/// child model was shown.
struct MockModel {
    addr: std::net::SocketAddr,
    requests: Arc<parking_lot::Mutex<Vec<String>>>,
}

impl MockModel {
    async fn start(secret: PathBuf) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let served = Arc::new(AtomicUsize::new(0));
        tokio::spawn({
            let requests = requests.clone();
            let served = served.clone();
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let requests = requests.clone();
                    let served = served.clone();
                    let secret = secret.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve(stream, requests, served, secret).await {
                            eprintln!("child-policy mock: {e}");
                        }
                    });
                }
            }
        });
        Self { addr, requests }
    }

    fn catalog(&self) -> Catalog {
        Catalog::parse(&format!(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \
             \"http://{}/v1\"\ncontext = 1000000\n",
            self.addr
        ))
        .unwrap()
    }

    async fn wait_requests(&self, n: usize) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while tokio::time::Instant::now() < deadline {
            {
                let seen = self.requests.lock();
                if seen.len() >= n {
                    return seen.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.requests.lock().clone()
    }
}

async fn serve(
    mut stream: tokio::net::TcpStream,
    requests: Arc<parking_lot::Mutex<Vec<String>>>,
    served: Arc<AtomicUsize>,
    secret: PathBuf,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n".as_slice()) {
            break pos + 4;
        }
        if buf.len() > 1 << 20 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let len: usize = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap_or(0))
        })
        .unwrap_or(0);
    while buf.len() < head_end + len {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    requests
        .lock()
        .push(String::from_utf8_lossy(&buf[head_end..]).into_owned());
    let first = served.fetch_add(1, Ordering::SeqCst) == 0;
    let body = if first {
        let args = serde_json::json!({ "path": secret.to_string_lossy() }).to_string();
        let call = serde_json::json!({"choices":[{"delta":{"tool_calls":[{
            "index": 0,
            "id": "call_1",
            "function": { "name": "read", "arguments": args }
        }]}}]});
        format!(
            "data: {call}\n\n\
             data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
             data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":1}}}}\n\n\
             data: [DONE]\n\n"
        )
    } else {
        "data: {\"choices\":[{\"delta\":{\"content\":\"nothing to report\"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
         data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":1}}\n\n\
         data: [DONE]\n\n"
            .to_string()
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\
         Connection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.shutdown().await
}

// ------------------------------------------------------------ engine driver

async fn prompt(handle: &mut ka_engine::EngineHandle) {
    handle
        .commands
        .send(Command::Prompt {
            text: "go".into(),
            schema: None,
            images: Vec::new(),
            allowed_tools: None,
            model: None,
            skills: Vec::new(),
        })
        .await
        .ok();
}

/// Collect events until `done` accepts one (inclusive) or the deadline
/// passes — a stuck engine fails the assertion that follows instead of
/// hanging the suite.
async fn wait_until(
    handle: &mut ka_engine::EngineHandle,
    done: impl Fn(&Event) -> bool,
    secs: u64,
) -> Vec<Event> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut seen = Vec::new();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), handle.events.recv()).await {
            Ok(Some(ev)) => {
                let stop = done(&ev);
                seen.push(ev);
                if stop {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }
    seen
}

fn idle(ev: &Event) -> bool {
    matches!(ev, Event::Idle)
}

// ----------------------------------------------------------------- contracts

/// pathfinder: a read-only nested voice, driven over the catalog.
#[tokio::test]
async fn pathfinder_child_inherits_rules_and_hooks() {
    let fx = Fixture::new("pathfinder");
    let _data = DataGuard::new("pathfinder");
    for policy in POLICIES {
        let mock = MockModel::start(fx.secret.clone()).await;
        let parent = Parent::speaker(vec![(
            "pathfinder",
            serde_json::json!({"query": "quote the token file verbatim"}),
        )]);
        let mut handle = ka_engine::spawn_with_speaker(
            config(&fx, policy),
            mock.catalog(),
            ka_engine::StrandChoice::New,
            ka_dialect::Wire::OpenaiChat,
            parent,
        );
        prompt(&mut handle).await;
        let seen = wait_until(&mut handle, idle, 30).await;
        assert!(
            seen.iter().any(idle),
            "pathfinder/{policy:?}: the parent turn never went idle: {seen:?}"
        );
        let requests = mock.wait_requests(2).await;
        assert_outcome("pathfinder", policy, &requests, &fx.secret_str());
    }
}

/// A delegate agent (`.ka/agents/scout.md`): a nested voice over the
/// catalog, read-only for a non-isolated agent.
#[cfg(feature = "test-util")]
#[tokio::test]
async fn delegate_agent_inherits_rules_and_hooks() {
    let fx = Fixture::new("delegate");
    let _data = DataGuard::new("delegate");
    // project-scoped agents are trust-gated: approve this temp project
    // against a redirected trust store
    let (_trust_file, _trust) = ka_engine::trust::test_support::trust_guard();
    ka_engine::trust::approve(&fx.project);
    let agents = fx.project.join(".ka/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("scout.md"),
        "---\nname: scout\ndescription: looks around\n---\nYou scout. Answer in one line.\n",
    )
    .unwrap();
    for policy in POLICIES {
        let mock = MockModel::start(fx.secret.clone()).await;
        let parent = Parent::speaker(vec![(
            "delegate",
            serde_json::json!({
                "agent": "scout",
                "task": "quote the token file verbatim"
            }),
        )]);
        let mut handle = ka_engine::spawn_with_speaker(
            config(&fx, policy),
            mock.catalog(),
            ka_engine::StrandChoice::New,
            ka_dialect::Wire::OpenaiChat,
            parent,
        );
        prompt(&mut handle).await;
        let seen = wait_until(&mut handle, idle, 30).await;
        assert!(
            seen.iter().any(idle),
            "delegate/{policy:?}: the parent turn never went idle: {seen:?}"
        );
        let requests = mock.wait_requests(2).await;
        assert_outcome("delegate", policy, &requests, &fx.secret_str());
    }
}

/// `/tan`: a background job through the same spawn path as delegate.
#[tokio::test]
async fn tan_job_inherits_rules_and_hooks() {
    let fx = Fixture::new("tan");
    let _data = DataGuard::new("tan");
    for policy in POLICIES {
        let mock = MockModel::start(fx.secret.clone()).await;
        let parent = Parent::speaker(Vec::new());
        let handle = ka_engine::spawn_with_speaker(
            config(&fx, policy),
            mock.catalog(),
            ka_engine::StrandChoice::New,
            ka_dialect::Wire::OpenaiChat,
            parent,
        );
        handle
            .commands
            .send(Command::Tan {
                text: "quote the token file verbatim".into(),
            })
            .await
            .expect("engine alive");
        let requests = mock.wait_requests(2).await;
        assert_outcome("tan", policy, &requests, &fx.secret_str());
        // let the background job settle before its strand root goes away
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A team member: a full leaf engine with its own injected speaker.
#[cfg(feature = "test-util")]
#[tokio::test]
async fn team_member_inherits_rules_and_hooks() {
    use ka_engine::hands::teams::{MemberSpeakers, TEAM_NOTE_PREFIX};

    /// One member's speaker: a `read` of the secret first, then text;
    /// every request is recorded.
    struct Member {
        secret: String,
        served: AtomicUsize,
    }

    impl Speaker for Member {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                REQUESTS.with(|r| r.borrow_mut().push(format!("{:?}", req.messages)));
                if self.served.fetch_add(1, Ordering::SeqCst) == 0 {
                    out.send(StreamEvent::Call(ToolCall {
                        id: "m1".into(),
                        tool: "read".into(),
                        arguments: serde_json::json!({"path": self.secret}),
                    }))
                    .await
                    .ok();
                } else {
                    out.send(StreamEvent::Text("nothing to report".into()))
                        .await
                        .ok();
                }
                out.send(StreamEvent::Finished {
                    stop: ka_protocol::Stop::Done,
                    usage: ka_protocol::Usage::default(),
                })
                .await
                .ok();
            })
        }
    }

    thread_local! {
        static REQUESTS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    let fx = Fixture::new("team");
    let _data = DataGuard::new("team");
    for policy in POLICIES {
        REQUESTS.with(|r| r.borrow_mut().clear());
        let secret = fx.secret_str();
        let factory: MemberSpeakers = Arc::new(move || {
            let member = Arc::new(Member {
                secret: secret.clone(),
                served: AtomicUsize::new(0),
            });
            (ka_dialect::Wire::OpenaiChat, member as Arc<dyn Speaker>)
        });
        let parent = Parent::speaker(vec![(
            "team",
            serde_json::json!({
                "action": "spawn",
                "name": "scout",
                "prompt": "quote the token file verbatim"
            }),
        )]);
        let mut handle = ka_engine::spawn_with_team_speakers(
            config(&fx, policy),
            Catalog::parse(
                "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \
                 \"http://127.0.0.1:1\"\ncontext = 1000000\n",
            )
            .unwrap(),
            ka_engine::StrandChoice::New,
            ka_dialect::Wire::OpenaiChat,
            parent,
            factory,
        );
        prompt(&mut handle).await;
        let seen = wait_until(
            &mut handle,
            |ev| matches!(ev, Event::Note { message } if message.starts_with(TEAM_NOTE_PREFIX)),
            30,
        )
        .await;
        assert!(
            seen.iter().any(
                |ev| matches!(ev, Event::Note { message } if message.starts_with(TEAM_NOTE_PREFIX))
            ),
            "team/{policy:?}: the member never reported back: {seen:?}"
        );
        let requests = REQUESTS.with(|r| r.borrow().clone());
        assert_outcome("team", policy, &requests, &fx.secret_str());
    }
}

/// `code`: the calls a script routes go through the router's own gate.
#[cfg(feature = "code")]
#[tokio::test]
async fn code_script_calls_inherit_rules_and_hooks() {
    let fx = Fixture::new("code");
    let _data = DataGuard::new("code");
    for policy in POLICIES {
        let script = format!(
            "return await ka.tools.call(\"read\", JSON.stringify({{path: {}}}))",
            serde_json::json!(fx.secret_str())
        );
        let parent = Parent::speaker(vec![("code", serde_json::json!({"script": script}))]);
        let mut handle = ka_engine::spawn_with_speaker(
            config(&fx, policy),
            Catalog::parse(
                "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \
                 \"http://127.0.0.1:1\"\ncontext = 1000000\n",
            )
            .unwrap(),
            ka_engine::StrandChoice::New,
            ka_dialect::Wire::OpenaiChat,
            parent,
        );
        prompt(&mut handle).await;
        let seen = wait_until(&mut handle, idle, 30).await;
        let out: String = seen
            .iter()
            .filter_map(|ev| match ev {
                Event::CallOutput { tool, excerpt, .. } if tool == "code" => Some(excerpt.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            seen.iter().any(idle) && !out.is_empty(),
            "code/{policy:?}: no code result reached the parent: {seen:?}"
        );
        match marker(policy) {
            Some(want) => {
                assert!(
                    out.contains(want),
                    "code/{policy:?}: the refusal `{want}` is missing: {out:?}"
                );
                assert!(
                    !out.contains(SECRET) && !format!("{seen:?}").contains(SECRET),
                    "code/{policy:?}: the secret reached the model"
                );
            }
            None => assert!(
                out.contains(SECRET),
                "code/none control: the script should have read the secret: {out:?}"
            ),
        }
    }
}

/// The posture filter: children inherit the tool-call hooks only.
#[test]
fn tool_hooks_filters_to_tool_call_events() {
    use ka_engine::child_policy::ChildPolicy;
    let hook = |event: HookEvent| Hook {
        event,
        tool: None,
        command: "true".into(),
        timeout_secs: None,
        async_: None,
        url: None,
    };
    let all = vec![
        hook(HookEvent::PreToolUse),
        hook(HookEvent::PostToolUse),
        hook(HookEvent::Stop),
        hook(HookEvent::SessionStart),
        hook(HookEvent::UserPromptSubmit),
    ];
    let kept: Vec<HookEvent> = ChildPolicy::tool_hooks(&all)
        .into_iter()
        .map(|h| h.event)
        .collect();
    assert_eq!(kept, vec![HookEvent::PreToolUse, HookEvent::PostToolUse]);
}
