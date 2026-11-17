//! DAP client — the probe (roadmap 8.2). Same hand-rolled stdio
//! JSON-RPC-over-Content-Length pattern as `lsp.rs`, sharing the
//! extracted `wire` codec. One `debug` hand drives sessions:
//! launch/attach → breakpoints → configurationDone → stopped-event
//! waits → threads/stack/variables/evaluate → disconnect.
//!
//! Deliberate bounds (fail toward boring):
//! - blocking actions (start/continue/step) wait for the next stop with
//!   a hard cap and report "still running" instead of hanging;
//! - the adapter's stdout ring is bounded (oldest lines drop);
//! - sessions are capped and reaped lazily (dead or past TTL);
//! - `runInTerminal` reverse requests are refused — ka's console is
//!   external, so an adapter asking us to spawn a terminal gets an
//!   instructive failure, never a spawn.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Notify, mpsc, oneshot};

/// Adapters can be slow to boot (gdb initializes its symbols).
const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// One request-response round trip.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a blocking action waits for the next stop.
const STOP_WAIT: Duration = Duration::from_secs(30);
/// Output ring: lines (oldest dropped) and a per-line render cap.
const OUTPUT_LINES: usize = 200;
const OUTPUT_LINE_CAP: usize = 2000;
/// Live session cap and the idle TTL (reaped lazily on access).
const MAX_SESSIONS: usize = 4;
const SESSION_TTL: Duration = Duration::from_secs(30 * 60);

/// Embedded adapter seed: name → stdio launch command. Path-dependent
/// adapters (codelldb's extension dir, js-debug's node script) belong in
/// the `[debug.adapters]` config overlay, not in a guessed seed path.
const ADAPTERS_TOML: &str = include_str!("debug_adapters.toml");

#[derive(Deserialize)]
struct AdapterSeed {
    command: String,
}

/// The effective adapter table: embedded seed with the config overlay
/// merged on top (overlay wins).
pub fn adapters(overlay: Option<&BTreeMap<String, String>>) -> BTreeMap<String, String> {
    let seed: BTreeMap<String, AdapterSeed> = toml::from_str(ADAPTERS_TOML).unwrap_or_default();
    let mut out: BTreeMap<String, String> = seed.into_iter().map(|(k, v)| (k, v.command)).collect();
    if let Some(ov) = overlay {
        for (k, v) in ov {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// One live debug session: the adapter child plus the routing state
/// shared with the reader task.
pub struct Session {
    id: String,
    child: Mutex<Child>,
    writer: Arc<tokio::sync::Mutex<Option<ChildStdin>>>,
    seq: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    /// Adapter capabilities off `initialize` (written once, pre-handout).
    capabilities: Mutex<Value>,
    /// Signaled on `stopped` events.
    stopped: Arc<Notify>,
    /// Signaled on `terminated`/`exited` events or reader EOF.
    ended: Arc<Notify>,
    /// Signaled on the `initialized` event (handshake gate).
    initialized: Arc<Notify>,
    /// Bounded adapter console output.
    output: Arc<Mutex<VecDeque<String>>>,
    /// The thread that last stopped (DAP default-thread semantics).
    thread: Mutex<Option<i64>>,
    /// Top frame of the last stackTrace (vars/eval target).
    frame: Mutex<i64>,
    /// The adapter is gone (EOF/terminated) — reap criterion.
    dead: AtomicBool,
    /// Breakpoints as set: file → lines (hand-side bookkeeping).
    breaks: Mutex<Vec<(String, Vec<u64>)>>,
    /// Engine event channel for async stop Notes (None in tests).
    notes: Option<mpsc::Sender<ka_protocol::Event>>,
    /// A hand-side wait (`wait_stop`/`resume`/`pause`) is in flight —
    /// the stop it awaits must not also fire a Note.
    hand_waiting: AtomicBool,
    last_used: Mutex<Instant>,
}

/// Outcome of [`Session::wait_stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// The debuggee hit a breakpoint / finished a step / stopped on entry.
    Stopped,
    /// The debuggee ran to completion or the adapter died.
    Ended,
    /// The cap passed with no stop — the debuggee is simply running.
    Running,
}

impl Session {
    /// Session id (short, hand-facing: `d1`, `d2`, ...).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Adapter capabilities off `initialize`.
    pub fn capabilities(&self) -> Value {
        self.capabilities.lock().clone()
    }

    /// Breakpoints as set: file → lines.
    pub fn breakpoints(&self) -> Vec<(String, Vec<u64>)> {
        self.breaks.lock().clone()
    }

    /// The console ring, oldest first; bounded at [`OUTPUT_LINES`].
    pub fn output_lines(&self) -> Vec<String> {
        self.output.lock().iter().cloned().collect()
    }

    fn touch(&self) {
        *self.last_used.lock() = Instant::now();
    }

    /// Public touch for the hand layer: every action extends the TTL.
    pub fn touch_pub(&self) {
        self.touch();
    }

    fn expired(&self) -> bool {
        self.dead.load(Ordering::Relaxed) || self.last_used.lock().elapsed() > SESSION_TTL
    }

    /// Roster status: `live` while the adapter is connected and not
    /// expired, `ended` once it terminated or was disconnected.
    pub fn status(&self) -> &'static str {
        if self.expired() { "ended" } else { "live" }
    }

    /// Send one request and wait for its (successful) body. DAP has no
    /// separate id field: responses echo the request's `seq` as
    /// `request_seq`, so the pending map is keyed by seq.
    async fn request(&self, command: &str, args: Value) -> Result<Value, String> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(seq, tx);
        let msg = json!({
            "seq": seq,
            "type": "request",
            "command": command,
            "arguments": args,
        });
        {
            let mut w = self.writer.lock().await;
            let Some(w) = w.as_mut() else {
                self.pending.lock().remove(&seq);
                return Err("adapter pipe is closed".to_string());
            };
            let body = serde_json::to_vec(&msg).map_err(|e| format!("serialize: {e}"))?;
            if crate::wire::write_frame(w, &body).await.is_err() {
                self.pending.lock().remove(&seq);
                return Err("adapter pipe is closed".to_string());
            }
        }
        let resp = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) | Err(_) => {
                self.pending.lock().remove(&seq);
                return Err(format!("{command}: adapter did not answer in time"));
            }
        };
        if resp["success"] == json!(false) {
            let message = resp["message"].as_str().unwrap_or("request failed");
            return Err(format!("{command}: {message}"));
        }
        Ok(resp.get("body").cloned().unwrap_or(Value::Null))
    }

    /// Frame and send a request whose response we deliberately do not
    /// wait for — some adapters (debugpy) answer `launch` only after
    /// `configurationDone`, so a synchronous wait would deadlock the
    /// handshake. `configurationDone` (also sent sync) is the ordering
    /// point instead.
    async fn send_request(&self, command: &str, args: Value) -> Result<(), String> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let msg = json!({ "seq": seq, "type": "request", "command": command, "arguments": args });
        let mut w = self.writer.lock().await;
        let Some(w) = w.as_mut() else {
            return Err("adapter pipe is closed".to_string());
        };
        let body = serde_json::to_vec(&msg).map_err(|e| format!("serialize: {e}"))?;
        crate::wire::write_frame(w, &body)
            .await
            .map_err(|_| "adapter pipe is closed".to_string())
    }

    /// The last-stopped thread id, if any.
    fn stopped_thread(&self) -> Result<i64, String> {
        self.thread
            .lock()
            .ok_or_else(|| "no stopped thread — resume or wait for a stop".to_string())
    }

    /// Wait for the debuggee to stop. `Wait::Running` = the cap passed
    /// with no stop (not an error — the debuggee may simply be running).
    pub async fn wait_stop(&self) -> Wait {
        self.hand_waiting.store(true, Ordering::Relaxed);
        let wait = tokio::select! {
            _ = self.stopped.notified() => Wait::Stopped,
            _ = self.ended.notified() => Wait::Ended,
            _ = tokio::time::sleep(STOP_WAIT) => Wait::Running,
        };
        self.hand_waiting.store(false, Ordering::Relaxed);
        wait
    }

    /// Resume execution (`continue`, `next`, `stepIn`, `stepOut`) and
    /// wait for the next stop.
    pub async fn resume(&self, what: &str) -> Result<Wait, String> {
        let thread = self.stopped_thread()?;
        // hold hand_waiting across the request too: a stop landing while
        // the continue response is in flight belongs to this wait, not a
        // Note
        self.hand_waiting.store(true, Ordering::Relaxed);
        if let Err(e) = self.request(what, json!({ "threadId": thread })).await {
            self.hand_waiting.store(false, Ordering::Relaxed);
            return Err(e);
        }
        Ok(self.wait_stop().await)
    }

    async fn stack_frames(&self, levels: u64) -> Result<Vec<Value>, String> {
        let thread = self.stopped_thread()?;
        let body = self
            .request(
                "stackTrace",
                json!({ "threadId": thread, "start": 0, "levels": levels }),
            )
            .await?;
        let frames = body["stackFrames"].as_array().cloned().unwrap_or_default();
        if let Some(top) = frames.first().and_then(|f| f["id"].as_i64()) {
            *self.frame.lock() = top;
        }
        Ok(frames)
    }

    /// Where the debuggee last stopped: `path:line (function)`.
    pub async fn stopped_at(&self) -> Result<String, String> {
        let frames = self.stack_frames(1).await?;
        let frame = frames.first().ok_or("no stack frames")?;
        Ok(format!(
            "{}:{} ({})",
            frame["source"]["path"].as_str().unwrap_or("?"),
            frame["line"].as_u64().unwrap_or(0),
            frame["name"].as_str().unwrap_or("?")
        ))
    }

    /// Stack frames for the last-stopped thread, bounded.
    pub async fn stack(&self, levels: u64) -> Result<Vec<String>, String> {
        let frames = self.stack_frames(levels).await?;
        Ok(frames
            .iter()
            .map(|f| {
                format!(
                    "{}  {}:{}",
                    f["name"].as_str().unwrap_or("?"),
                    f["source"]["path"].as_str().unwrap_or("?"),
                    f["line"].as_u64().unwrap_or(0)
                )
            })
            .collect())
    }

    /// Locals of the top frame, non-expensive scopes first, bounded.
    pub async fn variables(&self, per_scope: usize) -> Result<Vec<String>, String> {
        let frame = *self.frame.lock();
        let body = self.request("scopes", json!({ "frameId": frame })).await?;
        let scopes = body["scopes"].as_array().cloned().unwrap_or_default();
        let mut out = Vec::new();
        for scope in scopes.iter().filter(|s| s["expensive"] != json!(true)) {
            let name = scope["name"].as_str().unwrap_or("?");
            let r = match scope["variablesReference"].as_i64() {
                Some(r) if r != 0 => r,
                _ => continue,
            };
            out.push(format!("{name}:"));
            let vars = self
                .request("variables", json!({ "variablesReference": r }))
                .await?;
            if let Some(items) = vars["variables"].as_array() {
                for v in items.iter().take(per_scope) {
                    out.push(format!(
                        "  {} = {}",
                        v["name"].as_str().unwrap_or("?"),
                        v["value"].as_str().unwrap_or("?")
                    ));
                }
                if items.len() > per_scope {
                    out.push(format!("  (+{} more)", items.len() - per_scope));
                }
            }
            if out.len() > 60 {
                break;
            }
        }
        Ok(out)
    }

    /// Evaluate an expression in the repl context of the top frame.
    pub async fn evaluate(&self, expression: &str) -> Result<String, String> {
        let mut args = json!({ "expression": expression, "context": "repl" });
        let frame = *self.frame.lock();
        if frame != 0 {
            args["frameId"] = json!(frame);
        }
        let body = self.request("evaluate", args).await?;
        Ok(body["result"].as_str().unwrap_or("").to_string())
    }

    /// Threads of the debuggee.
    pub async fn threads(&self) -> Result<Vec<String>, String> {
        let body = self.request("threads", Value::Null).await?;
        Ok(body["threads"]
            .as_array()
            .map(|ts| {
                ts.iter()
                    .map(|t| {
                        format!(
                            "{} {}",
                            t["id"].as_i64().unwrap_or(0),
                            t["name"].as_str().unwrap_or("?")
                        )
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Pause the last-known thread and wait for the resulting stop.
    pub async fn pause(&self) -> Result<Wait, String> {
        let thread = self.stopped_thread()?;
        self.hand_waiting.store(true, Ordering::Relaxed);
        if let Err(e) = self.request("pause", json!({ "threadId": thread })).await {
            self.hand_waiting.store(false, Ordering::Relaxed);
            return Err(e);
        }
        Ok(self.wait_stop().await)
    }

    /// Set breakpoints for one file (replacing that file's set); the
    /// adapter reports back verified locations.
    pub async fn set_breakpoints(&self, file: &str, lines: &[u64]) -> Result<String, String> {
        let bps: Vec<Value> = lines.iter().map(|l| json!({ "line": l })).collect();
        let body = self
            .request(
                "setBreakpoints",
                json!({ "source": { "path": file }, "breakpoints": bps }),
            )
            .await?;
        let rendered: Vec<String> = body["breakpoints"]
            .as_array()
            .map(|bps| {
                bps.iter()
                    .map(|b| {
                        let line = b["line"].as_u64().unwrap_or(0);
                        if b["verified"] == json!(true) {
                            format!("line {line}")
                        } else {
                            format!("line {line} (unverified)")
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        {
            let mut tracked = self.breaks.lock();
            tracked.retain(|(f, _)| f != file);
            if !lines.is_empty() {
                tracked.push((file.to_string(), lines.to_vec()));
            }
        }
        if rendered.is_empty() {
            Ok(format!("{file}: no breakpoints set"))
        } else {
            Ok(format!("{file}: {}", rendered.join(", ")))
        }
    }

    /// Clear every tracked breakpoint.
    pub async fn clear_breakpoints(&self) -> Result<usize, String> {
        let tracked: Vec<(String, Vec<u64>)> = self.breaks.lock().drain(..).collect();
        let n = tracked.len();
        for (file, _) in &tracked {
            self.request(
                "setBreakpoints",
                json!({ "source": { "path": file }, "breakpoints": [] }),
            )
            .await?;
        }
        Ok(n)
    }

    /// Roster rows for the /debug overlay: a header mirroring the /tasks
    /// dap row, one indented line per breakpoint file, then the tail of
    /// the adapter console.
    pub fn roster(&self) -> Vec<String> {
        let bps = self.breakpoints();
        let mut rows = vec![format!(
            "dap {}  {}  breakpoints in {} file(s)",
            self.id(),
            self.status(),
            bps.len()
        )];
        for (file, lines) in bps {
            let lines: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
            rows.push(format!("  {}: lines {}", file, lines.join(",")));
        }
        let out = self.output_lines();
        let start = out.len().saturating_sub(5);
        for line in &out[start..] {
            rows.push(format!("  | {line}"));
        }
        rows
    }

    /// Disconnect (terminate debuggee) and end the session.
    pub async fn disconnect(&self) -> Result<(), String> {
        let _ = self
            .request("disconnect", json!({ "terminateDebuggee": true }))
            .await;
        self.child.lock().start_kill().ok();
        self.dead.store(true, Ordering::Relaxed);
        self.ended.notify_one();
        Ok(())
    }

    /// Abandon a session whose handshake failed: kill the adapter and
    /// mark it dead so `reap` drops it immediately — a half-configured
    /// session must never answer `session(None)` ("newest").
    fn abandon(&self) {
        self.child.lock().start_kill().ok();
        self.dead.store(true, Ordering::Relaxed);
        self.ended.notify_one();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.child.lock().start_kill().ok();
    }
}

/// The session table: ids `d1..`, capped, lazily reaped.
pub struct DebugManager {
    adapters: Option<BTreeMap<String, String>>,
    /// Engine event channel for async stop Notes (cloned into each
    /// session; None in tests).
    notes: Option<mpsc::Sender<ka_protocol::Event>>,
    sessions: Mutex<Vec<Arc<Session>>>,
    counter: AtomicU64,
}

impl DebugManager {
    /// `notes` receives async stop Notes for stops no hand is waiting
    /// on (None keeps the manager silent — tests).
    pub fn new(
        overlay: Option<BTreeMap<String, String>>,
        notes: Option<mpsc::Sender<ka_protocol::Event>>,
    ) -> Self {
        Self {
            adapters: overlay,
            notes,
            sessions: Mutex::new(Vec::new()),
            counter: AtomicU64::new(1),
        }
    }

    /// Roster rows across live sessions; a hint row when none.
    pub fn roster(&self) -> Vec<String> {
        let sessions = self.sessions();
        if sessions.is_empty() {
            return vec!["no debug sessions (debug action \"start\")".to_string()];
        }
        sessions.iter().flat_map(|s| s.roster()).collect()
    }

    /// Live sessions, oldest first (reaps dead/expired first).
    pub fn sessions(&self) -> Vec<Arc<Session>> {
        self.reap();
        self.sessions.lock().clone()
    }

    /// Look a session up by id (the hand's `session` argument; None =
    /// the newest).
    pub fn session(&self, id: Option<&str>) -> Result<Arc<Session>, String> {
        self.reap();
        let sessions = self.sessions.lock();
        match id {
            Some(id) => sessions
                .iter()
                .find(|s| s.id() == id)
                .cloned()
                .ok_or_else(|| format!("no debug session {id:?}")),
            None => sessions
                .last()
                .cloned()
                .ok_or_else(|| "no debug session — action \"start\" first".to_string()),
        }
    }

    /// Remove one session (the disconnect action's bookkeeping).
    pub fn remove(&self, id: &str) {
        self.reap();
        self.sessions.lock().retain(|s| s.id() != id);
    }

    fn reap(&self) {
        self.sessions.lock().retain(|s| !s.expired());
    }

    /// Resolve an adapter name to its stdio command.
    pub fn adapter_command(&self, name: &str) -> Result<String, String> {
        let table = adapters(self.adapters.as_ref());
        table.get(name).cloned().ok_or_else(|| {
            format!(
                "unknown adapter {name:?} — known: {} (add more via [debug.adapters])",
                table.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
    }

    /// Spawn an adapter and run the handshake: initialize →
    /// (`initialized` event) → breakpoints → configurationDone. `launch`
    /// picks launch vs attach; `launch_args` is the adapter's launch /
    /// attach arguments verbatim (program, pid, ...).
    pub async fn start(
        &self,
        adapter: &str,
        launch: bool,
        launch_args: Value,
        breakpoints: &[(String, Vec<u64>)],
        cwd: &std::path::Path,
    ) -> Result<Arc<Session>, String> {
        self.reap();
        if self.sessions.lock().len() >= MAX_SESSIONS {
            return Err(format!(
                "live debug sessions at the cap ({MAX_SESSIONS}) — disconnect one first"
            ));
        }
        let command = self.adapter_command(adapter)?;
        let mut words = command.split_whitespace();
        let bin = words
            .next()
            .ok_or_else(|| format!("adapter {adapter:?} has an empty command"))?;
        let mut cmd = Command::new(bin);
        cmd.args(words)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn {bin}: {e} (is the adapter installed and on PATH?)"))?;
        let stdin = child.stdin.take().ok_or("adapter stdin unavailable")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("adapter stdout unavailable")?);
        let id = format!("d{}", self.counter.fetch_add(1, Ordering::Relaxed));
        let session = Arc::new(Session {
            id: id.clone(),
            child: Mutex::new(child),
            writer: Arc::new(tokio::sync::Mutex::new(Some(stdin))),
            seq: AtomicI64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            capabilities: Mutex::new(Value::Null),
            stopped: Arc::new(Notify::new()),
            ended: Arc::new(Notify::new()),
            initialized: Arc::new(Notify::new()),
            output: Arc::new(Mutex::new(VecDeque::new())),
            thread: Mutex::new(None),
            frame: Mutex::new(0),
            dead: AtomicBool::new(false),
            breaks: Mutex::new(Vec::new()),
            notes: self.notes.clone(),
            hand_waiting: AtomicBool::new(false),
            last_used: Mutex::new(Instant::now()),
        });
        tokio::spawn(read_loop(stdout, session.clone()));
        self.sessions.lock().push(session.clone());

        // handshake — on failure, abandon: the half-configured session
        // must never answer `session(None)` ("newest")
        if let Err(e) = handshake(&session, adapter, launch, launch_args, breakpoints).await {
            session.abandon();
            return Err(e);
        }
        session.touch();
        Ok(session)
    }
}

/// The handshake tail of [`DebugManager::start`]: initialize →
/// launch/attach (fire-and-forget) → `initialized` event →
/// breakpoints → configurationDone.
async fn handshake(
    session: &Session,
    adapter: &str,
    launch: bool,
    launch_args: Value,
    breakpoints: &[(String, Vec<u64>)],
) -> Result<(), String> {
    let caps = session
        .request(
            "initialize",
            json!({
                "clientID": "ka",
                "clientName": "ka",
                "adapterID": adapter,
                "locale": "en",
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "pathFormat": "path",
                "supportsVariableType": true,
                "supportsRunInTerminalRequest": false,
            }),
        )
        .await?;
    *session.capabilities.lock() = caps;
    // launch/attach goes out immediately after the initialize
    // response — VS Code's order, and required by adapters (debugpy)
    // that only emit `initialized` once the debuggee is started. Its
    // RESPONSE is deferred until configurationDone by such adapters,
    // so the request is fire-and-forget and configurationDone below
    // is the synchronization point.
    let method = if launch { "launch" } else { "attach" };
    session.send_request(method, launch_args).await?;
    if tokio::time::timeout(READY_TIMEOUT, session.initialized.notified())
        .await
        .is_err()
    {
        return Err(format!("{adapter}: no `initialized` event in time"));
    }
    // The client does NOT echo an `initialized` event: debugpy treats
    // a client-sent one as "start the debuggee now", which would run
    // the program before breakpoints are registered. setBreakpoints
    // is legal as soon as the event above arrives.
    for (file, lines) in breakpoints {
        session.set_breakpoints(file, lines).await?;
    }
    session.request("configurationDone", Value::Null).await?;
    Ok(())
}

/// Reader task: routes responses by `request_seq`, collects console
/// output into the bounded ring, signals stop/end/initialized events,
/// and refuses reverse requests with an instructive failure — an
/// adapter awaiting that response (runInTerminal) would otherwise hang.
async fn read_loop<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut reader: R,
    session: Arc<Session>,
) {
    loop {
        let Some(body) = crate::wire::read_frame(&mut reader).await else {
            session.dead.store(true, Ordering::Relaxed);
            session.ended.notify_one();
            return;
        };
        let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
            continue;
        };
        match msg["type"].as_str() {
            Some("response") => {
                if let Some(id) = msg["request_seq"].as_i64() {
                    if let Some(tx) = session.pending.lock().remove(&id) {
                        let _ = tx.send(msg);
                    }
                }
            }
            Some("event") => match msg["event"].as_str() {
                Some("stopped") => {
                    if let Some(t) = msg["body"]["threadId"].as_i64() {
                        // kept across `continued` too: resume/pause target
                        // the last-known thread, and it is the same one
                        *session.thread.lock() = Some(t);
                    }
                    session.stopped.notify_one();
                    // async stop note: fires only when no hand-side wait
                    // (`wait_stop`/`resume`/`pause`) is mid-flight; the
                    // note task releases the latch once it is out.
                    if session.notes.is_some()
                        && !session.hand_waiting.swap(true, Ordering::Relaxed)
                    {
                        let stopped = session.clone();
                        if let Some(notes) = stopped.notes.clone() {
                            tokio::spawn(async move {
                                let id = stopped.id().to_string();
                                let message = match stopped.stopped_at().await {
                                    Ok(at) => format!("debug {id} stopped at {at}"),
                                    Err(_) => format!("debug {id} stopped"),
                                };
                                let message: String = message.chars().take(200).collect();
                                notes.send(ka_protocol::Event::Note { message }).await.ok();
                                // the note is out: further async stops may
                                // note again (the next hand wait re-arms the
                                // latch through its own store)
                                stopped.hand_waiting.store(false, Ordering::Relaxed);
                            });
                        }
                    }
                }
                Some("output") => {
                    let text = msg["body"]["output"].as_str().unwrap_or("");
                    let mut ring = session.output.lock();
                    for chunk in text.split('\n').filter(|s| !s.is_empty()) {
                        let mut owned = chunk.to_string();
                        owned.truncate(OUTPUT_LINE_CAP);
                        if ring.len() >= OUTPUT_LINES {
                            ring.pop_front();
                        }
                        ring.push_back(owned);
                    }
                }
                Some("terminated") | Some("exited") => {
                    session.dead.store(true, Ordering::Relaxed);
                    session.ended.notify_one();
                }
                Some("initialized") => {
                    session.initialized.notify_one();
                }
                _ => {}
            },
            Some("request") => {
                // reverse request: refuse everything — runInTerminal
                // (console external) and any other server pull
                let seq = session.seq.fetch_add(1, Ordering::Relaxed);
                let command = msg["command"].as_str().unwrap_or("?");
                let reply = json!({
                    "seq": seq,
                    "type": "response",
                    "request_seq": msg["seq"].as_i64().unwrap_or(0),
                    "success": false,
                    "command": command,
                    "message": format!(
                        "ka does not implement the reverse request {command:?} \
                         (the debug console is external)"
                    ),
                });
                let mut w = session.writer.lock().await;
                if let Some(w) = w.as_mut() {
                    if let Ok(bytes) = serde_json::to_vec(&reply) {
                        let _ = crate::wire::write_frame(w, &bytes).await;
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::path::Path;

    #[test]
    fn embedded_catalog_parses_and_covers_the_seed() {
        let table = adapters(None);
        for name in ["gdb", "lldb", "debugpy", "dlv", "netcoredbg", "codelldb"] {
            assert!(table.contains_key(name), "missing seed adapter {name}");
            assert!(!table[name].is_empty());
        }
    }

    #[test]
    fn overlay_wins_over_seed() {
        let mut ov = BTreeMap::new();
        ov.insert("gdb".to_string(), "/custom/gdb -i dap".to_string());
        ov.insert("mine".to_string(), "myadapter".to_string());
        let table = adapters(Some(&ov));
        assert_eq!(table["gdb"], "/custom/gdb -i dap");
        assert_eq!(table["mine"], "myadapter");
        assert_eq!(table["lldb"], "lldb-dap", "seed entries survive");
    }

    #[test]
    fn unknown_adapter_names_the_known_set() {
        let mgr = DebugManager::new(None, None);
        let err = mgr.adapter_command("nope").unwrap_err();
        assert!(
            err.contains("gdb") && err.contains("[debug.adapters]"),
            "{err}"
        );
    }

    /// A fake DAP adapter (python, stdio framing): full handshake,
    /// launch with a runInTerminal pull the client must refuse,
    /// breakpoint set → configurationDone → stopped, canned
    /// stack/vars/eval, a second stop on continue, then exit.
    #[cfg(unix)]
    fn fake_dap(dir: &std::path::Path) -> DebugManager {
        let fake = dir.join("fake.py");
        let root = dir.display().to_string().replace('\\', "/");
        std::fs::write(
            &fake,
            format!(
                r#"import sys, json
root = {root:?}
state = {{"hits": 0, "line": 7}}

def send(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(b)).encode() + b)
    sys.stdout.buffer.flush()

def read_msg():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b"\r\n", b"\n"):
            if length is not None:
                return json.loads(sys.stdin.buffer.read(length))
            continue
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":")[1].strip())

def reply(msg, body=None):
    send({{"seq": 0, "type": "response", "request_seq": msg["seq"], "success": True,
          "command": msg["command"], "body": body}})

def event(name, body=None):
    send({{"seq": 0, "type": "event", "event": name, "body": body or {{}}}})

while True:
    msg = read_msg()
    if msg.get("type") == "response" and msg.get("command") == "runInTerminal":
        # the client refused our reverse request (probe verified)
        refused = msg.get("success") is False and "ka" in (msg.get("message") or "")
        event("output", {{"output": "ka_rejected=%s\\n" % str(refused).lower()}})
        continue
    if msg.get("type") != "request":
        continue
    cmd = msg.get("command")
    if cmd == "initialize":
        reply(msg, {{"supportsConfigurationDoneRequest": True}})
        event("initialized")
    elif cmd == "launch":
        # pull a runInTerminal reverse request: the client must refuse
        # it (failure response, console external) and keep going. The
        # probe response is handled in the main loop — launch itself is
        # answered immediately (a real adapter defers it, but the client
        # sends launch fire-and-forget either way).
        send({{"seq": 0, "type": "request", "command": "runInTerminal",
              "arguments": {{"kind": "external"}}}})
        reply(msg)
    elif cmd == "setBreakpoints":
        lines = [b["line"] for b in msg["arguments"]["breakpoints"]]
        reply(msg, {{"breakpoints": [{{"verified": True, "line": l}} for l in lines]}})
    elif cmd == "configurationDone":
        reply(msg)
        event("stopped", {{"reason": "breakpoint", "threadId": 1}})
    elif cmd == "continue":
        state["hits"] += 1
        if state["hits"] == 1:
            state["line"] = 9
            reply(msg)
            event("stopped", {{"reason": "step", "threadId": 1}})
        else:
            reply(msg)
            event("exited", {{"exitCode": 0}})
            event("terminated")
    elif cmd == "threads":
        reply(msg, {{"threads": [{{"id": 1, "name": "main"}}]}})
    elif cmd == "stackTrace":
        reply(msg, {{"stackFrames": [{{"id": 100, "name": "main",
            "source": {{"path": root + "/x.py"}}, "line": state["line"]}}]}})
    elif cmd == "scopes":
        reply(msg, {{"scopes": [{{"name": "Locals", "variablesReference": 1000, "expensive": False}}]}})
    elif cmd == "variables":
        reply(msg, {{"variables": [{{"name": "x", "value": "1"}}]}})
    elif cmd == "evaluate":
        reply(msg, {{"result": "42"}})
    elif cmd == "disconnect":
        reply(msg)
        event("terminated")
    else:
        reply(msg)
"#
            ),
        )
        .unwrap();
        let mut ov = BTreeMap::new();
        ov.insert("fake".to_string(), format!("python3 {}", fake.display()));
        DebugManager::new(Some(ov), None)
    }

    #[cfg(unix)]
    fn which_python3() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn dap_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-dap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The full probe loop: handshake, launch (with the runInTerminal
    /// refusal surviving), breakpoint stop, stack/vars/eval inspection,
    /// continue → second stop, continue → exit, disconnect.
    #[cfg(unix)]
    #[tokio::test]
    async fn dap_round_trip_stop_inspect_resume_exit() {
        if !which_python3() {
            return;
        }
        let dir = dap_dir("trip");
        let mgr = fake_dap(&dir);
        let breaks = vec![(format!("{}/x.py", dir.display()), vec![7u64])];
        let s = mgr
            .start(
                "fake",
                true,
                json!({ "program": format!("{}/x.py", dir.display()) }),
                &breaks,
                &dir,
            )
            .await
            .unwrap();
        // the configurationDone stop arrives async — wait for it
        assert_eq!(s.wait_stop().await, Wait::Stopped);
        assert_eq!(
            s.stopped_at().await.unwrap(),
            format!("{}/x.py:7 (main)", dir.display()),
            "the breakpoint stop is the first stop"
        );
        let frames = s.stack(10).await.unwrap();
        assert!(frames[0].contains("main"), "{frames:?}");
        let vars = s.variables(20).await.unwrap();
        assert!(vars.iter().any(|v| v.contains("x = 1")), "{vars:?}");
        assert_eq!(s.evaluate("6*7").await.unwrap(), "42");
        // the runInTerminal pull was refused without hanging
        let out = s.output_lines().join("\n");
        assert!(out.contains("ka_rejected=true"), "{out}");
        // continue → second stop at line 9
        assert_eq!(s.resume("continue").await.unwrap(), Wait::Stopped);
        assert!(
            s.stopped_at().await.unwrap().ends_with(":9 (main)"),
            "second stop lands on the step line"
        );
        // continue → the fake exits the debuggee
        assert_eq!(s.resume("continue").await.unwrap(), Wait::Ended);
        s.disconnect().await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// roster(): header + indented breakpoint row + `| `-prefixed
    /// console tail — the /debug overlay rows.
    #[cfg(unix)]
    #[tokio::test]
    async fn dap_roster_renders_breakpoints_and_output_tail() {
        if !which_python3() {
            return;
        }
        let dir = dap_dir("roster");
        let mgr = fake_dap(&dir);
        let file = format!("{}/x.py", dir.display());
        let s = mgr
            .start(
                "fake",
                true,
                json!({ "program": file.clone() }),
                &[(file.clone(), vec![7u64])],
                &dir,
            )
            .await
            .unwrap();
        assert_eq!(s.wait_stop().await, Wait::Stopped);
        let roster = s.roster().join("\n");
        assert!(
            roster.contains(&format!("dap {}  live  breakpoints in 1 file(s)", s.id())),
            "{roster}"
        );
        assert!(roster.contains(&format!("  {file}: lines 7")), "{roster}");
        assert!(roster.contains("| ka_rejected=true"), "{roster}");
        s.disconnect().await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Disconnect removes the session; `session()` reports it gone.
    #[cfg(unix)]
    #[tokio::test]
    async fn dap_disconnect_removes_the_session() {
        if !which_python3() {
            return;
        }
        let dir = dap_dir("disc");
        let mgr = fake_dap(&dir);
        let s = mgr
            .start("fake", true, json!({ "program": "/w/x.py" }), &[], &dir)
            .await
            .unwrap();
        let id = s.id().to_string();
        assert_eq!(mgr.sessions().len(), 1);
        s.disconnect().await.unwrap();
        mgr.remove(&id);
        assert!(mgr.sessions().is_empty(), "the dead session is reaped");
        assert!(mgr.session(None).is_err(), "no newest session remains");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A broken adapter command fails the start with the install hint.
    #[tokio::test]
    async fn dap_spawn_failure_is_instructive() {
        let mut ov = BTreeMap::new();
        ov.insert(
            "broken".to_string(),
            "ka-no-such-adapter-binary --dap".to_string(),
        );
        let mgr = DebugManager::new(Some(ov), None);
        let err = match mgr
            .start(
                "broken",
                true,
                json!({ "program": "x" }),
                &[],
                Path::new("/tmp"),
            )
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("a broken adapter command must not start"),
        };
        assert!(
            err.contains("is the adapter installed"),
            "spawn failure names the binary and the hint: {err}"
        );
    }

    /// REAL third-party adapter dogfood (roadmap 8.2): drive Microsoft's
    /// debugpy over stdio — launch a python script, break, read the
    /// stack/variables, evaluate, resume to exit. Skips when debugpy is
    /// not installed.
    #[cfg(unix)]
    #[tokio::test]
    async fn dap_debugpy_real_adapter_round_trip() {
        if !which_python3() {
            return;
        }
        // debugpy present?
        if std::process::Command::new("python3")
            .args(["-m", "debugpy", "--version"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            return;
        }
        let dir = dap_dir("debugpy");
        let script = dir.join("sample.py");
        std::fs::write(
            &script,
            "import sys\n\n\ndef work(n):\n    total = n + 1\n    return total\n\n\nwork(41)\n",
        )
        .unwrap();
        let mut ov = BTreeMap::new();
        ov.insert(
            "debugpy".to_string(),
            "python3 -m debugpy.adapter".to_string(),
        );
        let mgr = DebugManager::new(Some(ov), None);
        let breaks = vec![(script.display().to_string(), vec![5u64])];
        let s = mgr
            .start(
                "debugpy",
                true,
                json!({
                    "program": script.display().to_string(),
                    "console": "internalConsole",
                    // stop at entry: the debuggee cannot race past the
                    // breakpoint on a script this small, and the entry
                    // stop is a deterministic first inspection point
                    "stopOnEntry": true
                }),
                &breaks,
                &dir,
            )
            .await
            .unwrap();
        // entry stop fires when configurationDone releases the debuggee
        assert_eq!(s.wait_stop().await, Wait::Stopped);
        // resume → runs to the line-5 breakpoint inside work()
        assert_eq!(s.resume("continue").await.unwrap(), Wait::Stopped);
        let frames = s.stack(10).await.unwrap();
        assert!(
            frames.iter().any(|f| f.contains("work")),
            "the work frame is on the stack: {frames:?}"
        );
        let vars = s.variables(20).await.unwrap();
        assert!(
            vars.iter().any(|v| v.contains("n = 41")),
            "frame locals carry the argument: {vars:?}"
        );
        assert_eq!(s.evaluate("n * 2").await.unwrap(), "82");
        assert_eq!(s.resume("continue").await.unwrap(), Wait::Ended);
        s.disconnect().await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// REAL second-adapter dogfood (roadmap 8.3): lldb's DAP adapter
    /// over a native C binary — entry stop, a verified line-2
    /// breakpoint, exit code 0. Ubuntu's LLVM ≤ 16 ships the adapter
    /// as `lldb-vscode` (the pre-rename binary); newer releases call
    /// it `lldb-dap`. Skips when no adapter binary or `cc` exists.
    #[cfg(unix)]
    #[tokio::test]
    async fn dap_lldb_real_adapter_round_trip() {
        let adapter = ["lldb-dap", "lldb-dap-14", "lldb-vscode-14"]
            .into_iter()
            .find(|bin| {
                std::process::Command::new(bin)
                    .arg("--help")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success())
            });
        let Some(adapter) = adapter else {
            return;
        };
        if !std::process::Command::new("cc")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            return;
        }
        let dir = dap_dir("lldb");
        let src = dir.join("main.c");
        std::fs::write(
            &src,
            "int main(void) {\n    int n = 41;\n    n = n + 1;\n    return n - 42;\n}\n",
        )
        .unwrap();
        let exe = dir.join("main");
        let built = std::process::Command::new("cc")
            .args(["-g", "-O0"])
            .arg("-o")
            .arg(&exe)
            .arg(&src)
            .status()
            .unwrap();
        assert!(built.success(), "cc -g -O0 must build the fixture");
        let mut ov = BTreeMap::new();
        ov.insert("lldb-dap".to_string(), adapter.to_string());
        let mgr = DebugManager::new(Some(ov), None);
        let breaks = vec![(src.display().to_string(), vec![2u64])];
        let s = mgr
            .start(
                "lldb-dap",
                true,
                json!({
                    "program": exe.display().to_string(),
                    "stopOnEntry": true
                }),
                &breaks,
                &dir,
            )
            .await
            .unwrap();
        // entry stop: the debuggee is released by configurationDone
        assert_eq!(s.wait_stop().await, Wait::Stopped);
        assert!(
            s.stopped_at().await.unwrap().contains("main"),
            "the entry stop is inside main"
        );
        // continue → the verified line-2 breakpoint
        assert_eq!(s.resume("continue").await.unwrap(), Wait::Stopped);
        assert!(
            s.stopped_at().await.unwrap().contains(":2"),
            "second stop is the line-2 breakpoint"
        );
        // continue → the program returns 0 and ends
        assert_eq!(s.resume("continue").await.unwrap(), Wait::Ended);
        s.disconnect().await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn failed_handshake_leaves_no_half_configured_session() {
        // a spawn failure returns pre-push: nothing is registered
        let mut ov = BTreeMap::new();
        ov.insert(
            "ghost".to_string(),
            "/nonexistent/ghost-adapter".to_string(),
        );
        let mgr = DebugManager::new(Some(ov), None);
        let Err(err) = mgr
            .start(
                "ghost",
                true,
                json!({ "program": "x" }),
                &[],
                Path::new("."),
            )
            .await
        else {
            panic!("spawning a nonexistent adapter must fail");
        };
        assert!(err.contains("spawn"), "{err}");
        assert!(
            mgr.session(None).is_err(),
            "spawn failure must not register"
        );

        // a handshake failure after the push must abandon the session:
        // dead ⇒ reaped ⇒ never answers `session(None)` ("newest")
        let mgr = DebugManager::new(None, None);
        let session = Arc::new(Session {
            id: "d1".to_string(),
            child: Mutex::new(
                tokio::process::Command::new("sleep")
                    .arg("30")
                    .spawn()
                    .unwrap(),
            ),
            writer: Arc::new(tokio::sync::Mutex::new(None)),
            seq: AtomicI64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            capabilities: Mutex::new(Value::Null),
            stopped: Arc::new(Notify::new()),
            ended: Arc::new(Notify::new()),
            initialized: Arc::new(Notify::new()),
            output: Arc::new(Mutex::new(VecDeque::new())),
            thread: Mutex::new(None),
            frame: Mutex::new(0),
            dead: AtomicBool::new(false),
            breaks: Mutex::new(Vec::new()),
            notes: None,
            hand_waiting: AtomicBool::new(false),
            last_used: Mutex::new(Instant::now()),
        });
        mgr.sessions.lock().push(session.clone());
        assert!(mgr.session(None).is_ok(), "live before the abandon");
        session.abandon();
        let Err(err) = mgr.session(None) else {
            panic!("an abandoned session must not answer session(None)");
        };
        assert!(err.contains("no debug session"), "{err}");
    }
}
