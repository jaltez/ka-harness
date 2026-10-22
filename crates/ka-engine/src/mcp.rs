//! Minimal MCP (Model Context Protocol) client: newline-delimited
//! JSON-RPC 2.0 over a spawned process (stdio), or streamable HTTP with
//! legacy-SSE fallback for HTTP endpoints. Hand-rolled on purpose —
//! the footprint budget has no room for rmcp, and ka only needs
//! initialize / tools-list / tools-call (plus resources and prompts).
//!
//! Lifecycle: [`McpClient::spawn_connect`] connects, handshakes, and
//! lists tools. Failures are per-server and non-fatal: the engine notes
//! them and carries on with the built-in hands.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc, oneshot};

/// A configured MCP server (one `[[mcp]]` table in ka.toml).
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Tool-name prefix (`<name>.<tool>`).
    pub name: String,
    /// Executable to run (stdio transport). Mutually exclusive with
    /// [`McpServerConfig::url`].
    #[serde(default)]
    pub command: Option<String>,
    /// Arguments for the executable (stdio transport).
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the process (stdio transport).
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// HTTP(S) endpoint (streamable-HTTP transport, with legacy-SSE
    /// fallback). Mutually exclusive with `command`.
    #[serde(default)]
    pub url: Option<String>,
    /// Extra HTTP headers (url transports, e.g. auth).
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

impl McpServerConfig {
    /// Exactly one transport must be configured.
    pub fn validate(&self) -> Result<(), String> {
        match (self.command.is_some(), self.url.is_some()) {
            (true, false) | (false, true) => Ok(()),
            (true, true) => Err(format!(
                "mcp {}: set either command or url, not both",
                self.name
            )),
            (false, false) => Err(format!(
                "mcp {}: one of command or url is required",
                self.name
            )),
        }
    }
}

/// One tool advertised by a server.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct McpTool {
    /// Wire-facing tool name (`server__tool`, `[a-zA-Z0-9_-]` only).
    pub name: String,
    /// Raw tool name on the server.
    pub raw_name: String,
    /// Human/model-facing description.
    pub description: String,
    /// JSON schema for the arguments object.
    pub schema: Value,
}

/// One resource advertised by a server.
#[derive(Debug, Clone, PartialEq)]
pub struct McpResource {
    /// Resource URI.
    pub uri: String,
    /// Display name.
    pub name: String,
    /// Human/model-facing description.
    pub description: String,
    /// MIME type (best known).
    pub mime_type: String,
}

/// One prompt advertised by a server.
#[derive(Debug, Clone, PartialEq)]
pub struct McpPrompt {
    /// Owning server name.
    pub server: String,
    /// Raw prompt name.
    pub name: String,
    /// Human/model-facing description.
    pub description: String,
    /// Argument names the prompt takes.
    pub arguments: Vec<String>,
}

/// Protocol version ka speaks (negotiation tolerates others).
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Per-request response timeout.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Connection to one MCP server over stdio or HTTP.
#[derive(Debug)]
pub struct McpClient {
    server_name: String,
    transport: Transport,
    /// Responses routed by id (shared with the stdio/SSE reader tasks).
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
    /// Server-initiated notifications/requests (logged, not handled).
    _noise: mpsc::Receiver<String>,
}

/// How one client talks to its server.
#[derive(Debug)]
enum Transport {
    /// Newline-delimited JSON-RPC over a spawned process.
    Stdio { child: Child, stdin: ChildStdin },
    /// Streamable HTTP: each request POSTs and the response comes back
    /// on the POST (JSON body or an SSE stream).
    StreamableHttp(HttpRpc),
    /// Legacy SSE: requests POST to the messages endpoint, responses
    /// arrive on the long-lived GET stream (routed by reader task).
    LegacySse(HttpRpc),
}

/// POST side of the HTTP transports: target URL, static headers, and
/// the `Mcp-Session-Id` the server assigned (streamable HTTP).
#[derive(Debug)]
struct HttpRpc {
    url: String,
    headers: Vec<(String, String)>,
    session_id: Option<String>,
    client: reqwest::Client,
}

/// Transport-level failure. The message carries the HTTP status when
/// one was received (a 404/405 on a streamable POST hints at a
/// legacy-SSE-only server — see [`McpClient::legacy_fallback_possible`]).
#[derive(Debug)]
struct HttpError {
    message: String,
}

impl HttpRpc {
    fn new(url: String, headers: Vec<(String, String)>) -> Self {
        Self {
            url,
            headers,
            session_id: None,
            client: reqwest::Client::new(),
        }
    }

    fn apply_common(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut req = req
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION);
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        req
    }

    /// POST one JSON-RPC message. `Ok(None)` = accepted with no body
    /// (notifications / 202).
    async fn post(&mut self, msg: &Value) -> Result<Option<Value>, HttpError> {
        let resp = self
            .apply_common(self.client.post(&self.url))
            .json(msg)
            .send()
            .await
            .map_err(|e| HttpError {
                message: format!("mcp http: {e}"),
            })?;
        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            self.session_id = Some(sid.to_string());
        }
        let status = resp.status();
        if !status.is_success() {
            return Err(HttpError {
                message: format!("mcp http: status {status}"),
            });
        }
        if status.as_u16() == 202 {
            return Ok(None);
        }
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp.text().await.map_err(|e| HttpError {
            message: format!("mcp http: {e}"),
        })?;
        if ct.contains("event-stream") {
            return Ok(first_sse_response(&body));
        }
        Ok(serde_json::from_str::<Value>(&body).ok())
    }
}

/// Pull the first JSON-RPC response out of an SSE body.
fn first_sse_response(body: &str) -> Option<Value> {
    for frame in body.split("\n\n") {
        for line in frame.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                    if v.get("result").is_some() || v.get("error").is_some() {
                        return Some(v);
                    }
                }
            }
        }
    }
    None
}

impl McpClient {
    /// Connect to the server (spawn or HTTP), run the initialize
    /// handshake, and list tools. One call so callers get a
    /// fully-usable client or an error string.
    pub async fn spawn_connect(cfg: &McpServerConfig) -> Result<(Self, Vec<McpTool>), String> {
        cfg.validate()?;
        match (&cfg.command, &cfg.url) {
            (Some(command), _) => Self::spawn_stdio(cfg, command).await,
            (None, Some(url)) => Self::connect_http(cfg, url).await,
            (None, None) => Err("mcp: unreachable: unvalidated config".to_string()),
        }
    }

    /// Stdio transport: boot the server process.
    async fn spawn_stdio(
        cfg: &McpServerConfig,
        command: &str,
    ) -> Result<(Self, Vec<McpTool>), String> {
        let mut child = Command::new(command)
            .args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("spawn {command} failed: {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "server stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "server stdout unavailable".to_string())?;
        let (noise_tx, noise_rx) = mpsc::channel(64);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(read_loop(stdout, pending.clone(), noise_tx));
        let mut client = Self {
            server_name: cfg.name.clone(),
            transport: Transport::Stdio { child, stdin },
            pending,
            next_id: AtomicU64::new(1),
            _noise: noise_rx,
        };
        client.initialize().await?;
        let tools = client.list_tools().await?;
        Ok((client, tools))
    }

    /// HTTP transport: try streamable HTTP first; on a 404/405 from the
    /// POST fall back to a legacy-SSE server (GET endpoint event gives
    /// the messages URL).
    async fn connect_http(
        cfg: &McpServerConfig,
        url: &str,
    ) -> Result<(Self, Vec<McpTool>), String> {
        let rpc = HttpRpc::new(url.to_string(), cfg.headers.clone());
        let mut client = Self {
            server_name: cfg.name.clone(),
            transport: Transport::StreamableHttp(rpc),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            _noise: mpsc::channel(64).1,
        };
        match client.initialize().await {
            Ok(()) => {}
            Err(e) if client.legacy_fallback_possible(&e) => {
                let rpc = client.legacy_endpoint_rpc().await?;
                client.transport = Transport::LegacySse(rpc);
                client.initialize().await?;
            }
            Err(e) => return Err(e),
        }
        let tools = client.list_tools().await?;
        Ok((client, tools))
    }

    /// Whether an initialize failure should trigger a legacy-SSE retry.
    fn legacy_fallback_possible(&self, err: &str) -> bool {
        matches!(self.transport, Transport::StreamableHttp(_))
            && (err.contains("status 404") || err.contains("status 405"))
    }

    /// Open the legacy-SSE GET stream and learn the messages endpoint.
    async fn legacy_endpoint_rpc(&mut self) -> Result<HttpRpc, String> {
        let Transport::StreamableHttp(old) = &self.transport else {
            return Err("legacy fallback: not an HTTP transport".to_string());
        };
        let base = old.url.clone();
        let headers = old.headers.clone();
        let client = old.client.clone();
        let resp = client
            .get(&base)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(|e| format!("mcp sse: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("mcp sse: status {}", resp.status()));
        }
        let (ep_tx, ep_rx) = oneshot::channel::<String>();
        let pending = self.pending.clone();
        let (noise_tx, noise_rx) = mpsc::channel(64);
        self._noise = noise_rx;
        tokio::spawn(sse_read_loop(resp, pending, noise_tx, Some(ep_tx)));
        let endpoint = tokio::time::timeout(REQUEST_TIMEOUT, ep_rx)
            .await
            .map_err(|_| "mcp sse: endpoint event timed out".to_string())?
            .map_err(|_| "mcp sse: reader dropped".to_string())?;
        let post_url = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            endpoint
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("http", &base));
            let host = rest.split('/').next().unwrap_or(rest);
            format!("{scheme}://{host}{endpoint}")
        };
        Ok(HttpRpc {
            url: post_url,
            headers,
            session_id: None,
            client,
        })
    }

    /// The configured server name.
    pub fn name(&self) -> &str {
        &self.server_name
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match &mut self.transport {
            // streamable HTTP: the response rides the POST itself
            Transport::StreamableHttp(rpc) => {
                let resp = tokio::time::timeout(REQUEST_TIMEOUT, rpc.post(&msg))
                    .await
                    .map_err(|_| format!("{method}: request timed out"))?
                    .map_err(|e| e.message)?
                    .ok_or_else(|| format!("{method}: no response body"))?;
                if let Some(err) = resp.get("error") {
                    return Err(format!("{method}: {err}"));
                }
                Ok(resp["result"].clone())
            }
            // stdio and legacy SSE: the write only submits the request;
            // responses arrive via the reader task, routed by id
            _ => {
                let (tx, rx) = oneshot::channel();
                self.pending.lock().await.insert(id, tx);
                self.send_msg(&msg).await?;
                let resp = tokio::time::timeout(REQUEST_TIMEOUT, rx)
                    .await
                    .map_err(|_| format!("{method}: server timed out"))?
                    .map_err(|_| format!("{method}: reader dropped the reply"))?;
                if let Some(err) = resp.get("error") {
                    return Err(format!("{method}: {err}"));
                }
                Ok(resp["result"].clone())
            }
        }
    }

    /// Submit a message on the write side (stdin line or HTTP POST).
    async fn send_msg(&mut self, msg: &Value) -> Result<(), String> {
        match &mut self.transport {
            Transport::Stdio { stdin, .. } => {
                let line = serde_json::to_string(msg).map_err(|e| format!("serialize: {e}"))?;
                stdin
                    .write_all(line.as_bytes())
                    .await
                    .map_err(|e| format!("write to server: {e}"))?;
                stdin
                    .write_all(b"\n")
                    .await
                    .map_err(|e| format!("write to server: {e}"))?;
                stdin
                    .flush()
                    .await
                    .map_err(|e| format!("write to server: {e}"))
            }
            Transport::StreamableHttp(rpc) | Transport::LegacySse(rpc) => {
                rpc.post(msg).await.map(|_| ()).map_err(|e| e.message)?;
                Ok(())
            }
        }
    }

    async fn initialize(&mut self) -> Result<(), String> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "ka", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await?;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send_msg(&msg).await
    }

    async fn list_tools(&mut self) -> Result<Vec<McpTool>, String> {
        let result = self.request("tools/list", json!({})).await?;
        parse_tools(&result, &self.server_name)
    }

    /// Invoke a tool; returns the concatenated text content.
    pub async fn call_tool(&mut self, tool: &str, args: Value) -> Result<String, String> {
        let result = self
            .request(
                "tools/call",
                json!({"name": tool, "arguments": args.as_object().cloned().unwrap_or_default()}),
            )
            .await?;
        if result["isError"].as_bool().unwrap_or(false) {
            return Err(extract_text(&result));
        }
        Ok(extract_text(&result))
    }

    /// List resources advertised by the server.
    pub async fn list_resources(&mut self) -> Result<Vec<McpResource>, String> {
        let result = self.request("resources/list", json!({})).await?;
        let items = result["resources"].as_array().ok_or_else(|| {
            "resources/list: no resources array (server may lack resources support)".to_string()
        })?;
        Ok(items
            .iter()
            .filter_map(|r| {
                Some(McpResource {
                    uri: r["uri"].as_str()?.to_string(),
                    name: r["name"].as_str().unwrap_or("").to_string(),
                    description: r["description"].as_str().unwrap_or("").to_string(),
                    mime_type: r["mimeType"].as_str().unwrap_or("text/plain").to_string(),
                })
            })
            .collect())
    }

    /// Read one resource; returns the concatenated text contents.
    pub async fn read_resource(&mut self, uri: &str) -> Result<String, String> {
        let result = self.request("resources/read", json!({"uri": uri})).await?;
        let parts = result["contents"]
            .as_array()
            .ok_or_else(|| "resources/read: no contents array".to_string())?;
        let text: Vec<&str> = parts.iter().filter_map(|p| p["text"].as_str()).collect();
        if text.is_empty() {
            return Err(format!("resources/read {uri}: no text content"));
        }
        Ok(text.join("\n"))
    }

    /// List prompts advertised by the server with their argument names.
    pub async fn list_prompts(&mut self) -> Result<Vec<McpPrompt>, String> {
        let result = self.request("prompts/list", json!({})).await?;
        let items = result["prompts"].as_array().ok_or_else(|| {
            "prompts/list: no prompts array (server may lack prompts support)".to_string()
        })?;
        Ok(items
            .iter()
            .filter_map(|p| {
                Some(McpPrompt {
                    server: self.server_name.clone(),
                    name: p["name"].as_str()?.to_string(),
                    description: p["description"].as_str().unwrap_or("").to_string(),
                    arguments: p["arguments"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|arg| arg["name"].as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                })
            })
            .collect())
    }

    /// Fetch a prompt; returns its messages as `role: text` lines.
    pub async fn get_prompt(
        &mut self,
        name: &str,
        args: &HashMap<String, String>,
    ) -> Result<String, String> {
        let result = self
            .request(
                "prompts/get",
                json!({
                    "name": name,
                    "arguments": args,
                }),
            )
            .await?;
        let messages = result["messages"]
            .as_array()
            .ok_or_else(|| "prompts/get: no messages array".to_string())?;
        let rendered: Vec<String> = messages
            .iter()
            .map(|m| {
                let role = m["role"].as_str().unwrap_or("user");
                let text = m
                    .pointer("/content/text")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                format!("{role}: {text}")
            })
            .collect();
        if rendered.is_empty() {
            return Err(format!("prompts/get {name}: empty"));
        }
        Ok(rendered.join("\n\n"))
    }

    /// Whether the server is still reachable: process liveness on
    /// stdio, an MCP `ping` round-trip on HTTP transports.
    pub async fn alive(&mut self) -> bool {
        match &mut self.transport {
            Transport::Stdio { child, .. } => matches!(child.try_wait(), Ok(None)),
            _ => self.request("ping", json!({})).await.is_ok(),
        }
    }

    /// Reconnect after a transport death: rebuild the same transport
    /// from the config and re-handshake. Returns the fresh tools list
    /// so callers can diff against the old one.
    pub async fn reconnect(cfg: &McpServerConfig) -> Result<(Self, Vec<McpTool>), String> {
        Self::spawn_connect(cfg).await
    }
}

/// Long-running MCP bookkeeping messages the engine consumes to keep
/// its hand registry in step with reconnected/refreshed servers.
#[derive(Debug)]
pub enum Maintenance {
    /// A watchdog reconnected the server; the hand set is replaced.
    McpReconnected {
        server: String,
        tools: Vec<McpTool>,
        gained: i64,
    },
    /// A reconnect attempt chain failed; hands stay erroring.
    McpGaveUp { server: String },
    /// An explicit refresh observed a tool-set change.
    McpRefreshed {
        server: String,
        tools: Vec<McpTool>,
        delta: i64,
    },
    /// The background verification of a cache-started server finished:
    /// `changed` says whether the live list differs from the cached
    /// one the hands were registered from. The engine swaps hands only
    /// on change and refreshes the cache either way.
    McpVerified {
        server: String,
        tools: Vec<McpTool>,
        changed: bool,
    },
}

/// Watchdog timing (tests shrink everything).
#[derive(Debug, Clone)]
pub struct WatchdogTiming {
    /// Liveness poll interval.
    pub poll: std::time::Duration,
    /// Reconnect backoff steps before giving up.
    pub backoffs: [std::time::Duration; 3],
}

impl Default for WatchdogTiming {
    fn default() -> Self {
        Self {
            poll: std::time::Duration::from_secs(30),
            backoffs: [
                std::time::Duration::from_secs(2),
                std::time::Duration::from_secs(8),
                std::time::Duration::from_secs(32),
            ],
        }
    }
}

/// Shared, reconnectable handle to one MCP server: hands call through
/// it and the watchdog swaps the client after a successful reconnect.
#[derive(Clone)]
pub struct McpShared {
    cfg: McpServerConfig,
    cell: Arc<Mutex<Option<McpClient>>>,
    tools: Arc<parking_lot::Mutex<Vec<McpTool>>>,
}

impl McpShared {
    /// Wrap a freshly connected client and its tool list.
    pub fn new(cfg: McpServerConfig, client: McpClient, tools: Vec<McpTool>) -> Self {
        Self {
            cfg,
            cell: Arc::new(Mutex::new(Some(client))),
            tools: Arc::new(parking_lot::Mutex::new(tools)),
        }
    }

    /// The configured server name.
    pub fn name(&self) -> &str {
        &self.cfg.name
    }

    /// The current tool list (empty while disconnected).
    pub fn tools(&self) -> Vec<McpTool> {
        self.tools.lock().clone()
    }

    /// A not-yet-connected shell over a CACHED tool list: hands can be
    /// registered against it immediately (startup never blocks on the
    /// handshake), calls answer "disconnected" until a client lands in
    /// the cell, and [`verify_initial`] fills it.
    pub fn placeholder(cfg: McpServerConfig, tools: Vec<McpTool>) -> Self {
        Self {
            cfg,
            cell: Arc::new(Mutex::new(None)),
            tools: Arc::new(parking_lot::Mutex::new(tools)),
        }
    }

    /// Whether the current client answers a liveness probe.
    pub async fn alive(&self) -> bool {
        let mut guard = self.cell.lock().await;
        match guard.as_mut() {
            Some(c) => c.alive().await,
            None => false,
        }
    }

    /// Invoke a tool through the current client.
    pub async fn call_tool(&self, tool: &str, args: Value) -> Result<String, String> {
        let mut guard = self.cell.lock().await;
        match guard.as_mut() {
            Some(c) => c.call_tool(tool, args).await,
            None => Err(format!("mcp {}: disconnected", self.cfg.name)),
        }
    }

    /// List resources through the current client.
    pub async fn list_resources(&self) -> Result<Vec<McpResource>, String> {
        let mut guard = self.cell.lock().await;
        let Some(c) = guard.as_mut() else {
            return Err(format!("mcp {}: disconnected", self.cfg.name));
        };
        c.list_resources().await
    }

    /// Read one resource through the current client.
    pub async fn read_resource(&self, uri: &str) -> Result<String, String> {
        let mut guard = self.cell.lock().await;
        let Some(c) = guard.as_mut() else {
            return Err(format!("mcp {}: disconnected", self.cfg.name));
        };
        c.read_resource(uri).await
    }

    /// List prompts across this server.
    pub async fn list_prompts(&self) -> Result<Vec<McpPrompt>, String> {
        let mut guard = self.cell.lock().await;
        let Some(c) = guard.as_mut() else {
            return Err(format!("mcp {}: disconnected", self.cfg.name));
        };
        c.list_prompts().await
    }

    /// Fetch and render one prompt.
    pub async fn get_prompt(
        &self,
        name: &str,
        args: &HashMap<String, String>,
    ) -> Result<String, String> {
        let mut guard = self.cell.lock().await;
        let Some(c) = guard.as_mut() else {
            return Err(format!("mcp {}: disconnected", self.cfg.name));
        };
        c.get_prompt(name, args).await
    }

    /// Explicit tool refresh: re-list, install, and report the delta.
    pub async fn refresh_and_install(&self) -> Result<(Vec<McpTool>, i64), String> {
        let tools = self.refresh_tools().await?;
        let delta = tools.len() as i64 - self.tool_count() as i64;
        *self.tools.lock() = tools.clone();
        Ok((tools, delta))
    }

    /// List tools through the current client (explicit refresh).
    pub async fn refresh_tools(&self) -> Result<Vec<McpTool>, String> {
        let mut guard = self.cell.lock().await;
        let Some(c) = guard.as_mut() else {
            return Err(format!("mcp {}: disconnected", self.cfg.name));
        };
        let result = c.request("tools/list", json!({})).await?;
        parse_tools(&result, &self.cfg.name)
    }

    async fn install(&self, client: McpClient, tools: Vec<McpTool>) {
        *self.cell.lock().await = Some(client);
        *self.tools.lock() = tools;
    }

    async fn mark_down(&self) {
        self.cell.lock().await.take();
    }

    fn tool_count(&self) -> usize {
        self.tools.lock().len()
    }

    /// Kill the current stdio server process (watchdog tests).
    #[cfg(test)]
    pub async fn test_kill(&self) {
        if let Some(c) = self.cell.lock().await.as_mut() {
            if let Transport::Stdio { child, .. } = &mut c.transport {
                let _ = child.start_kill();
            }
        }
    }
}

/// Watch one server: poll liveness on [`WatchdogTiming::poll`]; on
/// death retry `spawn_connect` through the backoff steps, swapping the
/// shared client on success and reporting the tool diff. After the
/// last backoff fails the watchdog gives up (hands stay erroring).
pub async fn supervise(
    shared: McpShared,
    maintenance: mpsc::Sender<Maintenance>,
    timing: WatchdogTiming,
) {
    loop {
        tokio::time::sleep(timing.poll).await;
        if shared.alive().await {
            continue;
        }
        shared.mark_down().await;
        let mut restored = None;
        for delay in timing.backoffs {
            tokio::time::sleep(delay).await;
            if let Ok((client, tools)) = McpClient::spawn_connect(&shared.cfg).await {
                restored = Some((client, tools));
                break;
            }
        }
        match restored {
            Some((client, tools)) => {
                let gained = tools.len() as i64 - shared.tool_count() as i64;
                shared.install(client, tools.clone()).await;
                maintenance
                    .send(Maintenance::McpReconnected {
                        server: shared.name().to_string(),
                        tools,
                        gained,
                    })
                    .await
                    .ok();
            }
            None => {
                maintenance
                    .send(Maintenance::McpGaveUp {
                        server: shared.name().to_string(),
                    })
                    .await
                    .ok();
                return;
            }
        }
    }
}

/// Immediate background handshake for a cache-started server, then —
/// success or failure — the watchdog takes over: chaining (rather
/// than spawning both) serializes the initial connect against
/// supervise's reconnects, so exactly one spawn_connect is ever in
/// flight and a slow-but-healthy server can't race a second process
/// or a bogus give-up.
pub async fn verify_initial(
    shared: McpShared,
    maintenance: mpsc::Sender<Maintenance>,
    timing: WatchdogTiming,
) {
    let cached = shared.tools();
    let cfg = shared.cfg.clone();
    if let Ok((client, tools)) = McpClient::spawn_connect(&cfg).await {
        let changed = tools != cached;
        shared.install(client, tools.clone()).await;
        maintenance
            .send(Maintenance::McpVerified {
                server: shared.name().to_string(),
                tools,
                changed,
            })
            .await
            .ok();
    }
    supervise(shared, maintenance, timing).await;
}

/// Parse a tools/list result into prefixed [`McpTool`]s.
fn parse_tools(result: &Value, server_name: &str) -> Result<Vec<McpTool>, String> {
    let tools = result["tools"]
        .as_array()
        .ok_or_else(|| "tools/list: no tools array".to_string())?;
    Ok(tools
        .iter()
        .filter_map(|t| {
            let raw_name = t["name"].as_str()?.to_string();
            let description = t["description"].as_str().unwrap_or("").to_string();
            let schema = t
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Some(McpTool {
                name: wire_name(server_name, &raw_name),
                raw_name,
                description,
                schema,
            })
        })
        .collect())
}
/// The name the model sees: `{server}__{raw}` with every character
/// outside `[a-zA-Z0-9_-]` collapsed to `_`. Providers reject function
/// names off that pattern (`tools[n].function.name`), so a dotted
/// `server.tool` namespace never reaches the wire.
pub(crate) fn wire_name(server: &str, raw: &str) -> String {
    let clean = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    format!("{}__{}", clean(server), clean(raw))
}

/// Concatenate a tool result's text content parts.
fn extract_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Reader task: one JSON-RPC message per line, responses routed by id.
async fn read_loop(
    stdout: ChildStdout,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    noise: mpsc::Sender<String>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        route_message(msg, &pending, &noise).await;
    }
}

/// Route one decoded JSON-RPC message: responses by id, methods to the
/// noise channel.
async fn route_message(
    msg: Value,
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    noise: &mpsc::Sender<String>,
) {
    if let Some(id) = msg["id"].as_u64() {
        if let Some(tx) = pending.lock().await.remove(&id) {
            tx.send(msg).ok();
        }
    } else if let Some(method) = msg["method"].as_str() {
        let _ = noise.send(method.to_string()).await;
    }
}

/// Legacy-SSE reader task: consume the long-lived GET stream, capture
/// the `endpoint` event, route subsequent data frames like stdio lines.
async fn sse_read_loop(
    mut resp: reqwest::Response,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    noise: mpsc::Sender<String>,
    endpoint_tx: Option<oneshot::Sender<String>>,
) {
    let mut buf = String::new();
    let mut endpoint_tx = endpoint_tx;
    loop {
        match resp.chunk().await {
            Ok(Some(bytes)) => {
                buf.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(pos) = buf.find("\n\n") {
                    let frame: String = buf.drain(..pos + 2).collect();
                    let mut event = String::new();
                    let mut data = String::new();
                    for line in frame.lines() {
                        if let Some(v) = line.strip_prefix("event:") {
                            event = v.trim().to_string();
                        } else if let Some(v) = line.strip_prefix("data:") {
                            if !data.is_empty() {
                                data.push('\n');
                            }
                            data.push_str(v.trim());
                        }
                    }
                    if event == "endpoint" {
                        if let Some(tx) = endpoint_tx.take() {
                            tx.send(data.clone()).ok();
                        }
                        continue;
                    }
                    let Ok(msg) = serde_json::from_str::<Value>(&data) else {
                        continue;
                    };
                    route_message(msg, &pending, &noise).await;
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
}

/// Bridge an MCP tool into the hand registry. External tools always pay
/// the exec-tier gate: the engine knows nothing about what they do.
pub struct McpHand {
    tool: McpTool,
    shared: McpShared,
}

impl McpHand {
    /// Wrap one advertised tool.
    pub fn new(tool: McpTool, shared: McpShared) -> Self {
        Self { tool, shared }
    }
}

impl crate::hands::Hand for McpHand {
    fn def(&self) -> crate::hands::HandDef {
        crate::hands::HandDef {
            name: self.tool.name.clone(),
            description: if self.tool.description.is_empty() {
                format!("MCP tool {}", self.tool.name)
            } else {
                self.tool.description.clone()
            },
            parameters: self.tool.schema.clone(),
            clearance: crate::hands::Clearance::Exec,
            read_only: false,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _ctx: &'a crate::hands::HandContext,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::hands::ToolOutput> + Send + 'a>> {
        let tool = self.tool.raw_name.clone();
        let args = args.clone();
        let shared = self.shared.clone();
        Box::pin(async move {
            match shared.call_tool(&tool, args).await {
                Ok(text) if text.trim().is_empty() => {
                    crate::hands::ToolOutput::ok("(empty result)".to_string())
                }
                Ok(text) => crate::hands::ToolOutput::ok(text),
                Err(e) => crate::hands::ToolOutput::err(e),
            }
        })
    }
}

/// Lazy-discovery call hand ([tools.mcp] discovery = "lazy"): one hand
/// stands in for every server's tools, so a fleet of MCP servers costs
/// one tool definition instead of dozens. Omitting `tool` lists a
/// server's tools; with `tool` it invokes. Same exec-tier gate as the
/// eager per-tool hands. Per-server feature toggles (`mcp:<name>`)
/// filter the listing and reject calls at execute time.
pub struct McpCallHand {
    servers: Vec<McpShared>,
    features: crate::features::FeatureSlot,
}

impl McpCallHand {
    /// One hand over every connected server.
    pub fn new(servers: Vec<McpShared>, features: crate::features::FeatureSlot) -> Self {
        Self { servers, features }
    }

    /// Servers visible to the model right now.
    fn visible(&self) -> Vec<&McpShared> {
        let features = self.features.read();
        self.servers
            .iter()
            .filter(|s| features.server_enabled(s.name()))
            .collect()
    }

    fn find(&self, server: &str) -> Result<&McpShared, String> {
        if let Some(shared) = self.servers.iter().find(|s| s.name() == server) {
            if !self.features.read().server_enabled(server) {
                return Err(format!(
                    "mcp_call: server {server:?} is disabled by a session feature toggle \
                     (`mcp:{server}`); it is unavailable until the user re-enables it"
                ));
            }
            return Ok(shared);
        }
        Err(format!(
            "mcp_call: no such server {server:?} (have: {})",
            self.visible()
                .iter()
                .map(|s| s.name())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

impl crate::hands::Hand for McpCallHand {
    fn def(&self) -> crate::hands::HandDef {
        let mut listing = String::from(
            "Call a tool on a configured MCP server (external execution). Omit `tool` \
             to list that server's tools first. Servers: ",
        );
        for s in self.visible() {
            listing.push_str(&format!("{} ({} tools), ", s.name(), s.tools().len()));
        }
        crate::hands::HandDef {
            name: "mcp_call".to_string(),
            description: listing,
            parameters: json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "Server name (required)"},
                    "tool": {"type": "string", "description": "Tool name from the listing; omit to list tools"},
                    "arguments": {"type": "object", "description": "Tool arguments (default {})"}
                },
                "required": ["server"]
            }),
            clearance: crate::hands::Clearance::Exec,
            read_only: false,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _ctx: &'a crate::hands::HandContext,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::hands::ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let Some(server) = args.get("server").and_then(Value::as_str) else {
                return crate::hands::ToolOutput::err("mcp_call: missing required 'server'");
            };
            let shared = match self.find(server) {
                Ok(s) => s,
                Err(e) => return crate::hands::ToolOutput::err(e),
            };
            let Some(tool) = args.get("tool").and_then(Value::as_str) else {
                let mut out = String::new();
                for t in shared.tools() {
                    let desc = if t.description.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", truncate_chars(&t.description, 140))
                    };
                    out.push_str(&format!("- {}{desc}\n", t.name));
                }
                return if out.is_empty() {
                    crate::hands::ToolOutput::ok(format!(
                        "mcp {server}: no tools listed (disconnected?)"
                    ))
                } else {
                    crate::hands::ToolOutput::ok(out)
                };
            };
            let raw = shared
                .tools()
                .iter()
                .find(|t| t.name == tool || t.raw_name == tool)
                .map(|t| t.raw_name.clone())
                .unwrap_or_else(|| tool.to_string());
            let call_args = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match shared.call_tool(&raw, call_args).await {
                Ok(text) if text.trim().is_empty() => {
                    crate::hands::ToolOutput::ok("(empty result)".to_string())
                }
                Ok(text) => crate::hands::ToolOutput::ok(text),
                Err(e) => crate::hands::ToolOutput::err(e),
            }
        })
    }
}

/// Char-boundary truncate with ellipsis (listing caps).
fn truncate_chars(s: &str, cap: usize) -> String {
    if s.chars().count() > cap {
        let cut: String = s.chars().take(cap).collect();
        format!("{cut}…")
    } else {
        s.to_string()
    }
}

/// The built-in `mcp` meta-hand: browse server resources and prompts
/// without burning a model turn on discovery. Read-clearance: listing
/// and reading never mutate server state. Per-server feature toggles
/// (`mcp:<name>`) filter the listing and reject calls at execute time.
pub struct McpMetaHand {
    servers: Vec<McpShared>,
    features: crate::features::FeatureSlot,
}

impl McpMetaHand {
    /// One hand over every connected server.
    pub fn new(servers: Vec<McpShared>, features: crate::features::FeatureSlot) -> Self {
        Self { servers, features }
    }

    fn find(&self, server: &str) -> Result<&McpShared, String> {
        if let Some(shared) = self.servers.iter().find(|s| s.name() == server) {
            if !self.features.read().server_enabled(server) {
                return Err(format!(
                    "mcp: server {server:?} is disabled by a session feature toggle \
                     (`mcp:{server}`); it is unavailable until the user re-enables it"
                ));
            }
            return Ok(shared);
        }
        Err(format!(
            "mcp: no such server {server:?} (have: {})",
            self.servers
                .iter()
                .map(|s| s.name())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

impl crate::hands::Hand for McpMetaHand {
    fn def(&self) -> crate::hands::HandDef {
        crate::hands::HandDef {
            name: "mcp".to_string(),
            description: "Browse MCP servers: list resources, read one resource, or list                           prompts. Actions: {\"action\": \"resources\", \"server\": \"name\"} |                           {\"action\": \"resource\", \"server\": \"name\", \"uri\": \"...\"} |                           {\"action\": \"prompts\"}."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["resources", "resource", "prompts"],
                               "description": "Which browse operation to run"},
                    "server": {"type": "string", "description": "Server name (required)"},
                    "uri": {"type": "string", "description": "Resource URI (resource action)"}
                },
                "required": ["action"]
            }),
            clearance: crate::hands::Clearance::Read,
            read_only: true,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _ctx: &'a crate::hands::HandContext,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::hands::ToolOutput> + Send + 'a>> {
        let servers = self.servers.clone();
        Box::pin(async move {
            let action = args["action"].as_str().unwrap_or("");
            let server = args["server"].as_str().unwrap_or_default().to_string();
            match action {
                "resources" => {
                    let mut out = String::new();
                    for shared in &servers {
                        if !server.is_empty() && shared.name() != server {
                            continue;
                        }
                        match shared.list_resources().await {
                            Ok(items) => {
                                out.push_str(&format!(
                                    "{}: {} resource(s)\n",
                                    shared.name(),
                                    items.len()
                                ));
                                for r in items {
                                    out.push_str(&format!(
                                        "  {} — {} ({})\n",
                                        r.uri,
                                        if r.name.is_empty() {
                                            &r.description
                                        } else {
                                            &r.name
                                        },
                                        r.mime_type
                                    ));
                                }
                            }
                            Err(e) => out.push_str(&format!("{}: {e}\n", shared.name())),
                        }
                    }
                    if out.is_empty() {
                        out.push_str("(no servers matched)\n");
                    }
                    crate::hands::ToolOutput::ok(out)
                }
                "resource" => {
                    if server.is_empty() {
                        return crate::hands::ToolOutput::err("resource: 'server' required");
                    }
                    let Some(uri) = args["uri"].as_str() else {
                        return crate::hands::ToolOutput::err("resource: 'uri' required");
                    };
                    let shared = match self.find(&server) {
                        Ok(s) => s,
                        Err(e) => return crate::hands::ToolOutput::err(e),
                    };
                    match shared.read_resource(uri).await {
                        Ok(text) => crate::hands::ToolOutput::ok(text),
                        Err(e) => crate::hands::ToolOutput::err(e),
                    }
                }
                "prompts" => {
                    let mut out = String::new();
                    for shared in &servers {
                        match shared.list_prompts().await {
                            Ok(items) => {
                                for p in items {
                                    out.push_str(&format!(
                                        "{}/{} ({})\n",
                                        p.server,
                                        p.name,
                                        if p.arguments.is_empty() {
                                            "no args".to_string()
                                        } else {
                                            format!("args: {}", p.arguments.join(", "))
                                        }
                                    ));
                                }
                            }
                            Err(e) => out.push_str(&format!("{}: {e}\n", shared.name())),
                        }
                    }
                    if out.is_empty() {
                        out.push_str("(no prompts advertised)\n");
                    }
                    crate::hands::ToolOutput::ok(out)
                }
                other => crate::hands::ToolOutput::err(format!(
                    "mcp: unknown action {other:?} (resources | resource | prompts)"
                )),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A fake MCP stdio server in python: initialize, tools/list (one
    /// echo tool), tools/call returns the arguments as text.
    const FAKE_SERVER: &str = r#"
import json, sys
def send(o): sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    try: m = json.loads(line)
    except Exception: continue
    if m.get("method") == "initialize":
        send({"jsonrpc":"2.0","id":m["id"],"result":{"protocolVersion":m["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0"}}})
    elif m.get("method") == "notifications/initialized":
        pass
    elif m.get("method") == "tools/list":
        send({"jsonrpc":"2.0","id":m["id"],"result":{"tools":[{"name":"echo","description":"echo the text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}})
    elif m.get("method") == "tools/call":
        if m["params"]["name"] != "echo":
            send({"jsonrpc":"2.0","id":m["id"],"result":{"isError":True,"content":[{"type":"text","text":"unknown tool: " + m["params"]["name"]}]}})
        else:
            text = m["params"]["arguments"].get("text","")
            send({"jsonrpc":"2.0","id":m["id"],"result":{"content":[{"type":"text","text":"echo: " + text}]}})
"#;

    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn cfg() -> McpServerConfig {
        McpServerConfig {
            name: "fake".to_string(),
            command: Some("python3".to_string()),
            args: vec!["-c".to_string(), FAKE_SERVER.to_string()],
            env: HashMap::new(),
            url: None,
            headers: Vec::new(),
        }
    }

    #[tokio::test]
    async fn handshake_lists_and_calls_tools() {
        if !python3_available() {
            return;
        }
        let (mut client, tools) = McpClient::spawn_connect(&cfg()).await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "fake__echo");
        assert!(tools[0].description.contains("echo"));
        let out = client
            .call_tool("echo", json!({"text": "hello"}))
            .await
            .unwrap();
        assert_eq!(out, "echo: hello");
        assert!(client.alive().await);
    }

    #[tokio::test]
    async fn unknown_tool_errors() {
        if !python3_available() {
            return;
        }
        let (mut client, _tools) = McpClient::spawn_connect(&cfg()).await.unwrap();
        let err = client.call_tool("nope", json!({})).await.unwrap_err();
        assert!(err.contains("unknown tool"), "{err}");
    }

    #[tokio::test]
    async fn bad_command_reports_spawn_error() {
        let mut bad = cfg();
        bad.command = Some("ka-no-such-binary".to_string());
        let err = McpClient::spawn_connect(&bad).await.unwrap_err();
        assert!(err.contains("spawn"), "{err}");
    }

    #[test]
    fn config_requires_exactly_one_transport() {
        let mut c = cfg();
        assert!(c.validate().is_ok());
        c.url = Some("http://127.0.0.1:1/mcp".into());
        assert!(c.validate().is_err(), "both set must error");
        c.command = None;
        assert!(c.validate().is_ok());
        c.url = None;
        assert!(c.validate().is_err(), "neither set must error");
    }

    // ------------------------------------------------------------- HTTP

    /// Serve one raw HTTP request per connection; `handler` decides the
    /// response from the request body.
    type Handler = std::sync::Arc<
        dyn Fn(String, Vec<u8>) -> std::pin::Pin<Box<dyn Future<Output = Vec<u8>> + Send>>
            + Send
            + Sync,
    >;

    async fn serve_http(listener: tokio::net::TcpListener, handler: Handler) {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let handler = handler.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
                    match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if let Ok(text) = std::str::from_utf8(&buf) {
                                if let Some(head_end) = text.find("\r\n\r\n") {
                                    let cl = text
                                        .lines()
                                        .find_map(|l| {
                                            l.strip_prefix("content-length:")
                                                .or_else(|| l.strip_prefix("Content-Length:"))
                                                .and_then(|v| v.trim().parse::<usize>().ok())
                                        })
                                        .unwrap_or(0);
                                    if buf.len() >= head_end + 4 + cl {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                let text = String::from_utf8_lossy(&buf).into_owned();
                let split = text.find("\r\n\r\n").map(|p| (p, p + 4)).unwrap_or((0, 0));
                let head = text[..split.0].to_string();
                let body = buf[split.1..].to_vec();
                let resp = handler(head, body).await;
                sock.write_all(&resp).await.ok();
                sock.shutdown().await.ok();
            });
        }
    }

    fn http_ok(content_type: &str, extra: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// Minimal streamable-HTTP MCP server: initialize (assigns a
    /// session), notifications accepted, tools/list returns one tool.
    async fn streamable_server(listener: tokio::net::TcpListener) {
        serve_http(
            listener,
            std::sync::Arc::new(|_head, body| {
                Box::pin(async move {
                    let text = String::from_utf8_lossy(&body).into_owned();
                    let msg: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    match msg["method"].as_str() {
                        Some("initialize") => {
                            let result = json!({
                                "jsonrpc": "2.0",
                                "id": msg["id"],
                                "result": {
                                    "protocolVersion": msg["params"]["protocolVersion"],
                                    "capabilities": {"tools": {}},
                                    "serverInfo": {"name": "fake-http", "version": "0"}
                                }
                            });
                            let mut resp = http_ok("application/json", "", &result.to_string());
                            // splice a session header in
                            let resp_str = String::from_utf8(resp).unwrap().replace(
                                "Content-Type:",
                                "Mcp-Session-Id: sess-1\r\nContent-Type:",
                            );
                            resp = resp_str.into_bytes();
                            resp
                        }
                        Some("notifications/initialized") => {
                            b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                .to_vec()
                        }
                        Some("ping") => {
                            let result = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {}});
                            http_ok("application/json", "", &result.to_string())
                        }
                        Some("tools/list") => {
                            let result = json!({
                                "jsonrpc": "2.0",
                                "id": msg["id"],
                                "result": {"tools": [
                                    {"name": "echo", "description": "echo", "inputSchema": {"type": "object"}}
                                ]}
                            });
                            http_ok("application/json", "", &result.to_string())
                        }
                        Some("resources/list") => {
                            let result = json!({
                                "jsonrpc": "2.0",
                                "id": msg["id"],
                                "result": {"resources": [
                                    {"uri": "mem://notes", "name": "notes",
                                     "description": "server notes", "mimeType": "text/plain"}
                                ]}
                            });
                            http_ok("application/json", "", &result.to_string())
                        }
                        Some("resources/read") => {
                            let result = json!({
                                "jsonrpc": "2.0",
                                "id": msg["id"],
                                "result": {"contents": [
                                    {"uri": msg["params"]["uri"], "text": "RESOURCE-BODY"}
                                ]}
                            });
                            http_ok("application/json", "", &result.to_string())
                        }
                        _ => {
                            b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                .to_vec()
                        }
                    }
                })
            }),
        )
        .await;
    }

    #[tokio::test]
    async fn streamable_http_handshake_and_tools() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(streamable_server(listener));
        let cfg = McpServerConfig {
            name: "http".to_string(),
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            url: Some(format!("http://{addr}/mcp")),
            headers: vec![("Authorization".into(), "Bearer t".into())],
        };
        let (mut client, tools) = McpClient::spawn_connect(&cfg).await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "http__echo");
        // session id from the initialize response is echoed on later calls
        assert!(client.alive().await, "ping round-trip must succeed");
    }

    /// Minimal legacy-SSE server: GET /sse emits the endpoint event then
    /// forwards JSON-RPC responses written to the channel; POST
    /// /messages answers 202 and dispatches the response over SSE.
    async fn legacy_sse_server(listener: tokio::net::TcpListener) {
        use tokio::io::AsyncReadExt;
        let (tx, rx) = mpsc::channel::<Vec<u8>>(16);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            let tx = tx.clone();
            let rx = rx.clone();
            tokio::spawn(async move {
                let mut sock = sock;
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
                    match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let req = String::from_utf8_lossy(&buf).into_owned();
                if req.starts_with("GET /sse") {
                    let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                    let _ = sock
                        .write_all(b"event: endpoint\ndata: /messages\n\n")
                        .await;
                    let _ = sock.flush().await;
                    // forward JSON responses over the SSE stream
                    let mut rx = rx.lock().await;
                    while let Some(body) = rx.recv().await {
                        let frame = format!("data: {}\n\n", String::from_utf8_lossy(&body));
                        if sock.write_all(frame.as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = sock.flush().await;
                    }
                } else if req.starts_with("POST /sse") {
                    // a streamable-HTTP probe against a legacy-only server:
                    // answer 405 so the client falls back to this SSE stream
                    let _ = sock
                    .write_all(
                        b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                    let _ = sock.shutdown().await;
                } else if req.starts_with("POST /messages") {
                    let body_start = req.find("\r\n\r\n").map(|p| p + 4).unwrap_or(req.len());
                    let msg: Value =
                        serde_json::from_str(req[body_start..].trim()).unwrap_or(Value::Null);
                    let reply = match msg["method"].as_str() {
                        Some("initialize") => json!({
                            "jsonrpc": "2.0",
                            "id": msg["id"],
                            "result": {
                                "protocolVersion": msg["params"]["protocolVersion"],
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "fake-sse", "version": "0"}
                            }
                        }),
                        Some("tools/list") => json!({
                            "jsonrpc": "2.0",
                            "id": msg["id"],
                            "result": {"tools": [
                                {"name": "ping_tool", "description": "p", "inputSchema": {"type": "object"}}
                            ]}
                        }),
                        _ => json!({"jsonrpc": "2.0", "id": msg["id"], "result": {}}),
                    };
                    let _ = sock
                    .write_all(
                        b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                    let _ = sock.shutdown().await;
                    tx.send(reply.to_string().into_bytes()).await.ok();
                }
            });
        }
    }

    #[tokio::test]
    async fn legacy_sse_fallback_handshake_and_tools() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(legacy_sse_server(listener));
        let cfg = McpServerConfig {
            name: "sse".to_string(),
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            url: Some(format!("http://{addr}/sse")),
            headers: Vec::new(),
        };
        let (_client, tools) = McpClient::spawn_connect(&cfg).await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "sse__ping_tool");
    }

    #[tokio::test]
    async fn watchdog_reconnects_after_server_death() {
        if !python3_available() {
            return;
        }
        let cfg = cfg();
        let (client, tools) = McpClient::spawn_connect(&cfg).await.unwrap();
        assert_eq!(tools.len(), 1);
        let shared = McpShared::new(cfg, client, tools);
        assert!(shared.alive().await);
        let (mtx, mut mrx) = mpsc::channel(8);
        let timing = WatchdogTiming {
            poll: std::time::Duration::from_millis(50),
            backoffs: [
                std::time::Duration::from_millis(50),
                std::time::Duration::from_millis(50),
                std::time::Duration::from_millis(50),
            ],
        };
        tokio::spawn(supervise(shared.clone(), mtx, timing));
        // kill the server; the watchdog must notice and reconnect
        shared.test_kill().await;
        let update = tokio::time::timeout(std::time::Duration::from_secs(10), mrx.recv())
            .await
            .unwrap()
            .expect("watchdog must report");
        match update {
            Maintenance::McpReconnected { server, gained, .. } => {
                assert_eq!(server, "fake");
                assert_eq!(gained, 0, "same tool set after reconnect");
            }
            other => panic!("unexpected maintenance: {other:?}"),
        }
        assert!(shared.alive().await, "reconnected client must serve pings");
    }

    #[tokio::test]
    async fn resources_list_and_read_round_trip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(streamable_server(listener));
        let cfg = McpServerConfig {
            name: "http".to_string(),
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            url: Some(format!("http://{addr}/mcp")),
            headers: Vec::new(),
        };
        let (client, _tools) = McpClient::spawn_connect(&cfg).await.unwrap();
        let shared = McpShared::new(cfg, client, Vec::new());
        let items = shared.list_resources().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].uri, "mem://notes");
        assert_eq!(items[0].mime_type, "text/plain");
        let body = shared.read_resource("mem://notes").await.unwrap();
        assert_eq!(body, "RESOURCE-BODY");
    }

    #[test]
    fn extract_text_joins_parts() {
        let v = json!({"content": [
            {"type":"text","text":"a"},
            {"type":"image","data":"..."},
            {"type":"text","text":"b"},
        ]});
        assert_eq!(extract_text(&v), "a\nb");
    }
}
