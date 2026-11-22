//! EXT-06: `ka mcp-serve` — ka as a Model Context Protocol server.
//!
//! A read-only MCP stdio server over ka's read-tier hands: JSON-RPC
//! 2.0, one message per line (the same NDJSON framing ka's own MCP
//! client in ka-engine speaks). Lifecycle mirrors any MCP server:
//! `initialize` → `notifications/initialized` → `tools/list` /
//! `tools/call`; unknown requests get JSON-RPC -32601, tool failures
//! come back as `isError` results, and stdin EOF ends the server.
//!
//! Tool surface, v1: `read` (with `archive_entry`), `glob`, `grep`,
//! `sessions_search` (full-text when the `index` feature is built,
//! title/name/recap/id substring otherwise) and `session_info`. Every
//! call executes with a FRESH [`HandContext`] — cwd = the server's
//! launch directory, sandbox `Policy::Off`, inert snapshot journal,
//! fresh ledger and spill store — so the server keeps no state between
//! calls, and nothing the hands do can mutate the workspace: the read
//! hands only mint ledger stamps (memory, dropped with the context)
//! and may park oversized extracts in ka's spill directory.
//!
//! Deliberately NO `bash` tool — with or without `--read-only`.
//! Exposing arbitrary command execution over an MCP pipe hands the
//! process's full user privileges to every MCP client that connects
//! (another agent's harness, an IDE, a script); the interactive
//! ask-gate that guards bash inside ka has no surface over a pipe, and
//! the inspect-only hands already cover what an external client needs.
//! `--read-only` is the explicit contract marker: today it selects the
//! same set (all five tools are read-only); a future write tier would
//! ADD hands to the default surface behind that flag, never remove
//! them. `--tools` narrows the list further: unknown names simply
//! match nothing, and naming `bash` is refused outright, never a
//! grant.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use ka_engine::hands::{
    Clearance, Hand, HandContext, HandDef, Ledger, Spill, ToolOutput, glob::GlobHand,
    grep::GrepHand, read::ReadHand,
};

/// Protocol version this server negotiates (matches the client ka
/// ships); an `initialize` request's version is echoed back when one
/// is given, so the pair always agrees.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// `serverInfo.name`.
const SERVER_NAME: &str = "ka";

/// Options for one server run (from the `McpServe` CLI variant).
#[derive(Debug, Clone, Default)]
pub(crate) struct ServeOpts {
    /// `--tools`: only expose these tools (subset of the v1 surface;
    /// unknown names match nothing, `bash` is refused).
    pub tools: Vec<String>,
    /// `--read-only`: restrict to read-only tools (the v1 surface is
    /// read-only either way — see the module docs).
    pub read_only: bool,
}

/// One tool in the server's registry: the hand plus its static
/// definition (built once per server, not per listing).
struct ToolEntry {
    def: HandDef,
    hand: Arc<dyn Hand>,
}

fn entry(hand: impl Hand + 'static) -> ToolEntry {
    ToolEntry {
        def: hand.def(),
        hand: Arc::new(hand),
    }
}

/// The v1 registry: the five read hands. No bash, in either flag
/// mode — see the module docs.
fn registry(opts: &ServeOpts) -> Vec<ToolEntry> {
    let mut tools = vec![
        entry(ReadHand),
        entry(GlobHand),
        entry(GrepHand),
        entry(SessionsSearchHand),
        entry(SessionInfoHand),
    ];
    // The --read-only contract: strip everything that is not read-only.
    // Every v1 hand passes, so the surface is identical today; a future
    // write tier added to the default registry would be excluded here,
    // which is exactly what the flag promises.
    if opts.read_only {
        tools.retain(|t| t.def.read_only);
    }
    if !opts.tools.is_empty() {
        let allow = &opts.tools;
        tools.retain(|t| allow.contains(&t.def.name));
    }
    tools
}

/// `ka mcp-serve`: run the server on stdin/stdout until EOF.
pub(crate) async fn run(opts: ServeOpts) -> Result<ExitCode, String> {
    if opts.tools.iter().any(|n| n == "bash") {
        return Err("ka mcp-serve: bash is never exposed by the MCP server".to_string());
    }
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    serve(tokio::io::stdin(), tokio::io::stdout(), opts, cwd).await?;
    Ok(ExitCode::SUCCESS)
}

/// The server loop: one JSON-RPC message per input line, one response
/// line per request. Requests are served sequentially (a long hand
/// delays later requests — correct, just not concurrent). Returns when
/// stdin hits EOF.
pub(crate) async fn serve<R, W>(
    input: R,
    output: W,
    opts: ServeOpts,
    cwd: PathBuf,
) -> Result<(), String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let tools = registry(&opts);
    let mut output = output;
    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| format!("read stdin: {e}"))?
    {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                respond(
                    &mut output,
                    &json!({"jsonrpc": "2.0", "id": null,
                            "error": {"code": -32700, "message": format!("parse error: {e}")}}),
                )
                .await?;
                continue;
            }
        };
        if msg.is_array() {
            // JSON-RPC batching is not part of the MCP transports ka
            // speaks (removed from the spec in 2025-06-18).
            respond(
                &mut output,
                &json!({"jsonrpc": "2.0", "id": null,
                        "error": {"code": -32600, "message": "batch requests are not supported"}}),
            )
            .await?;
            continue;
        }
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            // A response to a server-initiated request: none are ever
            // sent, so nothing to route it to.
            continue;
        };
        let id = msg.get("id").cloned().filter(|v| !v.is_null());
        let Some(id) = id else {
            // Notification: accepted silently (JSON-RPC forbids
            // responding to them, so unknown ones are dropped too).
            continue;
        };
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
        let reply = match handle_request(method, &params, &tools, &cwd).await {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => json!({"jsonrpc": "2.0", "id": id,
                                          "error": {"code": code, "message": message}}),
        };
        respond(&mut output, &reply).await?;
    }
    Ok(())
}

/// Write one response line (NDJSON framing).
async fn respond<W: AsyncWrite + Unpin>(out: &mut W, payload: &Value) -> Result<(), String> {
    let line = serde_json::to_string(payload).map_err(|e| format!("serialize: {e}"))?;
    out.write_all(line.as_bytes())
        .await
        .map_err(|e| format!("write stdout: {e}"))?;
    out.write_all(b"\n")
        .await
        .map_err(|e| format!("write stdout: {e}"))?;
    out.flush().await.map_err(|e| format!("write stdout: {e}"))
}

/// Dispatch one JSON-RPC request: `Ok` is a result payload, `Err` a
/// `(code, message)` JSON-RPC error.
async fn handle_request(
    method: &str,
    params: &Value,
    tools: &[ToolEntry],
    cwd: &Path,
) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(initialize_result(params)),
        "tools/list" => Ok(json!({
            "tools": tools.iter().map(tool_descriptor).collect::<Vec<_>>()
        })),
        "tools/call" => call_tool(params, tools, cwd).await,
        // Spec'd liveness probe: an empty result, no capabilities.
        "ping" => Ok(json!({})),
        // Everything else — including resources/* and prompts/*, which
        // this server never advertises — is method-not-found.
        other => Err((-32601, format!("method not found: {other}"))),
    }
}

/// The `initialize` result: ka's identity and the tools capability
/// only (no resources, no prompts, no subscriptions).
fn initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": requested,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("KA_VERSION"),
        },
    })
}

/// The MCP tool descriptor for one entry: schema straight off the
/// HandDef, plus the read-only annotations ka's own client filters on.
fn tool_descriptor(t: &ToolEntry) -> Value {
    json!({
        "name": t.def.name,
        "description": t.def.description,
        "inputSchema": t.def.parameters,
        "annotations": {"readOnlyHint": true, "destructiveHint": false},
    })
}

/// Execute one `tools/call`: fresh HandContext per call, text content
/// back, hand failures as `isError` results (model-visible, not
/// protocol errors).
async fn call_tool(
    params: &Value,
    tools: &[ToolEntry],
    cwd: &Path,
) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| (-32602, "tools/call: missing tool name".to_string()))?;
    let Some(entry) = tools.iter().find(|t| t.def.name == name) else {
        return Ok(json!({
            "content": [{"type": "text",
                         "text": format!("unknown tool: {name} (ka mcp-serve exposes only the read hands)")}],
            "isError": true,
        }));
    };
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let ctx = fresh_context(cwd);
    let out = entry.hand.execute(&args, &ctx).await;
    let mut content = vec![json!({"type": "text", "text": out.content})];
    for image in &out.images {
        content.push(json!({"type": "image", "data": image.data, "mimeType": image.media_type}));
    }
    Ok(json!({"content": content, "isError": out.is_error}))
}

/// A fresh, stateless per-call context: cwd pinned to the server's
/// launch directory, sandbox off, snapshots inert, ledger and spill
/// store brand new (dropped when the call returns).
fn fresh_context(cwd: &Path) -> HandContext {
    HandContext {
        cwd: cwd.to_path_buf(),
        ledger: Arc::new(parking_lot::Mutex::new(Ledger::default())),
        spill: Arc::new(Spill::new()),
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

/// `sessions_search`: past sessions for the server's cwd. With the
/// `index` feature the FTS5 index answers (falling back to the cheap
/// listing on any index error — a stale or missing db must not turn
/// into a failed tool); without it, title/name/recap/id substring
/// match over the strand listings.
struct SessionsSearchHand;

impl Hand for SessionsSearchHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "sessions_search".to_string(),
            description: "Search past ka sessions for this directory: full-text over session \
                content when the index feature is built, otherwise a title/name/recap/id \
                substring match. Returns id, timestamp and matched text per hit."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search text" },
                    "limit": { "type": "integer", "description": "Max results (default 20)" }
                },
                "required": ["query"]
            }),
            clearance: Clearance::Read,
            read_only: true,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let Some(query) = args.get("query").and_then(Value::as_str) else {
                return ToolOutput::err("sessions_search: missing required 'query'");
            };
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map(|l| (l as usize).max(1))
                .unwrap_or(20);
            // The listing reads are small (header-only strand scans);
            // inline is fine — no MB-scale work like the file hands.
            #[cfg(feature = "index")]
            if let Some(text) = search_indexed(query, limit) {
                return ToolOutput::ok(text);
            }
            ToolOutput::ok(search_listings(&ctx.cwd, query, limit))
        })
    }
}

/// FTS5 answer, `None` when the index feature is off or the db cannot
/// answer (missing, stale schema, unsupported query syntax).
#[cfg(feature = "index")]
fn search_indexed(query: &str, limit: usize) -> Option<String> {
    let db = ka_index::open_db(&ka_index::default_db_path()).ok()?;
    let hits = ka_index::search(&db, query, None, limit).ok()?;
    if hits.is_empty() {
        // an empty answer from a stale/unbuilt index must not shadow the
        // listings fallback — return None so the caller scans those
        return None;
    }
    Some(
        hits.iter()
            .map(|r| format!("{}\t{}\t[{}] {}", r.session, r.ts, r.role, r.snippet))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Cheap fallback: scan live + archived listings for a case-insensitive
/// substring hit on title, memorable name, recap or id.
fn search_listings(cwd: &Path, query: &str, limit: usize) -> String {
    let needle = query.to_lowercase();
    let mut rows = Vec::new();
    let live = ka_strand::list(cwd).unwrap_or_default();
    let strands = live
        .into_iter()
        .chain(ka_strand::list_archived(cwd))
        .filter(|s| {
            [
                s.title.as_str(),
                s.name.as_deref().unwrap_or(""),
                s.recap.as_deref().unwrap_or(""),
                s.id.as_str(),
            ]
            .iter()
            .any(|field| field.to_lowercase().contains(&needle))
        });
    for s in strands {
        let label = match &s.name {
            Some(n) => format!("{n} · {}", s.title),
            None => s.title.clone(),
        };
        rows.push(format!("{}\t{}\t{}", s.id, s.ts, label));
        if rows.len() >= limit {
            break;
        }
    }
    if rows.is_empty() {
        format!("no sessions match {query:?}")
    } else {
        rows.join("\n")
    }
}

/// `session_info`: one session's header summary — newest for the cwd
/// by default, or the one matching an id prefix / memorable name.
struct SessionInfoHand;

impl Hand for SessionInfoHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "session_info".to_string(),
            description: "Summarize one ka session for this directory: id, creation time, \
                title, memorable name, message count, cost, tokens, pin state, recap and \
                strand file path. Defaults to the newest session; pass a session id prefix \
                or memorable name to pick another."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session id prefix or \
                        memorable name (default: newest for this cwd)" }
                },
                "required": []
            }),
            clearance: Clearance::Read,
            read_only: true,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let cwd = &ctx.cwd;
            let summary = match args.get("session").and_then(Value::as_str) {
                None => match ka_strand::latest(cwd) {
                    Ok(Some(s)) => s,
                    Ok(None) => {
                        return ToolOutput::ok(format!("no sessions yet for {}", cwd.display()));
                    }
                    Err(e) => return ToolOutput::err(format!("session_info: {e}")),
                },
                Some(reference) => {
                    let path = match crate::resolve_session(cwd, reference) {
                        Ok(ka_engine::StrandChoice::Path(p)) => p,
                        Ok(_) => {
                            return ToolOutput::err(
                                "session_info: session did not resolve to a strand file",
                            );
                        }
                        Err(e) => return ToolOutput::err(e),
                    };
                    match ka_strand::list(cwd)
                        .unwrap_or_default()
                        .into_iter()
                        .chain(ka_strand::list_archived(cwd))
                        .find(|s| s.path == path)
                    {
                        Some(s) => s,
                        None => {
                            return ToolOutput::err(format!(
                                "session_info: {} resolved but its summary is unreadable",
                                path.display()
                            ));
                        }
                    }
                }
            };
            ToolOutput::ok(format_session_summary(&summary))
        })
    }
}

/// Human-readable multi-line summary of one strand header.
fn format_session_summary(s: &ka_strand::StrandSummary) -> String {
    let mut out = format!(
        "id       {}\ncreated  {}\ntitle    {}\nmessages {}\ncost     ${:.4}\ntokens   {}\npath     {}",
        s.id,
        s.ts,
        s.title,
        s.messages,
        s.cost,
        s.tokens,
        s.path.display()
    );
    if let Some(name) = &s.name {
        out.push_str(&format!("\nname     {name}"));
    }
    if s.pinned {
        out.push_str("\npinned   ★");
    }
    if let Some(parent) = &s.parent {
        out.push_str(&format!("\nparent   {parent} (fork)"));
    }
    if let Some(recap) = &s.recap {
        out.push_str(&format!("\nrecap    {recap}"));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    use serde_json::json;
    use tokio::io::{AsyncWriteExt, duplex};
    use tokio::time::{Duration, timeout};

    /// Generous local round-trip bound: everything here is in-memory
    /// or a temp-dir fs read; a hang means a framing bug worth failing
    /// on, fast.
    const PATIENCE: Duration = Duration::from_secs(5);

    static TEMP_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    struct TestClient {
        writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
        reader: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    }

    impl TestClient {
        async fn send(&mut self, msg: &Value) {
            let line = serde_json::to_string(msg).unwrap();
            timeout(PATIENCE, async {
                self.writer.write_all(line.as_bytes()).await.unwrap();
                self.writer.write_all(b"\n").await.unwrap();
                self.writer.flush().await.unwrap();
            })
            .await
            .unwrap();
        }

        async fn recv(&mut self) -> Value {
            let line = timeout(PATIENCE, self.reader.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            serde_json::from_str(&line).unwrap()
        }

        async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
            self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
                .await;
            self.recv().await
        }

        /// Handshake: initialize + notifications/initialized (the
        /// notification must produce NO response line — asserted by
        /// the caller's next request getting the very next line).
        async fn open(
            opts: ServeOpts,
            cwd: PathBuf,
        ) -> (Self, tokio::task::JoinHandle<Result<(), String>>) {
            let (client, server) = duplex(64 * 1024);
            let (server_read, server_write) = tokio::io::split(server);
            let handle = tokio::spawn(serve(server_read, server_write, opts, cwd));
            let (client_read, client_write) = tokio::io::split(client);
            let mut c = Self {
                writer: client_write,
                reader: BufReader::new(client_read).lines(),
            };
            let resp = c
                .request(
                    1,
                    "initialize",
                    json!({"protocolVersion": PROTOCOL_VERSION, "capabilities": {},
                           "clientInfo": {"name": "test", "version": "0"}}),
                )
                .await;
            assert_eq!(resp["result"]["serverInfo"]["name"], "ka");
            assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
            assert!(resp["result"]["capabilities"]["tools"].is_object());
            c.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .await;
            (c, handle)
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let n = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ka-mcpserve-{}-{n}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn initialized_notification_stays_silent_then_ping() {
        let cwd = temp_dir("ping");
        let (mut c, handle) = TestClient::open(ServeOpts::default(), cwd).await;
        // The next line on the wire must be THIS response — proof the
        // notification above produced none.
        let resp = c.request(2, "ping", json!({})).await;
        assert_eq!(resp["id"], 2);
        assert!(resp["result"].is_object());
        assert_eq!(resp["result"], json!({}));
        drop(c);
        assert!(handle.await.unwrap().is_ok(), "EOF ends the server");
    }

    #[tokio::test]
    async fn unknown_request_is_32601() {
        let cwd = temp_dir("unknown");
        let (mut c, _handle) = TestClient::open(ServeOpts::default(), cwd).await;
        let resp = c.request(3, "resources/list", json!({})).await;
        assert_eq!(resp["error"]["code"], -32601);
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap()
                .contains("resources/list")
        );
        // Notifications with unknown methods are dropped, not errored.
        c.send(&json!({"jsonrpc": "2.0", "method": "xyz/nope"}))
            .await;
        let resp = c.request(4, "ping", json!({})).await;
        assert_eq!(resp["id"], 4, "no error line for an unknown notification");
    }

    #[tokio::test]
    async fn parse_error_is_32700() {
        let cwd = temp_dir("parse");
        let (client, server) = duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let handle = tokio::spawn(serve(server_read, server_write, ServeOpts::default(), cwd));
        let (client_read, client_write) = tokio::io::split(client);
        let mut writer = client_write;
        let mut reader = BufReader::new(client_read).lines();
        writer.write_all(b"{not json\n").await.unwrap();
        writer.flush().await.unwrap();
        let line = timeout(PATIENCE, reader.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let resp: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(resp["error"]["code"], -32700);
        drop((writer, reader));
        assert!(handle.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn tools_list_declares_the_read_hands() {
        let cwd = temp_dir("tools-list");
        let (mut c, _handle) = TestClient::open(ServeOpts::default(), cwd).await;
        let resp = c.request(5, "tools/list", json!({})).await;
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec!["read", "glob", "grep", "sessions_search", "session_info"],
            "the v1 surface is exactly the five read hands"
        );
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
            assert_eq!(t["annotations"]["readOnlyHint"], true, "{}", t["name"]);
        }
        let by_name = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap();
        // Schemas derive from the HandDefs: read carries archive_entry,
        // grep requires a pattern, sessions_search requires a query,
        // session_info takes none.
        assert!(by_name("read")["inputSchema"]["properties"]["archive_entry"].is_object());
        assert_eq!(
            by_name("grep")["inputSchema"]["required"],
            json!(["pattern"])
        );
        assert_eq!(
            by_name("sessions_search")["inputSchema"]["required"],
            json!(["query"])
        );
        assert!(
            by_name("session_info")["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            by_name("read")["description"]
                .as_str()
                .unwrap()
                .contains("archive_entry")
        );
    }

    #[tokio::test]
    async fn tools_filter_narrows_the_list() {
        let cwd = temp_dir("tools-filter");
        let opts = ServeOpts {
            tools: vec!["grep".to_string(), "read".to_string()],
            read_only: true,
        };
        let (mut c, _handle) = TestClient::open(opts, cwd).await;
        let resp = c.request(6, "tools/list", json!({})).await;
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["read", "grep"]);
    }

    #[tokio::test]
    async fn tools_call_read_and_glob_and_grep_roundtrip() {
        let cwd = temp_dir("hands");
        std::fs::write(
            cwd.join("note.txt"),
            "the hidden gem lies here\nsecond line\n",
        )
        .unwrap();
        let (mut c, _handle) = TestClient::open(ServeOpts::default(), cwd.clone()).await;

        let resp = c
            .request(
                7,
                "tools/call",
                json!({"name": "read", "arguments": {"path": cwd.join("note.txt")}}),
            )
            .await;
        assert_eq!(resp["result"]["isError"], false);
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("the hidden gem lies here")
        );

        let resp = c
            .request(
                8,
                "tools/call",
                json!({"name": "glob", "arguments": {"pattern": "*.txt"}}),
            )
            .await;
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("note.txt")
        );

        let resp = c
            .request(
                9,
                "tools/call",
                json!({"name": "grep", "arguments": {"pattern": "hidden gem"}}),
            )
            .await;
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("note.txt:1:"), "{text}");
        assert_eq!(resp["result"]["isError"], false);
    }

    #[tokio::test]
    async fn tools_call_errors_come_back_as_is_error() {
        let cwd = temp_dir("errors");
        let (mut c, _handle) = TestClient::open(ServeOpts::default(), cwd).await;
        // Unknown tool name.
        let resp = c
            .request(10, "tools/call", json!({"name": "bash", "arguments": {}}))
            .await;
        assert_eq!(resp["result"]["isError"], true);
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("unknown tool")
        );
        // Known tool, missing required argument.
        let resp = c
            .request(11, "tools/call", json!({"name": "read", "arguments": {}}))
            .await;
        assert_eq!(resp["result"]["isError"], true);
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("missing required 'path'")
        );
        // Never-exposed tool is refused by name, not silently.
        let resp = c
            .request(
                12,
                "tools/call",
                json!({"name": "no_such_hand", "arguments": {}}),
            )
            .await;
        assert_eq!(resp["result"]["isError"], true);
    }

    #[tokio::test]
    async fn sessions_tools_report_an_empty_directory() {
        let cwd = temp_dir("sessions-empty");
        let (mut c, _handle) = TestClient::open(ServeOpts::default(), cwd).await;
        let resp = c
            .request(
                13,
                "tools/call",
                json!({"name": "sessions_search", "arguments": {"query": "anything"}}),
            )
            .await;
        assert_eq!(resp["result"]["isError"], false);
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("no sessions match")
        );
        let resp = c
            .request(
                14,
                "tools/call",
                json!({"name": "session_info", "arguments": {}}),
            )
            .await;
        assert_eq!(resp["result"]["isError"], false);
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("no sessions yet")
        );
    }

    /// Session tools run inline on the server task, which shares the
    /// test's thread under the current_thread runtime — so ka-strand's
    /// thread-local data-dir override redirects them without racing
    /// other tests.
    #[tokio::test]
    async fn sessions_search_and_info_find_a_real_session() {
        let cwd = temp_dir("sessions-real");
        ka_strand::set_data_dir_override(Some(temp_dir("sessions-data")));
        let mut strand = ka_strand::StrandFile::create(&cwd, None).unwrap();
        strand
            .append(ka_strand::Record::Message {
                id: ka_strand::new_record_id(),
                role: ka_strand::Role::User,
                content: "find the hidden gem".to_string(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
                images: Vec::new(),
            })
            .unwrap();
        strand
            .append(ka_strand::Record::Title {
                id: ka_strand::new_record_id(),
                title: "gem hunt".to_string(),
            })
            .unwrap();

        let (mut c, _handle) = TestClient::open(ServeOpts::default(), cwd.clone()).await;
        let resp = c
            .request(
                15,
                "tools/call",
                json!({"name": "sessions_search", "arguments": {"query": "gem"}}),
            )
            .await;
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(resp["result"]["isError"], false);
        assert!(text.contains("gem hunt"), "{text}");

        // Newest-by-default session_info, then by id prefix.
        let resp = c
            .request(
                16,
                "tools/call",
                json!({"name": "session_info", "arguments": {}}),
            )
            .await;
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("title    gem hunt"), "{text}");
        assert!(text.contains("messages 1"), "{text}");
        let id_line = text
            .lines()
            .find(|l| l.starts_with("id"))
            .unwrap()
            .to_string();
        let id = id_line.split_whitespace().nth(1).unwrap().to_string();
        let prefix = id.get(..3).unwrap();
        let resp = c
            .request(
                17,
                "tools/call",
                json!({"name": "session_info", "arguments": {"session": prefix}}),
            )
            .await;
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("gem hunt"), "{text}");
        // And a bogus reference is an isError, not a protocol error.
        let resp = c
            .request(
                18,
                "tools/call",
                json!({"name": "session_info", "arguments": {"session": "zzz"}}),
            )
            .await;
        assert_eq!(resp["result"]["isError"], true);

        ka_strand::set_data_dir_override(None);
    }

    #[tokio::test]
    async fn fresh_context_has_no_shared_state() {
        let cwd = temp_dir("ctx");
        let a = fresh_context(&cwd);
        let b = fresh_context(&cwd);
        assert!(!Arc::ptr_eq(&a.ledger, &b.ledger), "ledgers are per-call");
        assert!(!Arc::ptr_eq(&a.spill, &b.spill), "spills are per-call");
        assert_eq!(a.cwd, cwd);
        assert_eq!(a.sandbox, ka_sandbox::Policy::Off);
    }

    #[test]
    fn registry_honors_the_tool_filter() {
        let all = registry(&ServeOpts::default());
        assert_eq!(all.len(), 5);
        assert!(all.iter().all(|t| t.def.read_only), "v1 is read-only");
        let narrowed = registry(&ServeOpts {
            tools: vec!["grep".to_string(), "nope".to_string()],
            read_only: false,
        });
        let names: Vec<String> = narrowed.iter().map(|t| t.def.name.clone()).collect();
        assert_eq!(names, vec!["grep".to_string()]);
    }
}
