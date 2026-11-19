//! `ka acp`: Agent Client Protocol server over line-delimited JSON-RPC
//! 2.0 on stdin/stdout. One `spawn_full` engine per session; events map
//! to `session/update` notifications; permission asks surface as
//! `session/request_permission` requests to the client (responses are
//! correlated by request id). Logging goes to stderr only. Bounded
//! method set: initialize, session/new, session/load, session/prompt,
//! session/cancel.

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ka_engine::config::Config;
use ka_engine::{EngineHandle, spawn};
use ka_protocol::{Command, DeltaKind, Event, Stop};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};

/// Pending client permission responses: rpc request id →
/// (engine ask id, session id, answer channel to the drive task).
type Pending = Arc<Mutex<HashMap<u64, (String, String, oneshot::Sender<usize>)>>>;

type SharedEngine = Arc<Mutex<EngineHandle>>;

/// Sessions own their engine behind a lock (one turn at a time). The
/// command sender rides NEXT TO the locked handle: `session/cancel`
/// must reach the engine mid-turn, when the turn task holds that lock.
#[derive(Clone)]
struct SessionHandle {
    commands: mpsc::Sender<Command>,
    engine: SharedEngine,
}

/// Entry: run the ACP loop over stdin/stdout.
pub async fn run(configs: &[std::path::PathBuf]) -> Result<ExitCode, String> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    run_loop(stdin, stdout, configs).await
}

async fn run_loop<R, W>(
    input: R,
    mut output: W,
    configs: &[std::path::PathBuf],
) -> Result<ExitCode, String>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    // writer task: serialized stdout
    tokio::spawn(async move {
        while let Some(v) = out_rx.recv().await {
            let mut line = v.to_string();
            line.push('\n');
            output.write_all(line.as_bytes()).await.ok();
            output.flush().await.ok();
        }
    });

    let sessions: Arc<Mutex<HashMap<String, SessionHandle>>> = Arc::default();
    let pending: Pending = Arc::default();
    let counter = Arc::new(AtomicU64::new(0));
    // a counter, not map.len()+1: session ids stay unique even if
    // entries ever leave the map (same invariant as serve.rs)
    let mut session_counter = 0u64;

    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines.next_line().await.map_err(|e| format!("stdin: {e}"))? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            out_tx
                .send(rpc_error(Value::Null, -32700, "parse error"))
                .ok();
            continue;
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg["method"].as_str().map(str::to_string);

        match method {
            None => {
                // client response: resolve a pending permission request
                let Some(req_id) = id.as_u64() else {
                    continue;
                };
                let option_id = msg["result"]["option"]["optionId"]
                    .as_str()
                    .and_then(|s| s.parse::<usize>().ok())
                    .or_else(|| {
                        msg["result"]["optionId"]
                            .as_str()
                            .and_then(|s| s.parse().ok())
                    })
                    // engine choice 2 = deny; an unparseable or absent
                    // answer must NEVER fall back to 1 (always-allow,
                    // persisted to .ka/ka.toml)
                    .unwrap_or(2);
                let mut pending = pending.lock().await;
                if let Some((_ask_id, _session, tx)) = pending.remove(&req_id) {
                    tx.send(option_id).ok();
                }
            }
            Some(m) if m == "initialize" => {
                out_tx
                    .send(rpc_result(
                        id,
                        json!({
                            "protocolVersion": 1,
                            "agentCapabilities": {"loadSession": true},
                            "authMethods": [],
                        }),
                    ))
                    .ok();
            }
            Some(m) if m == "session/new" => {
                let cwd = msg["params"]["cwd"]
                    .as_str()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                let cfg = match crate::load_surface_config(configs, &cwd) {
                    Ok(cfg) => Config {
                        cwd: Some(cwd.display().to_string()),
                        ..cfg
                    },
                    Err(e) => {
                        out_tx
                            .send(rpc_error(id, -32603, format!("config: {e}")))
                            .ok();
                        continue;
                    }
                };
                let ka_engine::EngineHandle { commands, events } = spawn(cfg);
                let handle = Arc::new(Mutex::new(ka_engine::EngineHandle {
                    commands: commands.clone(),
                    events,
                }));
                let mut sessions = sessions.lock().await;
                session_counter += 1;
                let session = format!("s{session_counter}");
                sessions.insert(
                    session.clone(),
                    SessionHandle {
                        commands,
                        engine: handle,
                    },
                );
                out_tx
                    .send(rpc_result(id, json!({"sessionId": session})))
                    .ok();
            }
            Some(m) if m == "session/load" => {
                let session = msg["params"]["sessionId"].as_str().unwrap_or_default();
                {
                    let sessions = sessions.lock().await;
                    // exact hit …
                    if sessions.contains_key(session) {
                        out_tx.send(rpc_result(id, json!({}))).ok();
                        continue;
                    }
                    // … or a prefix of an in-memory strand id: reuse that
                    // engine instead of double-attaching the same strand
                    if let Some(key) = sessions
                        .keys()
                        .find(|k| k.starts_with(session) && session.len() >= 2)
                    {
                        let key = key.clone();
                        drop(sessions);
                        out_tx.send(rpc_result(id, json!({"sessionId": key}))).ok();
                        continue;
                    }
                }
                // not in memory: try to resume a strand from disk (same
                // prefix resolution as `ka --session`) in the session's
                // cwd (falling back to this process's cwd)
                let cwd = msg["params"]["cwd"]
                    .as_str()
                    .map(std::path::PathBuf::from)
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_default();
                let matched = match ka_strand::resolve_id(&cwd, session) {
                    Ok(ka_strand::IdMatch::Unique(summary)) => Some(summary),
                    Ok(ka_strand::IdMatch::None) => {
                        // memorable session names resolve like
                        // `ka --session` (exact, then unique prefix)
                        match ka_strand::resolve_name(&cwd, session) {
                            Ok(ka_strand::IdMatch::Unique(summary)) => Some(summary),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                let resumed = matched;
                match resumed {
                    Some(summary) => {
                        let cfg = match crate::load_surface_config(configs, &cwd) {
                            Ok(cfg) => Config {
                                cwd: Some(cwd.display().to_string()),
                                ..cfg
                            },
                            Err(e) => {
                                out_tx
                                    .send(rpc_error(id, -32603, format!("config: {e}")))
                                    .ok();
                                continue;
                            }
                        };
                        let ka_engine::EngineHandle {
                            commands,
                            mut events,
                        } = ka_engine::spawn_full(
                            cfg,
                            ka_dialect::Catalog::embedded(),
                            ka_engine::StrandChoice::Path(summary.path.clone()),
                        );
                        // deliver the bootstrap replay at load time (the
                        // engine emits it during attach_strand, before
                        // awaiting any command): drain until the Replay
                        // row(s) arrive and the channel idles, forwarding
                        // them to the client; everything else stays in
                        // the channel for the first prompt's drive_turn
                        let deadline =
                            tokio::time::Instant::now() + std::time::Duration::from_secs(2);
                        let mut replay_seen = false;
                        let mut idle_ticks = 0u32;
                        while tokio::time::Instant::now() < deadline {
                            match events.try_recv() {
                                Ok(Event::Replay { messages }) => {
                                    forward_replay(&out_tx, &summary.id, messages).await;
                                    replay_seen = true;
                                }
                                Ok(_) => {}
                                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                                    if replay_seen {
                                        idle_ticks += 1;
                                        if idle_ticks >= 2 {
                                            break;
                                        }
                                    }
                                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                                }
                                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                            }
                        }
                        let handle = Arc::new(Mutex::new(ka_engine::EngineHandle {
                            commands: commands.clone(),
                            events,
                        }));
                        let mut sessions = sessions.lock().await;
                        // key the engine by the full strand id so the
                        // client's prefix resolves to a canonical handle
                        sessions.insert(
                            summary.id.clone(),
                            SessionHandle {
                                commands,
                                engine: handle,
                            },
                        );
                        out_tx
                            .send(rpc_result(id, json!({"sessionId": summary.id})))
                            .ok();
                    }
                    None => {
                        out_tx
                            .send(rpc_error(
                                id,
                                -32602,
                                format!("unknown session {session:?}"),
                            ))
                            .ok();
                    }
                }
            }
            Some(m) if m == "session/prompt" => {
                let session = msg["params"]["sessionId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let Some(prompt) = prompt_text(&msg["params"]["prompt"]) else {
                    out_tx
                        .send(rpc_error(id, -32602, "prompt: no text content".to_string()))
                        .ok();
                    continue;
                };
                let sessions = sessions.lock().await;
                let Some(engine) = sessions.get(&session).map(|s| s.engine.clone()) else {
                    out_tx
                        .send(rpc_error(
                            id,
                            -32602,
                            format!("unknown session {session:?}"),
                        ))
                        .ok();
                    continue;
                };
                drop(sessions);
                // the drive task owns this request id: it emits
                // session/update notifications and finally the response
                tokio::spawn(drive_turn(
                    engine,
                    session,
                    prompt,
                    id,
                    out_tx.clone(),
                    Arc::clone(&pending),
                    Arc::clone(&counter),
                ));
            }
            Some(m) if m == "session/cancel" => {
                let session = msg["params"]["sessionId"].as_str().unwrap_or_default();
                // clone the command sender and release BOTH locks before
                // sending: drive_turn holds the engine mutex for the
                // whole turn, so locking here would park the stdin loop
                // until the turn ends — making cancel a no-op
                let commands = {
                    let sessions = sessions.lock().await;
                    sessions.get(session).map(|e| e.commands.clone())
                };
                if let Some(commands) = commands {
                    commands.send(Command::Abort).await.ok();
                }
                // the aborted turn's unanswered asks must not park the
                // drive task on the engine lock forever — deny them
                deny_pending(&pending, session).await;
            }
            Some(other) => {
                out_tx
                    .send(rpc_error(id, -32601, format!("method not found: {other}")))
                    .ok();
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Resolve every pending permission request of `session` with a deny
/// (2): a cancelled turn must not leave its drive task parked on an
/// answer that will never come while it holds the engine lock.
async fn deny_pending(pending: &Pending, session: &str) {
    let stranded: Vec<_> = {
        let mut pending = pending.lock().await;
        let ids: Vec<u64> = pending
            .iter()
            .filter(|(_, (_, s, _))| s == session)
            .map(|(k, _)| *k)
            .collect();
        ids.into_iter().filter_map(|k| pending.remove(&k)).collect()
    };
    for (_ask, _session, tx) in stranded {
        tx.send(2).ok(); // deny — same default as a garbage client answer
    }
}

/// Drive one engine turn: translate events into session/update
/// notifications and finish with the prompt response. Permission asks
/// surface as `session/request_permission` requests; the main loop
/// resolves them and wakes the drive task through the pending map.
#[allow(clippy::too_many_arguments)]
async fn drive_turn(
    engine: SharedEngine,
    session: String,
    prompt: String,
    request_id: Value,
    out: mpsc::UnboundedSender<Value>,
    pending: Pending,
    counter: Arc<AtomicU64>,
) {
    let mut engine = engine.lock().await;
    let engine = &mut *engine;
    if engine
        .commands
        .send(Command::Prompt {
            allowed_tools: None,
            model: None,
            text: prompt,
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .is_err()
    {
        out.send(rpc_error(request_id, -32603, String::from("engine closed")))
            .ok();
        return;
    }
    let stop = loop {
        let Some(evt) = engine.events.recv().await else {
            out.send(rpc_error(
                request_id,
                -32603,
                String::from("engine closed mid-turn"),
            ))
            .ok();
            return;
        };
        match evt {
            Event::Delta { kind } => match kind {
                DeltaKind::Text(t) => {
                    session_update(
                        &out,
                        &session,
                        json!({
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": t}
                        }),
                    )
                    .await;
                }
                DeltaKind::Thought(t) => {
                    session_update(
                        &out,
                        &session,
                        json!({
                            "sessionUpdate": "agent_thought_chunk",
                            "content": {"type": "text", "text": t}
                        }),
                    )
                    .await;
                }
                DeltaKind::Call { .. } => {}
            },
            Event::CallStarted { tool, id, detail } => {
                let content = if detail.is_empty() {
                    Vec::<Value>::new()
                } else {
                    vec![json!({
                        "type": "content",
                        "content": {"type": "text", "text": detail}
                    })]
                };
                session_update(
                    &out,
                    &session,
                    json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": id,
                        "title": tool,
                        "content": content,
                    }),
                )
                .await;
            }
            Event::CallOutput {
                tool,
                id,
                excerpt,
                is_error,
                ..
            } => {
                session_update(
                    &out,
                    &session,
                    json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": id,
                        "title": tool,
                        "status": if is_error { "failed" } else { "completed" },
                        "content": [{
                            "type": "content",
                            "content": {"type": "text", "text": excerpt}
                        }],
                    }),
                )
                .await;
            }
            Event::Ask { id, questions, .. } => {
                let (question_text, options_list) = questions
                    .first()
                    .map(|q| (q.text.clone(), q.options.clone()))
                    .unwrap_or_else(|| (String::new(), Vec::new()));
                let options: Vec<Value> = options_list
                    .iter()
                    .enumerate()
                    .map(|(i, o)| {
                        // engine: 0=allow, 1=always (persistent,
                        // written to .ka/ka.toml), 2=deny
                        let kind = match i {
                            0 => "allow_once",
                            1 => "allow_always",
                            _ => "reject_once",
                        };
                        json!({
                            "optionId": i.to_string(),
                            "name": o,
                            "kind": kind,
                        })
                    })
                    .collect();
                let req_id = counter.fetch_add(1, Ordering::Relaxed) + 10_000;
                let (tx, rx) = oneshot::channel();
                pending
                    .lock()
                    .await
                    .insert(req_id, (id.0.clone(), session.clone(), tx));
                out.send(json!({
                    "jsonrpc": "2.0",
                    "method": "session/request_permission",
                    "params": {
                        "sessionId": session,
                        "toolCall": {"title": question_text},
                        "options": options,
                    }
                }))
                .ok();
                // the main loop resolves us when the client responds
                // client went away: deny
                let choice = rx.await.unwrap_or(2);
                engine
                    .commands
                    .send(Command::Answer {
                        question: id,
                        choice,
                    })
                    .await
                    .ok();
            }
            Event::Replay { messages } => {
                // resumed strand: prior rows land on the client as
                // replayed message chunks (digest boundaries as dividers)
                forward_replay(&out, &session, messages).await;
            }
            Event::TurnFinished { stop, .. } => {
                break match stop {
                    Stop::Done => "end_turn",
                    Stop::Length => "max_tokens",
                    Stop::Aborted => "cancelled",
                    Stop::Error => "refusal",
                };
            }
            // engine commentary must reach the editor: errors (otherwise
            // the client only sees stopReason "refusal" with no reason),
            // verify results, auto-commit and reconnect notes
            Event::Error { message, .. } | Event::Note { message } => {
                session_update(
                    &out,
                    &session,
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": format!("{message}\n")}
                    }),
                )
                .await;
            }
            _ => {}
        }
    };
    out.send(rpc_result(request_id, json!({"stopReason": stop})))
        .ok();
}

async fn session_update(out: &mpsc::UnboundedSender<Value>, session: &str, update: Value) {
    out.send(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {"sessionId": session, "update": update}
    }))
    .ok();
}

/// Map replayed messages (bootstrap or prompt-time) onto the client as
/// message-chunk updates; digest boundaries become divider rows.
async fn forward_replay(
    out: &mpsc::UnboundedSender<Value>,
    session: &str,
    messages: Vec<ka_protocol::ReplayedMessage>,
) {
    for m in messages {
        let kind = match m.role.as_str() {
            "user" => "user_message_chunk",
            _ => "agent_message_chunk",
        };
        let text = if m.digest {
            "— context digest boundary —".to_string()
        } else {
            m.content
        };
        session_update(
            out,
            session,
            json!({
                "sessionUpdate": kind,
                "content": {"type": "text", "text": text}
            }),
        )
        .await;
    }
}

/// Extract the concatenated text of a prompt content array (or a bare
/// string).
fn prompt_text(v: &Value) -> Option<String> {
    if let Some(text) = v.as_str() {
        return Some(text.to_string());
    }
    let parts = v.as_array()?;
    let mut out = String::new();
    for part in parts {
        if let Some(t) = part["text"].as_str() {
            out.push_str(t);
        }
    }
    (!out.is_empty()).then_some(out)
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i32, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Drive the loop over in-memory duplexes: initialize → session/new →
    /// session/prompt; collect output until the prompt response arrives
    /// and assert session/update notifications were streamed.
    #[tokio::test]
    async fn acp_loop_serves_prompt_over_pipe() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut to_server, server_stdin) = tokio::io::duplex(64 * 1024);
        let (mut from_server, server_stdout) = tokio::io::duplex(64 * 1024);

        tokio::spawn(async move {
            let _ = run_loop(server_stdin, server_stdout, &[]).await;
        });

        let mut buf = vec![0u8; 64 * 1024];
        let mut collected = String::new();

        // initialize
        to_server
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .unwrap();
        to_server.flush().await.unwrap();

        // session/new
        to_server
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{}}\n")
            .await
            .unwrap();
        to_server.flush().await.unwrap();

        let session = loop {
            let n = from_server.read(&mut buf).await.unwrap();
            collected.push_str(&String::from_utf8_lossy(&buf[..n]));
            if let Some(line) = collected.lines().find(|l| l.contains("\"sessionId\"")) {
                let v: Value = serde_json::from_str(line).unwrap();
                break v["result"]["sessionId"].as_str().unwrap().to_string();
            }
        };

        // prompt the canned engine (no model configured: canned speaker)
        let prompt = json!({
            "jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId": session, "prompt": "hello"}
        });
        let mut line = prompt.to_string();
        line.push('\n');
        to_server.write_all(line.as_bytes()).await.unwrap();
        to_server.flush().await.unwrap();

        let (collected, stop) = loop {
            let n = from_server.read(&mut buf).await.unwrap();
            collected.push_str(&String::from_utf8_lossy(&buf[..n]));
            let mut found = None;
            for line in collected.lines() {
                if line.contains("\"id\":3") {
                    if let Ok(v) = serde_json::from_str::<Value>(line) {
                        if let Some(reason) = v["result"]["stopReason"].as_str() {
                            found = Some(reason.to_string());
                            break;
                        }
                    }
                }
            }
            if let Some(stop) = found {
                break (collected.clone(), stop);
            }
        };
        assert!(
            collected.contains("session/update"),
            "updates must stream: {collected}"
        );
        assert!(stop == "end_turn" || stop == "refusal", "{stop}");
    }

    #[tokio::test]
    async fn cancel_denies_stranded_permission_asks() {
        let pending: Pending = Arc::default();
        let (tx, rx) = oneshot::channel();
        pending
            .lock()
            .await
            .insert(10_001, ("ask-1".to_string(), "s1".to_string(), tx));
        // a different session's ask must survive
        let (tx2, mut rx2) = oneshot::channel();
        pending
            .lock()
            .await
            .insert(10_002, ("ask-2".to_string(), "s2".to_string(), tx2));
        deny_pending(&pending, "s1").await;
        assert_eq!(
            pending.lock().await.len(),
            1,
            "only the cancelled session's asks drain"
        );
        assert_eq!(rx.await.unwrap(), 2, "the stranded ask is denied");
        assert!(rx2.try_recv().is_err(), "other sessions keep waiting");
    }

    /// A broken --config surfaces as an rpc error on session/new instead
    /// of a silently defaulted engine.
    #[tokio::test]
    async fn bad_config_errors_session_new() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = std::env::temp_dir().join(format!("ka-acp-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.toml");
        std::fs::write(&bad, "[tui]\nunknown_key = 1\n").unwrap();
        let configs = vec![bad];
        let (mut to_server, server_stdin) = tokio::io::duplex(64 * 1024);
        let (mut from_server, server_stdout) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let _ = run_loop(server_stdin, server_stdout, &configs).await;
        });
        to_server
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{}}\n")
            .await
            .unwrap();
        to_server.flush().await.unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        let n = from_server.read(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(text.contains("error"), "{text}");
        assert!(text.contains("config"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A server never trusts a project implicitly: an untrusted
    /// project's .ka/ka.toml must not load. A strict-TOML error in it
    /// would otherwise fail session/new — success proves it was skipped.
    #[tokio::test]
    async fn untrusted_project_config_is_skipped() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = std::env::temp_dir().join(format!("ka-acp-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join(".ka")).unwrap();
        std::fs::write(dir.join(".ka/ka.toml"), "[tui]\nunknown_key = 1\n").unwrap();
        let (mut to_server, server_stdin) = tokio::io::duplex(64 * 1024);
        let (mut from_server, server_stdout) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let _ = run_loop(server_stdin, server_stdout, &[]).await;
        });
        let msg = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{{\"cwd\":{:?}}}}}\n",
            dir.display().to_string()
        );
        to_server.write_all(msg.as_bytes()).await.unwrap();
        to_server.flush().await.unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        let n = from_server.read(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(
            text.contains("sessionId"),
            "an untrusted project config must not load: {text}"
        );
        assert!(!text.contains("\"error\""), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
