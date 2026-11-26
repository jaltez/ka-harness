//! `ka serve`: HTTP/SSE server mode. Hand-rolled HTTP/1.1 on a tokio
//! TcpListener (request line + headers + Content-Length body, one
//! connection per request). Routes:
//!
//! - `GET /health` → `{"ok":true}`
//! - `POST /sessions` → spawn an engine → `{"id":"<32 hex chars>"}`;
//!   optional body `{"resume":"latest"}` or `{"resume":"<strand id/prefix>"}`
//!   attaches to a strand on disk (replayed over SSE)
//! - `GET /sessions` → `[{"id","busy","attached","title"}, …]`
//! - `GET /sessions/{id}` → `{"id","busy","attached","title"}`
//! - `POST /sessions/{id}/prompt` `{"text":...,"schema":...}` → queues
//!   the turn → `{"ok":true}` (the single writer path — observers have
//!   no write route)
//! - `GET /sessions/{id}/events` → SSE stream of ka events as
//!   `data:` NDJSON lines, each with an `id:` sequence number. Up to
//!   `MAX_SUBSCRIBERS_PER_SESSION` concurrent subscribers per session
//!   (roadmap 9.6: observe-only multi-client); `Last-Event-ID` replays
//!   the per-session ring buffer from the next event. Presence changes
//!   broadcast a `{"type":"presence", ...}` event so attached clients
//!   (and the attach UI) can show `busy · N attached`.
//!
//! Every POST body must carry `Content-Type: application/json`
//! (415 otherwise) — the header is what forces a CORS preflight, so a
//! web page cannot drive the loopback server. Session creation is
//! capped at `MAX_SESSIONS` (429) and concurrent connections at
//! `MAX_CONCURRENT_CONNECTIONS` (503).
//!
//! `--token T` enables bearer-token auth on every route; without it the
//! server binds loopback only (refused otherwise). Both ends honor the
//! `KA_SERVE_TOKEN` env var as the token fallback.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ka_engine::{StrandChoice, spawn_full};
use ka_protocol::{Command, ErrorClass, Event};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, mpsc};

/// How many recent engine events each session keeps for late
/// subscribers / reconnects. The startup `Event::Replay` is pinned in
/// addition to this cap (never evicted), so a fresh attach always sees
/// full history no matter how busy the session has been.
const RING_CAP: usize = 512;

/// Maximum concurrent TCP connections accepted by `ka serve`.
const MAX_CONCURRENT_CONNECTIONS: usize = 64;

/// Maximum live sessions kept in memory.
const MAX_SESSIONS: usize = 32;

/// Maximum concurrent SSE subscribers per session.
const MAX_SUBSCRIBERS_PER_SESSION: usize = 16;

/// Monotonic session creation order (for stable `GET /sessions` sorting);
/// session IDs themselves are CSPRNG hex strings, never sequential.
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a 128-bit random hex session ID (32 lowercase hex chars).
/// Uses `/dev/urandom` on Unix and OS-seeded `RandomState` + SHA-256 as a
/// portable fallback (e.g. Windows).
fn new_session_id() -> String {
    let mut bytes = [0u8; 16];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_ok();
    if !filled {
        use sha2::{Digest, Sha256};
        use std::hash::{BuildHasher, Hasher};
        let mut h = Sha256::new();
        for _ in 0..4 {
            let state = std::collections::hash_map::RandomState::new();
            h.update(state.build_hasher().finish().to_le_bytes());
        }
        h.update(
            SESSION_COUNTER
                .fetch_add(1, Ordering::Relaxed)
                .to_le_bytes(),
        );
        let digest = h.finalize();
        bytes.copy_from_slice(&digest[..16]);
    }
    let mut out = String::with_capacity(32);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Constant-time byte slice equality (prevents timing leaks on bearer tokens).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// True iff `host` (with optional `:port`) is a loopback host (`127.0.0.1`,
/// `localhost`, or `[::1]`).
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        let Some((addr, after)) = rest.split_once(']') else {
            return false;
        };
        if addr != "::1" {
            return false;
        }
        return match after.strip_prefix(':') {
            None => after.is_empty(),
            Some(port) => !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()),
        };
    }
    let (name, port) = match host.split_once(':') {
        Some((n, p)) => (n, Some(p)),
        None => (host, None),
    };
    if let Some(p) = port {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1"
}

/// True iff `origin` is an `http://` or `https://` origin whose authority
/// host is loopback (`127.0.0.1`, `localhost`, `[::1]`). Rejects `null`,
/// `file://`, userinfo (`@`), and any non-loopback host.
fn is_loopback_origin(origin: &str) -> bool {
    let rest = origin
        .trim()
        .strip_prefix("http://")
        .or_else(|| origin.trim().strip_prefix("https://"));
    let Some(rest) = rest else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    is_loopback_host(authority)
}

struct Session {
    created_order: u64,
    commands: mpsc::Sender<Command>,
    /// Fan-out to every SSE subscriber (plus the ring buffer writer);
    /// each payload carries the event's own seq.
    broadcast: tokio::sync::broadcast::Sender<(u64, Event)>,
    /// (seq, event) ring of recent events for replay.
    ring: Mutex<Vec<(u64, Event)>>,
    /// Highest sequence number emitted for this session.
    seq: AtomicU64,
    /// Live SSE subscribers right now (presence).
    attached: AtomicUsize,
    /// A turn is in flight (TurnStarted seen, Idle not yet).
    busy: AtomicBool,
    /// Last known title (for `GET /sessions` listings).
    title: Mutex<String>,
}

impl Session {
    fn snapshot(&self) -> (bool, usize) {
        (
            self.busy.load(Ordering::Relaxed),
            self.attached.load(Ordering::Relaxed),
        )
    }

    fn broadcast_presence(&self) {
        let (busy, attached) = self.snapshot();
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.broadcast.send((seq, presence_event(busy, attached)));
    }

    /// Engine stream closed mid-turn: `/sessions` must not report
    /// busy forever — clear and publish one last presence.
    fn close_fixup(&self) {
        if self.busy.swap(false, Ordering::Relaxed) {
            self.broadcast_presence();
        }
    }
}

/// The synthetic presence event (roadmap 9.6). Additive on the wire;
/// surfaces that predate it ignore it.
fn presence_event(busy: bool, attached: usize) -> Event {
    Event::Presence { busy, attached }
}

/// Entry: bind and serve until the process is stopped.
pub async fn run(
    addr: &str,
    token: Option<String>,
    configs: &[std::path::PathBuf],
) -> Result<ExitCode, String> {
    // env fallback, mirroring `ka attach` so KA_SERVE_TOKEN works on
    // both ends of the connection
    let token = token.or_else(|| {
        std::env::var("KA_SERVE_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
    });
    // doc-enforced invariant: an unauthenticated server may only bind
    // a loopback address
    if token.is_none() && !binds_loopback(addr)? {
        return Err(format!(
            "refusing to bind {addr} without a token: pass --token \
             (or set KA_SERVE_TOKEN) to serve on a non-loopback address"
        ));
    }
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    eprintln!("ka serve listening on {addr}");
    serve(listener, token, configs.to_vec(), None).await;
    Ok(ExitCode::SUCCESS)
}

/// Does every address `host:port` resolves to lie on loopback?
fn binds_loopback(addr: &str) -> Result<bool, String> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<_> = addr
        .to_socket_addrs()
        .map_err(|e| format!("resolve {addr}: {e}"))?
        .collect();
    Ok(!addrs.is_empty() && addrs.iter().all(|sa| sa.ip().is_loopback()))
}

async fn serve(
    listener: tokio::net::TcpListener,
    token: Option<String>,
    configs: Vec<std::path::PathBuf>,
    base: Option<ka_engine::config::Config>,
) {
    let configs = Arc::new(configs);
    let base = Arc::new(base);
    let sessions: Arc<Mutex<std::collections::HashMap<String, Arc<Session>>>> = Arc::default();
    let conn_sem = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    let mut next_id = 0u64;
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let Ok(permit) = conn_sem.clone().try_acquire_owned() else {
            // answer instead of dropping: a silent close looks like a hang
            tokio::spawn(async move {
                let mut socket = socket;
                let body = r#"{"error":"too many connections"}"#;
                let resp = format!(
                    "HTTP/1.1 503 Service Unavailable\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
            continue;
        };
        next_id += 1;
        tokio::spawn(handle_connection(
            socket,
            Arc::clone(&sessions),
            token.clone(),
            Arc::clone(&configs),
            Arc::clone(&base),
            next_id,
            permit,
        ));
    }
}

struct Request {
    method: String,
    path: String,
    last_event_id: Option<u64>,
    bearer: Option<String>,
    origin: Option<String>,
    host: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

/// Case-insensitive RFC 9110 header lookup in the request head (skipping
/// the request line).
fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// Read one request: bounded (1 MB), total-deadline-guarded (15 s across
/// all reads), UTF-8 tolerant in the body, Content-Length parsed from
/// the head only.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Request> {
    use tokio::io::AsyncReadExt;
    /// One request (headers + body) may not exceed this.
    const MAX_REQUEST: usize = 1024 * 1024;
    /// Total time allowed to receive the entire request (slowloris bound).
    const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

    let deadline = tokio::time::Instant::now() + READ_TIMEOUT;
    // byte-exact head boundary hoisted out of the loop: lossy-converted
    // head text must never be used to index the raw buffer
    let mut head_end;
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        if buf.len() > MAX_REQUEST {
            return None;
        }
        let n = match tokio::time::timeout_at(deadline, socket.read(&mut tmp)).await {
            Err(_) => return None, // total read deadline exceeded
            Ok(Ok(0)) | Ok(Err(_)) => return None,
            Ok(Ok(n)) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
        // the head boundary is a BYTE pattern: the body may be non-UTF8
        let Some(end) = find_subslice(&buf, b"\r\n\r\n") else {
            continue;
        };
        head_end = end;
        // Content-Length is taken from the HEAD only — a body line
        // containing `content-length:` must not stall the read
        let head = String::from_utf8_lossy(&buf[..head_end]);
        let cl = header_value(&head, "content-length")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        if cl > MAX_REQUEST {
            return None;
        }
        if buf.len() >= head_end + 4 + cl {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let body = buf[head_end + 4..].to_vec();
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let last_event_id = header_value(&head, "last-event-id").and_then(|v| v.parse::<u64>().ok());
    let bearer = header_value(&head, "authorization").and_then(|v| {
        let (scheme, tok) = v.split_once(char::is_whitespace)?;
        scheme
            .eq_ignore_ascii_case("bearer")
            .then(|| tok.trim().to_string())
    });
    let origin = header_value(&head, "origin").map(str::to_string);
    let host = header_value(&head, "host").map(str::to_string);
    let content_type = header_value(&head, "content-type").map(str::to_string);
    Some(Request {
        method,
        path,
        last_event_id,
        bearer,
        origin,
        host,
        content_type,
        body,
    })
}

async fn handle_connection(
    mut socket: tokio::net::TcpStream,
    sessions: Arc<Mutex<std::collections::HashMap<String, Arc<Session>>>>,
    token: Option<String>,
    configs: Arc<Vec<std::path::PathBuf>>,
    base: Arc<Option<ka_engine::config::Config>>,
    _conn_id: u64,
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
    let Some(req) = read_request(&mut socket).await else {
        return;
    };

    // bearer token gate (constant-time comparison)
    if let Some(expected) = &token {
        let ok = req
            .bearer
            .as_deref()
            .is_some_and(|got| ct_eq(got.as_bytes(), expected.as_bytes()));
        if !ok {
            respond(&mut socket, 401, r#"{"error":"unauthorized"}"#).await;
            return;
        }
    } else {
        // Unauthenticated loopback mode: defend against DNS rebinding (Host)
        // and cross-origin browser-to-localhost CSRF (Origin).
        if req.host.as_deref().is_some_and(|h| !is_loopback_host(h)) {
            respond(&mut socket, 403, r#"{"error":"forbidden host"}"#).await;
            return;
        }
        if req
            .origin
            .as_deref()
            .is_some_and(|o| !is_loopback_origin(o))
        {
            respond(&mut socket, 403, r#"{"error":"forbidden origin"}"#).await;
            return;
        }
    }

    let is_json_content_type = || {
        req.content_type.as_deref().is_some_and(|ct| {
            ct.trim()
                .to_ascii_lowercase()
                .starts_with("application/json")
        })
    };

    if req.path == "/sessions" {
        match req.method.as_str() {
            "POST" => {
                if !is_json_content_type() {
                    respond(
                        &mut socket,
                        415,
                        r#"{"error":"content-type must be application/json"}"#,
                    )
                    .await;
                    return;
                }
                create_session(&mut socket, &sessions, &req.body, &configs, &base).await;
            }
            "GET" => list_sessions(&mut socket, &sessions).await,
            _ => respond(&mut socket, 404, r#"{"error":"not found"}"#).await,
        }
        return;
    }
    if let Some(rest) = req.path.strip_prefix("/sessions/") {
        let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
        let session = sessions.lock().await.get(id).cloned();
        let Some(session) = session else {
            respond(&mut socket, 404, r#"{"error":"no such session"}"#).await;
            return;
        };
        match (req.method.as_str(), action) {
            ("POST", "prompt") => {
                if !is_json_content_type() {
                    respond(
                        &mut socket,
                        415,
                        r#"{"error":"content-type must be application/json"}"#,
                    )
                    .await;
                    return;
                }
                prompt(&mut socket, &session, &req.body).await;
            }
            ("GET", "events") => stream_events(socket, &session, req.last_event_id).await,
            ("GET", "") => {
                let (busy, attached) = session.snapshot();
                let body = json!({
                    "id": id,
                    "busy": busy,
                    "attached": attached,
                    "title": *session.title.lock().await,
                })
                .to_string();
                respond(&mut socket, 200, &body).await;
            }
            _ => respond(&mut socket, 404, r#"{"error":"not found"}"#).await,
        }
        return;
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/health") => respond(&mut socket, 200, r#"{"ok":true}"#).await,
        _ => respond(&mut socket, 404, r#"{"error":"not found"}"#).await,
    }
}

/// `POST /sessions`: spawn an engine, start the forwarder, respond id.
async fn create_session(
    socket: &mut tokio::net::TcpStream,
    sessions: &Arc<Mutex<std::collections::HashMap<String, Arc<Session>>>>,
    body: &[u8],
    configs: &[std::path::PathBuf],
    base: &Option<ka_engine::config::Config>,
) {
    // optional body: {"resume": "latest" | "<strand id/prefix>"}
    // (absent = fresh session, today's behavior)
    let resume = std::str::from_utf8(body)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
        .and_then(|v| {
            v.get("resume").map(|r| match r.as_str() {
                Some(s) => Ok(s.to_string()),
                None => Err("resume must be a string".to_string()),
            })
        })
        .transpose();
    let resume = match resume {
        Ok(r) => r,
        Err(e) => {
            respond(socket, 400, &json!({"error": e}).to_string()).await;
            return;
        }
    };
    let choice = match resume.as_deref() {
        None => StrandChoice::New,
        Some("latest") => StrandChoice::Latest,
        Some(id) => match resolve_strand(id) {
            Ok(choice) => choice,
            Err(e) => {
                respond(socket, 400, &json!({"error": e}).to_string()).await;
                return;
            }
        },
    };
    let cfg = match base {
        // forced base (tests): hermetic — the machine's own
        // ~/.config/ka (model, keys, mode) must never reach a test
        // session, or "canned engine" turns become real network calls
        Some(base) => base.clone(),
        None => {
            match crate::load_surface_config(configs, &std::env::current_dir().unwrap_or_default())
            {
                Ok(cfg) => cfg,
                Err(e) => {
                    respond(
                        socket,
                        500,
                        &json!({"error": format!("config: {e}")}).to_string(),
                    )
                    .await;
                    return;
                }
            }
        }
    };
    if sessions.lock().await.len() >= MAX_SESSIONS {
        respond(socket, 429, r#"{"error":"too many sessions"}"#).await;
        return;
    }
    let ka_engine::EngineHandle {
        commands, events, ..
    } = spawn_full(cfg, ka_dialect::Catalog::embedded(), choice);
    let (broadcast, _) = tokio::sync::broadcast::channel::<(u64, Event)>(256);
    let mut sessions = sessions.lock().await;
    if sessions.len() >= MAX_SESSIONS {
        drop(sessions);
        respond(socket, 429, r#"{"error":"too many sessions"}"#).await;
        return;
    }
    let order = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    let id = new_session_id();
    let session = Arc::new(Session {
        created_order: order,
        commands,
        broadcast: broadcast.clone(),
        ring: Mutex::new(Vec::new()),
        seq: AtomicU64::new(0),
        attached: AtomicUsize::new(0),
        busy: AtomicBool::new(false),
        title: Mutex::new(String::new()),
    });
    sessions.insert(id.clone(), session.clone());
    drop(sessions);
    // forwarder: engine mpsc → (ring buffer + broadcast + busy/title
    // tracking). Runs until the engine's event stream closes.
    tokio::spawn(async move {
        let mut events = events;
        while let Some(evt) = events.recv().await {
            match &evt {
                Event::TurnStarted { .. } => session.busy.store(true, Ordering::Relaxed),
                Event::Idle => {
                    session.busy.store(false, Ordering::Relaxed);
                }
                Event::Title { title } => {
                    *session.title.lock().await = title.clone();
                }
                _ => {}
            }
            let seq = session.seq.fetch_add(1, Ordering::Relaxed) + 1;
            let mut ring = session.ring.lock().await;
            // the bootstrap Replay entry is pinned — the only
            // full-history event, it never counts against RING_CAP
            // and is never drained
            let pinned = usize::from(matches!(ring.first(), Some((_, Event::Replay { .. }))));
            if ring.len() - pinned >= RING_CAP {
                // amortized trim: drop the oldest half of the live entries
                let keep = RING_CAP / 2;
                ring.drain(pinned..pinned + keep);
            }
            ring.push((seq, evt.clone()));
            drop(ring);
            // no subscribers is fine — the ring keeps the history
            let _ = broadcast.send((seq, evt));
        }
        // engine death must not wedge /sessions busy forever — the
        // stream never delivered the trailing Idle
        session.close_fixup();
    });
    let body = json!({"id": id}).to_string();
    respond(socket, 200, &body).await;
}

/// `GET /sessions`: every live session with presence + title.
async fn list_sessions(
    socket: &mut tokio::net::TcpStream,
    sessions: &Arc<Mutex<std::collections::HashMap<String, Arc<Session>>>>,
) {
    let mut rows: Vec<(u64, serde_json::Value)> = Vec::new();
    {
        let sessions = sessions.lock().await;
        for (id, s) in sessions.iter() {
            let (busy, attached) = s.snapshot();
            rows.push((
                s.created_order,
                json!({
                    "id": id,
                    "busy": busy,
                    "attached": attached,
                    "title": *s.title.lock().await,
                }),
            ));
        }
    }
    rows.sort_by_key(|(order, _)| *order);
    let values: Vec<serde_json::Value> = rows.into_iter().map(|(_, v)| v).collect();
    let body = serde_json::to_string(&values).unwrap_or_else(|_| "[]".to_string());
    respond(socket, 200, &body).await;
}

/// `POST /sessions/{id}/prompt` — the single writer path.
async fn prompt(socket: &mut tokio::net::TcpStream, session: &Session, body: &[u8]) {
    let Ok(text) = std::str::from_utf8(body) else {
        respond(socket, 400, r#"{"error":"bad body"}"#).await;
        return;
    };
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        respond(socket, 400, r#"{"error":"bad JSON"}"#).await;
        return;
    };
    let prompt = v["text"].as_str().unwrap_or_default().to_string();
    let schema = v.get("schema").cloned();
    let sent = session
        .commands
        .send(Command::Prompt {
            allowed_tools: None,
            model: None,
            text: prompt,
            schema,
            images: Vec::new(),
            skills: Vec::new(),
        })
        .await;
    if sent.is_err() {
        // the engine task is gone (crashed or shut down); a 200 here
        // would silently drop the turn
        respond(socket, 503, r#"{"error":"engine closed"}"#).await;
        return;
    }
    respond(socket, 200, r#"{"ok":true}"#).await;
}

/// Resolve a strand reference (id, prefix, or memorable name) against
/// the server's cwd, exactly like `ka --session`. `Err` = 400
/// material.
fn resolve_strand(id: &str) -> Result<StrandChoice, String> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let matched =
        match ka_strand::resolve_id(&cwd, id).map_err(|e| format!("session lookup: {e}"))? {
            ka_strand::IdMatch::None => {
                ka_strand::resolve_name(&cwd, id).map_err(|e| format!("session lookup: {e}"))?
            }
            other => other,
        };
    match matched {
        ka_strand::IdMatch::Unique(summary) => Ok(StrandChoice::Path(summary.path)),
        ka_strand::IdMatch::None => Err(format!("no session matches '{id}'")),
        ka_strand::IdMatch::Ambiguous(candidates) => {
            let ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
            Err(format!("ambiguous session '{id}': {}", ids.join(", ")))
        }
    }
}

async fn respond(socket: &mut tokio::net::TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(resp.as_bytes()).await.ok();
    socket.flush().await.ok();
}

/// Max time one SSE socket write may take: beyond this the subscriber
/// is considered stalled and dropped (it can reconnect with
/// Last-Event-ID).
const SSE_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// One bounded SSE write (write + flush). `false` = error or 15 s
/// stall: caller drops the subscriber.
async fn sse_write(socket: &mut tokio::net::tcp::OwnedWriteHalf, bytes: &[u8]) -> bool {
    matches!(
        tokio::time::timeout(SSE_WRITE_TIMEOUT, async {
            socket.write_all(bytes).await.is_ok() && socket.flush().await.is_ok()
        })
        .await,
        Ok(true)
    )
}

/// Stream session events as SSE. Every subscriber: ring-buffer backlog
/// (skipping at/below `Last-Event-ID`), then the live broadcast; a
/// presence guard bumps the attached count and broadcasts presence on
/// arrive/leave. A comment ping keeps idle proxies from dropping the
/// stream mid-turn.
async fn stream_events(
    mut socket: tokio::net::TcpStream,
    session: &Arc<Session>,
    last_event_id: Option<u64>,
) {
    use tokio::io::AsyncReadExt;
    if session.attached.load(Ordering::Relaxed) >= MAX_SUBSCRIBERS_PER_SESSION {
        respond(&mut socket, 429, r#"{"error":"too many subscribers"}"#).await;
        return;
    }
    let (mut read_half, mut socket) = socket.into_split();
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    if !sse_write(&mut socket, head.as_bytes()).await {
        return;
    }

    let mut live = session.broadcast.subscribe();
    // backlog first, so ordering is seq-stable per client. Snapshot the
    // ring and drop the lock before touching the socket — a slow client
    // must never stall the forwarder.
    let snapshot = session.ring.lock().await.clone();
    // every event this subscriber has already observed (backlog plus
    // anything the client brings via Last-Event-ID): live entries with
    // these seqs — queued on the broadcast between `subscribe()` and
    // the snapshot — are skipped below so nothing is delivered twice
    let mut replayed: std::collections::HashSet<u64> = snapshot.iter().map(|(s, _)| *s).collect();
    // `Last-Event-ID` handling is by COMPARISON, never enumeration: a
    // client-supplied huge id must not materialize 0..=id (the set
    // stays bounded by ring size). Live entries at/below the id were
    // already seen and are skipped.
    // the last seq this subscriber actually observed — used as the id
    // of the lag hint so a Last-Event-ID reconnect replays the gap
    let mut last_seen = last_event_id;
    for (seq, evt) in &snapshot {
        if Some(*seq) <= last_event_id {
            continue;
        }
        let line = sse_line(Some(*seq), &serde_json::to_string(evt).unwrap_or_default());
        if !sse_write(&mut socket, line.as_bytes()).await {
            return;
        }
        last_seen = Some(*seq);
    }

    // presence: this subscriber counts and everyone hears about it
    session.attached.fetch_add(1, Ordering::Relaxed);
    session.broadcast_presence();
    struct PresenceGuard<'a>(&'a Session);
    impl Drop for PresenceGuard<'_> {
        fn drop(&mut self) {
            self.0.attached.fetch_sub(1, Ordering::Relaxed);
            self.0.broadcast_presence();
        }
    }
    let _guard = PresenceGuard(session);

    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(15));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await; // the first tick fires immediately — consume it

    // a discard buffer for the client→server direction: SSE clients
    // send nothing, so any read completing (EOF or error) means the
    // subscriber is gone — without this arm, presence only updates on
    // the next keepalive write failure (up to 15 s late)
    let mut discard = [0u8; 64];
    loop {
        tokio::select! {
            gone = read_half.read(&mut discard) => {
                if gone.is_err() || gone.is_ok_and(|n| n == 0) {
                    break;
                }
            }
            _ = keepalive.tick() => {
                // SSE comment frame: invisible to clients, keeps idle
                // proxies/timeouts from dropping a mid-turn stream
                if !sse_write(&mut socket, b": keepalive\r\n\r\n").await {
                    break;
                }
            }
            evt = live.recv() => {
                match evt {
                    Ok((seq, _evt)) if Some(seq) <= last_event_id || replayed.remove(&seq) => {}
                    Ok((seq, evt)) => {
                        let line = sse_line(Some(seq), &serde_json::to_string(&evt).unwrap_or_default());
                        if !sse_write(&mut socket, line.as_bytes()).await {
                            break;
                        }
                        last_seen = Some(seq);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // this subscriber fell behind the broadcast
                        // window; the ring buffer has the gap — tell the
                        // client to reconnect from the last seq it
                        // actually saw, so the gap is replayed
                        let hint = Event::Error {
                            class: ErrorClass::Network,
                            retryable: true,
                            message: "stream lagged — reconnect with Last-Event-ID".to_string(),
                        };
                        let line =
                            sse_line(last_seen, &serde_json::to_string(&hint).unwrap_or_default());
                        if !sse_write(&mut socket, line.as_bytes()).await {
                            break;
                        }
                    }
                    // unreachable in practice: every subscriber holds a
                    // Session Arc, whose sender keeps the channel open
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

fn sse_line(id: Option<u64>, data: &str) -> String {
    let id_part = id.map(|i| format!("id: {i}\r\n")).unwrap_or_default();
    format!("{id_part}data: {data}\r\n\r\n")
}

/// First index of `needle` in `hay` (byte-exact; used for the
/// `\r\n\r\n` head boundary, which must survive a non-UTF8 body).
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    // NOTE: session creation loads the standard config chain (user <
    // project < --config < env). CI has no user config, but locally a
    // broken or feature-restricting ~/.config/ka/ka.toml can change
    // these tests' outcomes. There is no safe env override to isolate
    // them: `std::env::set_var` is unsafe in edition 2024 and racy
    // across parallel tests in one process.

    use tokio::io::AsyncReadExt;

    use tokio::io::AsyncWriteExt;

    async fn http_get(
        addr: std::net::SocketAddr,
        path: &str,
        token: Option<&str>,
    ) -> (u16, String) {
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let req =
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n");
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        sock.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let status: u16 = text
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let body = text
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        (status, body)
    }

    async fn http_post(
        addr: std::net::SocketAddr,
        path: &str,
        token: Option<&str>,
        body: &str,
    ) -> (u16, String) {
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        sock.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let status: u16 = text
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let body = text
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        (status, body)
    }

    /// Open one SSE stream that reads until aborted; the shared buffer
    /// accumulates raw bytes (presence tests need subscribers that stay
    /// live, so there is no marker-based early exit).
    async fn open_stream(
        addr: std::net::SocketAddr,
        path: &str,
        token: Option<&str>,
    ) -> (
        tokio::task::JoinHandle<()>,
        std::sync::Arc<tokio::sync::Mutex<Vec<u8>>>,
    ) {
        let collected: std::sync::Arc<tokio::sync::Mutex<Vec<u8>>> =
            std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let sink = collected.clone();
        let path = path.to_string();
        let token = token.map(str::to_string);
        let task = tokio::spawn(async move {
            let Ok(mut sock) = tokio::net::TcpStream::connect(addr).await else {
                return;
            };
            let auth = token
                .as_deref()
                .map(|t| format!("Authorization: Bearer {t}\r\n"))
                .unwrap_or_default();
            let req = format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n"
            );
            if sock.write_all(req.as_bytes()).await.is_err() {
                return;
            }
            let mut buf = [0u8; 16384];
            loop {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().await.extend_from_slice(&buf[..n]),
                }
            }
        });
        (task, collected)
    }

    /// Like `open_stream`, but sends a `Last-Event-ID` header (the
    /// reconnect path).
    async fn open_stream_with_last_id(
        addr: std::net::SocketAddr,
        path: &str,
        token: Option<&str>,
        last_id: u64,
    ) -> (
        tokio::task::JoinHandle<()>,
        std::sync::Arc<tokio::sync::Mutex<Vec<u8>>>,
    ) {
        let collected: std::sync::Arc<tokio::sync::Mutex<Vec<u8>>> =
            std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let sink = collected.clone();
        let path = path.to_string();
        let token = token.map(str::to_string);
        let task = tokio::spawn(async move {
            let Ok(mut sock) = tokio::net::TcpStream::connect(addr).await else {
                return;
            };
            let auth = token
                .as_deref()
                .map(|t| format!("Authorization: Bearer {t}\r\n"))
                .unwrap_or_default();
            let req = format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Last-Event-ID: {last_id}\r\nConnection: close\r\n\r\n"
            );
            if sock.write_all(req.as_bytes()).await.is_err() {
                return;
            }
            let mut buf = [0u8; 16384];
            loop {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().await.extend_from_slice(&buf[..n]),
                }
            }
        });
        (task, collected)
    }

    /// Poll a stream buffer until it contains `needle` (or `secs`
    /// elapse); returns the text so far either way.
    async fn wait_contains(
        buf: &std::sync::Arc<tokio::sync::Mutex<Vec<u8>>>,
        needle: &str,
        secs: u64,
    ) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            let text = String::from_utf8_lossy(&buf.lock().await.clone()).into_owned();
            if text.contains(needle) || std::time::Instant::now() > deadline {
                return text;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    #[tokio::test]
    async fn serve_routes_health_sessions_prompt_and_auth() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(super::serve(
            listener,
            Some("sekrit".into()),
            Vec::new(),
            Some(ka_engine::config::Config::default()),
        ));

        // health (token required)
        let (status, _) = http_get(addr, "/health", None).await;
        assert_eq!(status, 401, "missing token must 401");
        let (status, body) = http_get(addr, "/health", Some("sekrit")).await;
        assert_eq!(status, 200);
        assert_eq!(body, r#"{"ok":true}"#);

        // create a session
        let (status, body) = http_post(addr, "/sessions", Some("sekrit"), "{}").await;
        assert_eq!(status, 200);
        assert!(body.contains("\"id\""), "{body}");

        // unknown session 404s
        let (status, _) = http_post(
            addr,
            "/sessions/nope/prompt",
            Some("sekrit"),
            r#"{"text":"hi"}"#,
        )
        .await;
        assert_eq!(status, 404);

        // prompt a real session (canned engine, no model)
        let id: String = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .into();
        let (status, body) = http_post(
            addr,
            &format!("/sessions/{id}/prompt"),
            Some("sekrit"),
            r#"{"text":"hello"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");

        // events stream (canned reply lands quickly)
        let (task, collected) =
            open_stream(addr, &format!("/sessions/{id}/events"), Some("sekrit")).await;
        let text = wait_contains(&collected, "turn_finished", 5).await;
        task.abort();
        assert!(text.contains("data:"), "{text}");
        assert!(text.contains("turn_finished"), "{text}");
    }

    /// Roadmap 9.6: any number of concurrent observers, presence counts
    /// them, the session list carries busy/attached/title, and the
    /// write path stays the one POST route.
    #[tokio::test]
    async fn concurrent_observers_presence_and_listings() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(super::serve(
            listener,
            None,
            Vec::new(),
            Some(ka_engine::config::Config::default()),
        ));

        let (status, body) = http_post(addr, "/sessions", None, "{}").await;
        assert_eq!(status, 200, "{body}");
        let id: String = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .into();

        // one live observer
        let (first, first_buf) = open_stream(addr, &format!("/sessions/{id}/events"), None).await;
        // let the presence event from the first subscriber land
        let _ = wait_contains(&first_buf, "presence", 5).await;
        let (status, body) = http_get(addr, &format!("/sessions/{id}"), None).await;
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["attached"].as_u64().unwrap(), 1, "{body}");

        // a second concurrent observer streams the same history — the
        // old single-consumer error ("event stream already in use") is
        // gone
        let (second, second_buf) = open_stream(addr, &format!("/sessions/{id}/events"), None).await;
        let second_text = wait_contains(&second_buf, "\"type\":\"replay\"", 5).await;
        assert!(
            second_text.contains("replay"),
            "late observer gets history: {second_text}"
        );
        let (_status, body) = http_get(addr, &format!("/sessions/{id}"), None).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["attached"].as_u64().unwrap(), 2, "{body}");

        // the first observer also saw replay + presence events
        let first_text = wait_contains(&first_buf, "\"type\":\"replay\"", 5).await;
        assert!(
            first_text.contains("presence"),
            "presence events flow: {first_text}"
        );

        // the session list carries presence + title
        let (status, body) = http_get(addr, "/sessions", None).await;
        assert_eq!(status, 200);
        assert!(body.contains("\"attached\":2"), "{body}");
        assert!(body.contains("\"busy\""), "{body}");

        // dropping a subscriber updates presence
        second.abort();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let (_status, body) = http_get(addr, &format!("/sessions/{id}"), None).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["attached"].as_u64().unwrap(), 1, "{body}");
        first.abort();
    }

    /// Extract the ordered `(id, data)` pairs from raw SSE text.
    fn sse_pairs(text: &str) -> Vec<(u64, String)> {
        let mut pairs = Vec::new();
        let mut id: Option<u64> = None;
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("id: ") {
                id = v.trim().parse().ok();
            } else if let Some(v) = line.strip_prefix("data: ") {
                if let Some(i) = id.take() {
                    pairs.push((i, v.to_string()));
                }
            }
        }
        pairs
    }

    /// Last-Event-ID reconnect: replay resumes exactly at id+1 — no
    /// missing, no duplicated, no mislabeled events.
    #[tokio::test]
    async fn last_event_id_reconnect_replays_from_next_seq() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(super::serve(
            listener,
            None,
            Vec::new(),
            Some(ka_engine::config::Config::default()),
        ));

        let (status, body) = http_post(addr, "/sessions", None, "{}").await;
        assert_eq!(status, 200, "{body}");
        let id: String = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .into();

        // observer 1: sees turn 1 live
        let (first, first_buf) = open_stream(addr, &format!("/sessions/{id}/events"), None).await;
        let (status, body) = http_post(
            addr,
            &format!("/sessions/{id}/prompt"),
            None,
            r#"{"text":"hello"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let text = wait_contains(&first_buf, "turn_finished", 5).await;
        let last_id = sse_pairs(&text).last().map(|(i, _)| *i).unwrap();

        // drive a second turn while observer 1 stays live (its events
        // land in the ring + observer 1's stream)
        let (status, body) = http_post(
            addr,
            &format!("/sessions/{id}/prompt"),
            None,
            r#"{"text":"again"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let text = loop {
            let t = String::from_utf8_lossy(&first_buf.lock().await.clone()).into_owned();
            if t.matches("turn_finished").count() >= 2 || std::time::Instant::now() > deadline {
                break t;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        let observed: Vec<(u64, String)> = sse_pairs(&text)
            .into_iter()
            .filter(|(i, _)| *i > last_id)
            .collect();
        assert!(
            observed.iter().any(|(_, d)| d.contains("turn_finished")),
            "observer 1 saw turn 2: {text}"
        );

        // reconnect with Last-Event-ID = the id observer 1 had after
        // turn 1; replay must start exactly at last_id+1
        let (second, second_buf) =
            open_stream_with_last_id(addr, &format!("/sessions/{id}/events"), None, last_id).await;
        let second_text = wait_contains(&second_buf, "turn_finished", 5).await;
        second.abort();
        first.abort();

        let replayed: Vec<(u64, String)> = sse_pairs(&second_text)
            .into_iter()
            .filter(|(_, d)| !d.contains("\"type\":\"presence\""))
            .collect();
        assert!(
            replayed.iter().any(|(_, d)| d.contains("turn_finished")),
            "reconnect saw turn 2: {second_text}"
        );
        assert_eq!(
            replayed.first().map(|(i, _)| *i),
            Some(last_id + 1),
            "replay starts at id+1: {second_text}"
        );
        // the turn-2 events replayed to the reconnecting client are
        // exactly the ones observer 1 saw live — no missing, no
        // duplicates, no mislabeled ids
        assert_eq!(
            replayed, observed,
            "replayed events match the live observation"
        );
    }

    /// A huge `Last-Event-ID` is handled by comparison, not
    /// enumeration: the server must stay responsive (the old
    /// `replayed.extend(0..=last)` materialized u64::MAX+1 entries and
    /// wedged/OOM'd the task).
    #[tokio::test]
    async fn huge_last_event_id_does_not_wedge_the_stream() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(super::serve(
            listener,
            None,
            Vec::new(),
            Some(ka_engine::config::Config::default()),
        ));

        let (status, body) = http_post(addr, "/sessions", None, "{}").await;
        assert_eq!(status, 200, "{body}");
        let id: String = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .into();

        let (task, collected) =
            open_stream_with_last_id(addr, &format!("/sessions/{id}/events"), None, u64::MAX).await;
        // the subscriber must come up promptly (the keepalive comment
        // frame arrives within 15 s) instead of hanging in a giant
        // replay loop; a client claiming id u64::MAX deliberately
        // mutes itself — every live seq is <= its claimed id — so no
        // data frames are expected
        let text = wait_contains(&collected, "keepalive", 20).await;
        task.abort();
        assert!(text.contains(": keepalive"), "stream is live: {text}");
        assert!(
            sse_pairs(&text).is_empty(),
            "no fabricated replay ids: {text}"
        );
        // and the server is still serving
        let (status, _) = http_get(addr, "/health", None).await;
        assert_eq!(status, 200);
    }

    /// An invalid-UTF-8 byte in the head must not panic or corrupt the
    /// body: the head boundary is byte-exact, so the request is
    /// answered cleanly (404 for a mangled path; the untouched body
    /// still routes when the junk byte sits in an unused header).
    #[tokio::test]
    async fn invalid_utf8_in_head_is_handled_cleanly() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(super::serve(
            listener,
            None,
            Vec::new(),
            Some(ka_engine::config::Config::default()),
        ));

        // mangled path → clean 404 error, not a panic/reset
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = b"GET /hea\xFFlth HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
        sock.write_all(req).await.unwrap();
        let mut buf = Vec::new();
        sock.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        assert!(
            text.starts_with("HTTP/1.1 404"),
            "clean 404 for a mangled path: {text}"
        );
        assert!(
            text.contains(r#"{"error":"not found"}"#),
            "error body intact: {text}"
        );

        // junk byte in an unused header, valid Content-Length body:
        // the body must survive byte-exact (session is created)
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = b"POST /sessions HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nX-Junk: \xC3\x28\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
        sock.write_all(req).await.unwrap();
        let mut buf = Vec::new();
        sock.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        assert!(
            text.starts_with("HTTP/1.1 200"),
            "body not corrupted by bad header: {text}"
        );
    }

    /// Without a token the server may only bind loopback.
    #[test]
    fn loopback_detection() {
        assert!(super::binds_loopback("127.0.0.1:8417").unwrap());
        assert!(super::binds_loopback("localhost:8417").unwrap());
        assert!(super::binds_loopback("[::1]:8417").unwrap());
        assert!(!super::binds_loopback("0.0.0.0:8417").unwrap());
        assert!(!super::binds_loopback("192.168.1.10:8417").unwrap());
    }

    #[tokio::test]
    async fn non_loopback_bind_without_token_is_refused() {
        let err = super::run("0.0.0.0:0", None, &[]).await.unwrap_err();
        assert!(
            err.contains("--token"),
            "error must tell the operator to pass --token: {err}"
        );
    }

    #[tokio::test]
    async fn engine_stream_end_clears_busy_and_broadcasts_presence() {
        let (commands, _rx) = tokio::sync::mpsc::channel(1);
        let (broadcast, mut brx) = tokio::sync::broadcast::channel(4);
        let session = super::Session {
            created_order: 1,
            commands,
            broadcast,
            ring: tokio::sync::Mutex::new(Vec::new()),
            seq: std::sync::atomic::AtomicU64::new(0),
            attached: std::sync::atomic::AtomicUsize::new(1),
            busy: std::sync::atomic::AtomicBool::new(true),
            title: tokio::sync::Mutex::new(String::new()),
        };
        session.close_fixup();
        let (busy, _) = session.snapshot();
        assert!(!busy, "engine death must not wedge /sessions busy");
        match brx.recv().await {
            Ok((_, ka_protocol::Event::Presence { busy, .. })) => assert!(!busy),
            other => panic!("expected a presence broadcast, got {other:?}"),
        }
        // idempotent: a settled session broadcasts nothing more
        session.close_fixup();
        assert!(brx.try_recv().is_err());
    }

    /// Strict TOML rides the surface: a broken --config fails session
    /// creation with the parse error instead of silently defaulting.
    #[tokio::test]
    async fn invalid_config_fails_session_creation() {
        let dir = std::env::temp_dir().join(format!("ka-serve-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.toml");
        std::fs::write(&bad, "[tui]\nunknown_key = 1\n").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(super::serve(listener, None, vec![bad], None));
        let (status, body) = http_post(addr, "/sessions", None, "{}").await;
        assert_eq!(status, 500, "{body}");
        assert!(body.contains("config"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The [features] baseline reaches serve sessions: the engine's
    /// bootstrap Inventory reports the disabled spec.
    #[tokio::test]
    async fn config_features_ride_the_inventory() {
        let dir = std::env::temp_dir().join(format!("ka-serve-feat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("ka.toml");
        std::fs::write(&good, "[features]\ndisable = [\"mcp\"]\n").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(super::serve(listener, None, vec![good], None));
        let (status, body) = http_post(addr, "/sessions", None, "{}").await;
        assert_eq!(status, 200, "{body}");
        let id: String = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .into();
        let (task, collected) = open_stream(addr, &format!("/sessions/{id}/events"), None).await;
        let text = wait_contains(&collected, "disabled", 5).await;
        task.abort();
        assert!(
            text.contains("\"disabled\":[\"mcp\"]"),
            "features baseline must ride the inventory: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
