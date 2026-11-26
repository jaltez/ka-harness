//! Minimal MCP (Model Context Protocol) client: newline-delimited
//! JSON-RPC 2.0 over a spawned process (stdio), or streamable HTTP with
//! legacy-SSE fallback for HTTP endpoints. Hand-rolled on purpose —
//! the footprint budget has no room for rmcp, and ka only needs
//! initialize / tools-list / tools-call (plus resources and prompts).
//!
//! Lifecycle: [`McpClient::spawn_connect`] connects, handshakes, and
//! lists tools. Failures are per-server and non-fatal: the engine notes
//! them and carries on with the built-in hands.
//!
//! EXT-03: HTTP servers gain auth — a `headers_helper` command, a
//! `bearer_token_env` bearer, or the OAuth 2.0 flow
//! ([`crate::mcp_oauth`]: metadata discovery, DCR/CIMD, PKCE code
//! exchange, cached refresh). A 401 carrying `WWW-Authenticate`
//! triggers the flow once per connect; an expired token refreshes once
//! per request before the request fails.
//!
//! EXT-05: server-initiated traffic is no longer dropped. Requests
//! (`roots/list`, `elicitation/create`, `sampling/createMessage`) and
//! notifications (`notifications/tools/list_changed`,
//! `notifications/resources/updated`) route through
//! [`McpShared`]'s inbox: `pump_inbound` answers `roots/list`
//! directly (cwd) and forwards everything else to the engine via
//! [`Maintenance`]. They are handled between turns — a server that
//! elicits mid-`tools/call` waits for the turn to settle (the engine
//! loop owns asks; see [`crate::engine`]).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc, oneshot};

/// A server-initiated JSON-RPC message (EXT-05). Requests carry the
/// raw id (string or number — echoed verbatim in the response);
/// notifications carry none.
#[derive(Debug, Clone)]
pub enum ServerMessage {
    /// A request the server expects answered (`roots/list`,
    /// `elicitation/create`, `sampling/createMessage`, …).
    Request {
        /// The verbatim JSON-RPC id to echo in the response.
        id: Value,
        /// The method name.
        method: String,
        /// The request params.
        params: Value,
    },
    /// A server-initiated notification (`notifications/*`).
    Notification {
        /// The method name.
        method: String,
        /// The notification params.
        params: Value,
    },
}

/// Auth context for an HTTP transport (EXT-03): the resolved bearer
/// plus what a 401 needs to refresh once before failing.
#[derive(Debug, Clone)]
struct HttpAuth {
    bearer: String,
    refresh: Option<String>,
    token_endpoint: String,
    client_id: String,
    scopes: Vec<String>,
    /// Token-cache key (`mcp_oauth::server_key`); `None` = never
    /// persist refreshed tokens (bearer_token_env path).
    cache_key: Option<String>,
    /// One silent refresh per transport lifetime guard: a fresh
    /// transport retries once, a poisoned token fails loud instead of
    /// looping.
    refreshed_401: bool,
}

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
    /// EXT-04 allowlist of tool names (raw or wire names, matched
    /// against both). Empty = every tool is offered. `disabled_tools`
    /// wins on a name present in both.
    #[serde(default)]
    pub enabled_tools: Vec<String>,
    /// EXT-04 denylist of tool names (raw or wire names). A disabled
    /// tool is filtered from eager registration, lazy listings, and
    /// rejected at call time.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    /// EXT-04 ask policy for this server's tools. Default
    /// [`McpApproval::Ask`].
    #[serde(default)]
    pub approval: Option<McpApproval>,
    /// EXT-04 per-call timeout in seconds for `tools/call` requests
    /// against this server. Unset keeps the protocol default
    /// ([`REQUEST_TIMEOUT`], 30 s).
    #[serde(default)]
    pub tool_timeout_secs: Option<u64>,
    /// EXT-04 result cap in characters. A longer result is cut on the
    /// cap with a truncation note.
    #[serde(default)]
    pub output_limit: Option<usize>,
    /// EXT-03: engage the OAuth 2.0 flow for this HTTP server (401 +
    /// `WWW-Authenticate` also triggers it when unset). With
    /// `bearer_token_env` set, OAuth never runs.
    #[serde(default)]
    pub oauth: Option<bool>,
    /// EXT-03: a pre-registered (CIMD) OAuth client id. Its presence
    /// implies `oauth = true` and skips dynamic client registration.
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    /// EXT-03: scopes to request during authorization.
    #[serde(default)]
    pub oauth_scopes: Vec<String>,
    /// EXT-03: bearer-token override — the access token is read from
    /// this environment variable and NO OAuth flow runs.
    #[serde(default)]
    pub bearer_token_env: Option<String>,
    /// EXT-03b: a shell command whose stdout is a JSON object of extra
    /// HTTP headers, executed before each HTTP connection (output
    /// capped at 8 KiB; failures surface as the connect error).
    #[serde(default)]
    pub headers_helper: Option<String>,
}

impl McpServerConfig {
    /// Exactly one transport must be configured.
    pub fn validate(&self) -> Result<(), String> {
        match (self.command.is_some(), self.url.is_some()) {
            (true, false) | (false, true) => {}
            (true, true) => {
                return Err(format!(
                    "mcp {}: set either command or url, not both",
                    self.name
                ));
            }
            (false, false) => {
                return Err(format!(
                    "mcp {}: one of command or url is required",
                    self.name
                ));
            }
        }
        // EXT-03: the HTTP-auth knobs are meaningless (and silently
        // dead) on a stdio server — reject them instead.
        if self.command.is_some() {
            for (key, set) in [
                ("oauth", self.oauth.is_some()),
                ("oauth_client_id", self.oauth_client_id.is_some()),
                ("oauth_scopes", !self.oauth_scopes.is_empty()),
                ("bearer_token_env", self.bearer_token_env.is_some()),
                ("headers_helper", self.headers_helper.is_some()),
            ] {
                if set {
                    return Err(format!(
                        "mcp {}: {key} applies to url servers only",
                        self.name
                    ));
                }
            }
        }
        if let Some(id) = &self.oauth_client_id {
            if id.trim().is_empty() {
                return Err(format!(
                    "mcp {}: oauth_client_id must not be empty",
                    self.name
                ));
            }
        }
        if let Some(helper) = &self.headers_helper {
            if helper.trim().is_empty() {
                return Err(format!(
                    "mcp {}: headers_helper must not be empty",
                    self.name
                ));
            }
        }
        Ok(())
    }

    /// EXT-03: whether OAuth should engage for this server (config
    /// opt-in, a CIMD client id, or any configured scope).
    pub fn oauth_enabled(&self) -> bool {
        self.bearer_token_env.is_none()
            && (self.oauth == Some(true)
                || self.oauth_client_id.is_some()
                || !self.oauth_scopes.is_empty())
    }

    /// EXT-04 tool filter: `disabled_tools` wins over `enabled_tools`;
    /// an empty allowlist admits everything else. Entries match a
    /// tool's raw name or its wire name.
    pub fn tool_enabled(&self, raw_name: &str, wire_name: &str) -> bool {
        if self
            .disabled_tools
            .iter()
            .any(|t| t == raw_name || t == wire_name)
        {
            return false;
        }
        self.enabled_tools.is_empty()
            || self
                .enabled_tools
                .iter()
                .any(|t| t == raw_name || t == wire_name)
    }

    /// The effective ask policy (default ask).
    pub fn effective_approval(&self) -> McpApproval {
        self.approval.unwrap_or_default()
    }

    /// The effective per-call timeout.
    pub fn effective_tool_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.tool_timeout_secs.unwrap_or(30).max(1))
    }

    /// The effective result cap in characters, if set (0 is treated as
    /// unset).
    pub fn effective_output_limit(&self) -> Option<usize> {
        self.output_limit.filter(|l| *l > 0)
    }
}

/// EXT-04 ask policy for one server's tools (`approval` in `[[mcp]]`).
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum McpApproval {
    /// Normal permission gate (the default).
    #[default]
    Ask,
    /// This server's tools skip the permission ask entirely (deny-class
    /// verdicts and hardstops still apply).
    Auto,
    /// Only tools advertised with `readOnlyHint = false` ask;
    /// read-only tools run without the ask.
    Writes,
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
    /// The tool's `annotations.readOnlyHint` (EXT-04). Unadvertised =
    /// false: under the `writes` approval policy only explicitly
    /// read-only tools skip the ask.
    #[serde(default)]
    pub read_only_hint: bool,
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
    /// Server-initiated requests/notifications (EXT-05), moved into
    /// [`McpShared`]'s inbox when the client is installed.
    noise_rx: Option<mpsc::Receiver<ServerMessage>>,
    /// The most recent HTTP transport error (status +
    /// `WWW-Authenticate`), kept so the connect path can decide on an
    /// OAuth retry (EXT-03). Cleared on every request.
    last_http: Option<HttpError>,
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
    /// EXT-03 auth: bearer + refresh material, `None` for anonymous.
    auth: Option<HttpAuth>,
    /// Server-initiated frames routed off the POST responses (EXT-05).
    noise: Option<mpsc::Sender<ServerMessage>>,
}

/// Transport-level failure. The message carries the HTTP status when
/// one was received (a 404/405 on a streamable POST hints at a
/// legacy-SSE-only server — see [`McpClient::legacy_fallback_possible`]);
/// `www_authenticate` carries the 401 challenge header verbatim (the
/// OAuth trigger, EXT-03).
#[derive(Debug, Clone)]
struct HttpError {
    message: String,
    www_authenticate: Option<String>,
    status: Option<u16>,
}

impl HttpError {
    fn plain(message: String) -> Self {
        Self {
            message,
            www_authenticate: None,
            status: None,
        }
    }
}

/// Classify one decoded JSON-RPC frame into its server-initiated
/// [`ServerMessage`]; `None` = a response frame (routed by id, not
/// method) or junk.
fn classify_frame(frame: Value) -> Option<ServerMessage> {
    let method = frame.get("method")?.as_str()?.to_string();
    let params = frame.get("params").cloned().unwrap_or(Value::Null);
    match frame.get("id").filter(|v| !v.is_null()) {
        Some(id) => Some(ServerMessage::Request {
            id: id.clone(),
            method,
            params,
        }),
        None => Some(ServerMessage::Notification { method, params }),
    }
}

/// Maximum size for one MCP HTTP response body, stdio JSON-RPC line,
/// or SSE frame buffer (16 MiB).
const MCP_BODY_CAP: usize = 16 * 1024 * 1024;

/// Whether two http(s) URLs share the same origin (scheme, lowercased
/// host, and effective port).
fn same_origin(a: &str, b: &str) -> bool {
    let (Ok(ua), Ok(ub)) = (reqwest::Url::parse(a), reqwest::Url::parse(b)) else {
        return false;
    };
    ua.scheme() == ub.scheme()
        && ua.host_str().map(str::to_ascii_lowercase) == ub.host_str().map(str::to_ascii_lowercase)
        && ua.port_or_known_default() == ub.port_or_known_default()
}

impl HttpRpc {
    fn new(url: String, headers: Vec<(String, String)>) -> Self {
        Self {
            url,
            headers,
            session_id: None,
            client: reqwest::Client::new(),
            auth: None,
            noise: None,
        }
    }

    fn with_noise(mut self, noise: mpsc::Sender<ServerMessage>) -> Self {
        self.noise = Some(noise);
        self
    }

    fn with_auth(mut self, auth: HttpAuth) -> Self {
        self.auth = Some(auth);
        self
    }

    fn apply_common(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut req = req
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION);
        for (k, v) in &self.headers {
            if self.auth.is_some() && k.eq_ignore_ascii_case("authorization") {
                continue;
            }
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(auth) = &self.auth {
            req = req.header("Authorization", format!("Bearer {}", auth.bearer));
        }
        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        req
    }

    /// EXT-03: refresh the access token once (401 guard). Updates the
    /// transport's bearer, re-caches, and reports whether the caller
    /// should retry the request.
    async fn refresh_on_401(&mut self) -> Result<bool, String> {
        let Some(auth) = self.auth.as_mut() else {
            return Ok(false);
        };
        if auth.refreshed_401 {
            return Ok(false);
        }
        let Some(refresh) = auth.refresh.clone() else {
            return Ok(false);
        };
        auth.refreshed_401 = true;
        let mut fresh = crate::mcp_oauth::refresh_tokens(
            &self.client,
            &auth.token_endpoint,
            &auth.client_id,
            &refresh,
            &auth.scopes,
        )
        .await?;
        // RFC 6749 §6: preserve the prior refresh_token if the server
        // did not rotate it, and keep client_id so future refreshes
        // continue to work across reconnects.
        if fresh.refresh_token.is_none() {
            fresh.refresh_token = Some(refresh);
        }
        if fresh.client_id.is_none() && !auth.client_id.is_empty() {
            fresh.client_id = Some(auth.client_id.clone());
        }
        auth.bearer = fresh.access_token.clone();
        auth.refresh = fresh.refresh_token.clone();
        if let Some(key) = &auth.cache_key {
            crate::mcp_oauth::store_tokens(key, &fresh);
        }
        Ok(true)
    }

    /// POST one JSON-RPC message. `Ok(None)` = accepted with no body
    /// (notifications / 202). A 401 with refreshable auth retries once
    /// after a refresh grant.
    async fn post(&mut self, msg: &Value) -> Result<Option<Value>, HttpError> {
        match self.post_once(msg).await {
            Err(e) if e.status == Some(401) => match self.refresh_on_401().await {
                Ok(true) => self.post_once(msg).await,
                Ok(false) => Err(e),
                Err(refresh_err) => Err(HttpError {
                    message: format!("{} (token refresh failed: {refresh_err})", e.message),
                    www_authenticate: e.www_authenticate,
                    status: e.status,
                }),
            },
            other => other,
        }
    }

    /// [`Self::post`] without the 401 refresh dance.
    async fn post_once(&mut self, msg: &Value) -> Result<Option<Value>, HttpError> {
        // keep a verbatim copy for id matching when the server streams
        // extra frames (server-initiated requests) beside the response
        let own_id = msg.get("id").cloned();
        let mut resp = self
            .apply_common(self.client.post(&self.url))
            .json(msg)
            .send()
            .await
            .map_err(|e| HttpError::plain(format!("mcp http: {e}")))?;
        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            self.session_id = Some(sid.to_string());
        }
        let status = resp.status();
        let www_authenticate = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if !status.is_success() {
            return Err(HttpError {
                message: format!("mcp http: status {status}"),
                www_authenticate,
                status: Some(status.as_u16()),
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
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| HttpError::plain(format!("mcp http: {e}")))?
        {
            if buf.len().saturating_add(chunk.len()) > MCP_BODY_CAP {
                return Err(HttpError::plain(
                    "mcp http: response exceeded 16 MiB limit".to_string(),
                ));
            }
            buf.extend_from_slice(&chunk);
        }
        let body = String::from_utf8_lossy(&buf).into_owned();
        if ct.contains("event-stream") {
            return Ok(self.consume_sse(&body, own_id));
        }
        let parsed: Value = serde_json::from_str(&body)
            .map_err(|e| HttpError::plain(format!("mcp http: non-JSON body: {e}")))?;
        Ok(self.consume_frame(parsed, own_id))
    }

    /// Route every JSON-RPC frame in an SSE body: our response is
    /// returned (after the WHOLE body is scanned — servers may append
    /// server-initiated requests/notifications behind it), everything
    /// else flows to the inbox.
    fn consume_sse(&mut self, body: &str, own_id: Option<Value>) -> Option<Value> {
        let mut own: Option<Value> = None;
        for frame in body.split("\n\n") {
            for line in frame.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                        if own.is_none() {
                            own = self.consume_frame(v, own_id.clone());
                        } else if let (Some(tx), Some(msg)) =
                            (self.noise.clone(), classify_frame(v))
                        {
                            tx.try_send(msg).ok();
                        }
                    }
                }
            }
        }
        own
    }

    /// Classify one decoded frame: our response (id match + result or
    /// error) is returned; other frames route to the inbox.
    fn consume_frame(&mut self, frame: Value, own_id: Option<Value>) -> Option<Value> {
        if let Some(id) = &own_id {
            if frame.get("id") == Some(id)
                && (frame.get("result").is_some() || frame.get("error").is_some())
            {
                return Some(frame);
            }
        }
        if let (Some(tx), Some(msg)) = (self.noise.clone(), classify_frame(frame)) {
            tx.try_send(msg).ok();
        }
        None
    }
}

impl McpClient {
    /// Connect to the server (spawn or HTTP), run the initialize
    /// handshake, and list tools. One call so callers get a
    /// fully-usable client or an error string.
    pub async fn spawn_connect(cfg: &McpServerConfig) -> Result<(Self, Vec<McpTool>), String> {
        Self::spawn_connect_with(cfg, &crate::mcp_oauth::FlowHooks::default()).await
    }

    /// [`Self::spawn_connect`] with OAuth flow hooks (EXT-03): the
    /// Note sink the authorize URL lands in and the headless/test
    /// allowance. Watchdogs thread the shared handle's hooks through
    /// so reconnects surface the same UX.
    pub async fn spawn_connect_with(
        cfg: &McpServerConfig,
        ux: &crate::mcp_oauth::FlowHooks,
    ) -> Result<(Self, Vec<McpTool>), String> {
        cfg.validate()?;
        match (&cfg.command, &cfg.url) {
            (Some(command), _) => Self::spawn_stdio(cfg, command).await,
            (None, Some(url)) => Self::connect_http(cfg, url, ux).await,
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
            noise_rx: Some(noise_rx),
            last_http: None,
        };
        client.initialize().await?;
        let tools = client.list_tools().await?;
        Ok((client, tools))
    }

    /// HTTP transport: try streamable HTTP first; on a 404/405 from the
    /// POST fall back to a legacy-SSE server (GET endpoint event gives
    /// the messages URL).
    ///
    /// EXT-03 auth order: `headers_helper` headers first, then the
    /// `bearer_token_env` bearer (which disables OAuth entirely), then
    /// OAuth — config opt-in up front, or once on a 401 that carries a
    /// `WWW-Authenticate` challenge. An OAuth connect retries the
    /// handshake with the fresh bearer.
    async fn connect_http(
        cfg: &McpServerConfig,
        url: &str,
        ux: &crate::mcp_oauth::FlowHooks,
    ) -> Result<(Self, Vec<McpTool>), String> {
        let (noise_tx, noise_rx) = mpsc::channel(64);
        let rpc =
            HttpRpc::new(url.to_string(), Self::http_headers(cfg).await?).with_noise(noise_tx);
        let mut client = Self {
            server_name: cfg.name.clone(),
            transport: Transport::StreamableHttp(rpc),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            noise_rx: Some(noise_rx),
            last_http: None,
        };
        match client.initialize().await {
            Ok(()) => {}
            Err(e) if client.legacy_fallback_possible(&e) => {
                let rpc = client.legacy_endpoint_rpc().await?;
                client.transport = Transport::LegacySse(rpc);
                client.initialize().await?;
            }
            Err(_) if Self::oauth_due(cfg, &client.last_http) => {
                // authorize (or refresh from cache), then rebuild the
                // transport with the fresh bearer and retry once
                let http = reqwest::Client::new();
                let (tokens, meta) = crate::mcp_oauth::ensure_tokens(&http, cfg, ux).await?;
                let (noise_tx, noise_rx) = mpsc::channel(64);
                let rpc = HttpRpc::new(url.to_string(), Self::http_headers(cfg).await?)
                    .with_noise(noise_tx)
                    .with_auth(HttpAuth {
                        bearer: tokens.access_token,
                        refresh: tokens.refresh_token,
                        token_endpoint: meta.token_endpoint,
                        client_id: tokens.client_id.unwrap_or_default(),
                        scopes: cfg.oauth_scopes.clone(),
                        cache_key: Some(crate::mcp_oauth::server_key(cfg)),
                        refreshed_401: false,
                    });
                client.transport = Transport::StreamableHttp(rpc);
                client.noise_rx = Some(noise_rx);
                client.last_http = None;
                client.initialize().await?;
            }
            Err(e) => return Err(e),
        }
        let tools = client.list_tools().await?;
        Ok((client, tools))
    }

    /// EXT-03b + EXT-03: the static headers for an HTTP connect —
    /// configured `headers`, then `headers_helper` output (8 KiB cap,
    /// failures inline), then the `bearer_token_env` bearer. Helper
    /// output carrying an `authorization` entry is dropped when ka
    /// adds its own bearer, so a transport never ships two.
    async fn http_headers(cfg: &McpServerConfig) -> Result<Vec<(String, String)>, String> {
        let mut headers = cfg.headers.clone();
        if let Some(helper) = &cfg.headers_helper {
            let extra = tokio::task::spawn_blocking({
                let helper = helper.clone();
                move || crate::mcp_oauth::run_headers_helper(&helper)
            })
            .await
            .map_err(|e| format!("mcp {}: headers_helper join failed: {e}", cfg.name))?
            .map_err(|e| format!("mcp {}: {e}", cfg.name))?;
            headers.extend(extra);
        }
        let mut bearer = None;
        if let Some(env_name) = &cfg.bearer_token_env {
            let token = std::env::var(env_name).map_err(|_| {
                format!(
                    "mcp {}: bearer_token_env {env_name:?} is not set in the environment",
                    cfg.name
                )
            })?;
            if token.trim().is_empty() {
                return Err(format!(
                    "mcp {}: bearer_token_env {env_name:?} is empty",
                    cfg.name
                ));
            }
            bearer = Some(token);
        }
        if bearer.is_some() {
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization"));
        }
        if let Some(token) = bearer {
            headers.push(("Authorization".into(), format!("Bearer {token}")));
        }
        Ok(headers)
    }

    /// Whether the OAuth flow should run for this connect: config
    /// opt-in always; a 401 whose `WWW-Authenticate` names OAuth also
    /// triggers it (never under `bearer_token_env` — that override
    /// wins and OAuth stays off).
    fn oauth_due(cfg: &McpServerConfig, last_http: &Option<HttpError>) -> bool {
        if !cfg.oauth_enabled() {
            return false;
        }
        if cfg.oauth == Some(true) || cfg.oauth_client_id.is_some() {
            return true;
        }
        // scopes-only configs still wait for the server to ask
        last_http.as_ref().is_some_and(|e| {
            e.status == Some(401)
                && e.www_authenticate
                    .as_deref()
                    .is_some_and(|w| w.to_ascii_lowercase().contains("oauth"))
        })
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
        let auth = old.auth.clone();
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
        let post_noise = noise_tx.clone();
        self.noise_rx = Some(noise_rx);
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
        if !same_origin(&base, &post_url) {
            return Err(format!(
                "mcp sse: refusing cross-origin endpoint {post_url:?} (expected origin of {base:?})"
            ));
        }
        Ok(HttpRpc {
            url: post_url,
            headers,
            session_id: None,
            client,
            auth,
            noise: Some(post_noise),
        })
    }

    /// The configured server name.
    pub fn name(&self) -> &str {
        &self.server_name
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: std::time::Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.last_http = None;
        match &mut self.transport {
            // streamable HTTP: the response rides the POST itself
            Transport::StreamableHttp(rpc) => {
                let resp = tokio::time::timeout(timeout, rpc.post(&msg))
                    .await
                    .map_err(|_| format!("{method}: request timed out"))?
                    .map_err(|e| {
                        self.last_http = Some(e.clone());
                        e.message
                    })?
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
                let resp = tokio::time::timeout(timeout, rx)
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

    /// Answer a server-initiated request (EXT-05): the result half of
    /// a JSON-RPC response, id echoed verbatim.
    async fn respond(&mut self, id: &Value, result: Value) -> Result<(), String> {
        self.send_msg(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await
    }

    /// Answer a server-initiated request with a JSON-RPC error object
    /// (unsupported method, refused elicitation, sampling rejection).
    async fn respond_error(&mut self, id: &Value, code: i64, message: &str) -> Result<(), String> {
        self.send_msg(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }))
        .await
    }

    /// The transport's live bearer token (oauth tests poison it to
    /// exercise the 401 → refresh → retry path).
    #[cfg(test)]
    pub fn test_bearer_mut(&mut self) -> Option<&mut String> {
        match &mut self.transport {
            Transport::StreamableHttp(rpc) | Transport::LegacySse(rpc) => {
                rpc.auth.as_mut().map(|a| &mut a.bearer)
            }
            Transport::Stdio { .. } => None,
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
            REQUEST_TIMEOUT,
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
        let result = self
            .request("tools/list", json!({}), REQUEST_TIMEOUT)
            .await?;
        parse_tools(&result, &self.server_name)
    }

    /// Invoke a tool; returns the concatenated text content.
    pub async fn call_tool(&mut self, tool: &str, args: Value) -> Result<String, String> {
        self.call_tool_timeout(tool, args, REQUEST_TIMEOUT).await
    }

    /// [`Self::call_tool`] with an explicit per-call timeout (EXT-04
    /// `[[mcp]] tool_timeout_secs`).
    pub async fn call_tool_timeout(
        &mut self,
        tool: &str,
        args: Value,
        timeout: std::time::Duration,
    ) -> Result<String, String> {
        let result = self
            .request(
                "tools/call",
                json!({"name": tool, "arguments": args.as_object().cloned().unwrap_or_default()}),
                timeout,
            )
            .await?;
        if result["isError"].as_bool().unwrap_or(false) {
            return Err(extract_text(&result));
        }
        Ok(extract_text(&result))
    }

    /// List resources advertised by the server.
    pub async fn list_resources(&mut self) -> Result<Vec<McpResource>, String> {
        let result = self
            .request("resources/list", json!({}), REQUEST_TIMEOUT)
            .await?;
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
        let result = self
            .request("resources/read", json!({"uri": uri}), REQUEST_TIMEOUT)
            .await?;
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
        let result = self
            .request("prompts/list", json!({}), REQUEST_TIMEOUT)
            .await?;
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
                REQUEST_TIMEOUT,
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
            _ => self
                .request("ping", json!({}), REQUEST_TIMEOUT)
                .await
                .is_ok(),
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
    /// EXT-05: a server-initiated JSON-RPC request reached the inbox
    /// (`elicitation/create`, `sampling/createMessage`, anything
    /// unknown). `roots/list` never leaves mcp — the pump answers it.
    McpServerRequest {
        server: String,
        id: Value,
        method: String,
        params: Value,
    },
    /// EXT-05: a server-initiated notification reached the inbox
    /// (`notifications/tools/list_changed`,
    /// `notifications/resources/updated`, anything unknown).
    McpServerNotification {
        server: String,
        method: String,
        params: Value,
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
    /// Server-initiated traffic (EXT-05): each installed client
    /// contributes its reader; [`pump_inbound`] takes and drains it.
    inbox: Arc<Mutex<Option<mpsc::Receiver<ServerMessage>>>>,
    /// OAuth flow hooks (EXT-03) threaded to every (re)connect.
    auth_hooks: crate::mcp_oauth::FlowHooks,
}

impl McpShared {
    /// Wrap a freshly connected client and its tool list. The client's
    /// server-initiated traffic moves into the inbox for the pump.
    pub fn new(cfg: McpServerConfig, mut client: McpClient, tools: Vec<McpTool>) -> Self {
        let inbox = client.noise_rx.take();
        Self {
            cfg,
            cell: Arc::new(Mutex::new(Some(client))),
            tools: Arc::new(parking_lot::Mutex::new(tools)),
            inbox: Arc::new(Mutex::new(inbox)),
            auth_hooks: crate::mcp_oauth::FlowHooks::default(),
        }
    }

    /// Builder: thread OAuth flow hooks (the engine's Note sink) into
    /// every subsequent connect, including watchdog reconnects.
    pub fn with_auth_hooks(mut self, hooks: crate::mcp_oauth::FlowHooks) -> Self {
        self.auth_hooks = hooks;
        self
    }

    /// The OAuth hooks every connect for this server carries.
    pub fn auth_hooks(&self) -> &crate::mcp_oauth::FlowHooks {
        &self.auth_hooks
    }

    /// The configured server name.
    pub fn name(&self) -> &str {
        &self.cfg.name
    }

    /// The server's configured policy ([`McpServerConfig`]).
    pub fn cfg(&self) -> &McpServerConfig {
        &self.cfg
    }

    /// The tools the model may see: the live tool list with the EXT-04
    /// `enabled_tools`/`disabled_tools` filters applied (`disabled`
    /// wins). Empty while disconnected.
    pub fn model_tools(&self) -> Vec<McpTool> {
        self.tools
            .lock()
            .clone()
            .into_iter()
            .filter(|t| self.cfg.tool_enabled(&t.raw_name, &t.name))
            .collect()
    }

    /// Find a policy-visible tool by raw or wire name (EXT-04: a
    /// filtered-out tool resolves to `None`).
    pub fn find_model_tool(&self, name: &str) -> Option<McpTool> {
        self.model_tools()
            .into_iter()
            .find(|t| t.raw_name == name || t.name == name)
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
            inbox: Arc::new(Mutex::new(None)),
            auth_hooks: crate::mcp_oauth::FlowHooks::default(),
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

    /// [`Self::call_tool`] through the EXT-04 policy: the configured
    /// `tool_timeout_secs` bounds the call and `output_limit` caps the
    /// result (truncation note appended). One entry point for both the
    /// eager per-tool hands and the lazy `mcp_call` path.
    pub async fn call_model_tool(&self, tool: &str, args: Value) -> Result<String, String> {
        let timeout = self.cfg.effective_tool_timeout();
        let mut guard = self.cell.lock().await;
        let text = match guard.as_mut() {
            Some(c) => match tokio::time::timeout(timeout, c.call_tool(tool, args)).await {
                Ok(r) => r?,
                Err(_) => {
                    return Err(format!(
                        "mcp {}: tool call timed out after {}s",
                        self.cfg.name,
                        timeout.as_secs()
                    ));
                }
            },
            None => return Err(format!("mcp {}: disconnected", self.cfg.name)),
        };
        Ok(match self.cfg.effective_output_limit() {
            Some(limit) if text.chars().count() > limit => {
                let cut: String = text.chars().take(limit).collect();
                format!("{cut}\n[mcp output truncated at {limit} chars]")
            }
            _ => text,
        })
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
        let result = c.request("tools/list", json!({}), REQUEST_TIMEOUT).await?;
        parse_tools(&result, &self.cfg.name)
    }

    async fn install(&self, mut client: McpClient, tools: Vec<McpTool>) {
        // hand the fresh client's server-initiated traffic to the pump:
        // a previous receiver simply closes when its client drops
        let inbox = client.noise_rx.take();
        *self.cell.lock().await = Some(client);
        *self.tools.lock() = tools;
        *self.inbox.lock().await = inbox;
    }

    async fn mark_down(&self) {
        self.cell.lock().await.take();
    }

    fn tool_count(&self) -> usize {
        self.tools.lock().len()
    }

    /// Take the current inbox reader, if one is installed. The pump
    /// holds it until the client is swapped (the old receiver closes),
    /// then calls again for the new one.
    pub async fn take_inbox(&self) -> Option<mpsc::Receiver<ServerMessage>> {
        self.inbox.lock().await.take()
    }

    /// Answer a server-initiated request through the current client
    /// (EXT-05 elicitation/sampling/roots). Fails when disconnected —
    /// the request is moot then anyway.
    pub async fn respond(&self, id: &Value, result: Value) -> Result<(), String> {
        let mut guard = self.cell.lock().await;
        match guard.as_mut() {
            Some(c) => c.respond(id, result).await,
            None => Err(format!("mcp {}: disconnected", self.cfg.name)),
        }
    }

    /// [`Self::respond`] with a JSON-RPC error object.
    pub async fn respond_error(&self, id: &Value, code: i64, message: &str) -> Result<(), String> {
        let mut guard = self.cell.lock().await;
        match guard.as_mut() {
            Some(c) => c.respond_error(id, code, message).await,
            None => Err(format!("mcp {}: disconnected", self.cfg.name)),
        }
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
            if let Ok((client, tools)) =
                McpClient::spawn_connect_with(&shared.cfg, shared.auth_hooks()).await
            {
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
    if let Ok((client, tools)) = McpClient::spawn_connect_with(&cfg, shared.auth_hooks()).await {
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

/// EXT-05: drain one server's inbox for the engine. `roots/list` is
/// answered right here (cwd-scoped, no ask machinery needed);
/// everything else becomes a [`Maintenance`] message. Reconnects swap
/// the inbox reader — this loop re-takes it whenever the old one
/// closes. While disconnected it parks on a short poll (the watchdog
/// cadence dwarfs it).
pub async fn pump_inbound(
    shared: McpShared,
    cwd: std::path::PathBuf,
    maintenance: mpsc::Sender<Maintenance>,
) {
    loop {
        let Some(mut rx) = shared.take_inbox().await else {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            continue;
        };
        while let Some(msg) = rx.recv().await {
            match msg {
                ServerMessage::Request { id, method, params } => {
                    if method == "roots/list" {
                        let name = cwd
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| cwd.to_string_lossy().into_owned());
                        let uri = format!("file://{}", cwd.display());
                        let _ = shared
                            .respond(&id, json!({"roots": [{"uri": uri, "name": name}]}))
                            .await;
                        continue;
                    }
                    let _ = maintenance
                        .send(Maintenance::McpServerRequest {
                            server: shared.name().to_string(),
                            id,
                            method,
                            params,
                        })
                        .await;
                }
                ServerMessage::Notification { method, params } => {
                    let _ = maintenance
                        .send(Maintenance::McpServerNotification {
                            server: shared.name().to_string(),
                            method,
                            params,
                        })
                        .await;
                }
            }
        }
    }
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
            let read_only_hint = t
                .get("annotations")
                .and_then(|a| a.get("readOnlyHint"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(McpTool {
                name: wire_name(server_name, &raw_name),
                raw_name,
                description,
                schema,
                read_only_hint,
            })
        })
        .collect())
}
/// Collapse every character outside `[a-zA-Z0-9_-]` to `_`. Providers
/// reject function names off that pattern (`tools[n].function.name`),
/// so a dotted `server.tool` namespace never reaches the wire.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The name the model sees: `{server}__{raw}`.
pub(crate) fn wire_name(server: &str, raw: &str) -> String {
    format!("{}__{}", clean(server), clean(raw))
}

/// The prefix every wire name from `server` shares. Feature toggles
/// hide a server's eager hands through it — same cleaning as
/// [`wire_name`], so an off-pattern server name still matches.
pub(crate) fn wire_prefix(server: &str) -> String {
    format!("{}__", clean(server))
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

/// Read one newline-delimited line from `reader`, capping buffered
/// memory at `cap` bytes. An oversized line is drained and reported as
/// [`CappedLine::Oversized`] so the caller can fail the waiter instead
/// of letting it time out.
enum CappedLine {
    Line(String),
    Oversized,
}

/// Fail every in-flight request: the response that would have answered
/// one of them was dropped, so waiting longer only burns the timeout.
async fn fail_pending(pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>, message: &str) {
    let err = json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": {"code": -32603, "message": message},
    });
    let mut map = pending.lock().await;
    for (_, tx) in map.drain() {
        let _ = tx.send(err.clone());
    }
}

async fn read_capped_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<Option<CappedLine>> {
    use tokio::io::AsyncBufReadExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut oversized = false;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            if buf.is_empty() && !oversized {
                return Ok(None);
            }
            break;
        }
        if let Some(nl) = chunk.iter().position(|&b| b == b'\n') {
            if !oversized && buf.len().saturating_add(nl) <= cap {
                buf.extend_from_slice(&chunk[..nl]);
            } else {
                oversized = true;
            }
            reader.consume(nl + 1);
            break;
        } else {
            let len = chunk.len();
            if !oversized && buf.len().saturating_add(len) <= cap {
                buf.extend_from_slice(chunk);
            } else {
                oversized = true;
                buf.clear();
            }
            reader.consume(len);
        }
    }
    if oversized {
        return Ok(Some(CappedLine::Oversized));
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    Ok(Some(CappedLine::Line(
        String::from_utf8_lossy(&buf).into_owned(),
    )))
}

/// Reader task: one JSON-RPC message per line, responses routed by id.
async fn read_loop(
    stdout: ChildStdout,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    noise: mpsc::Sender<ServerMessage>,
) {
    let mut reader = BufReader::new(stdout);
    while let Ok(Some(item)) = read_capped_line(&mut reader, MCP_BODY_CAP).await {
        let line = match item {
            CappedLine::Line(line) => line,
            CappedLine::Oversized => {
                fail_pending(&pending, "mcp: response line exceeded the size limit").await;
                continue;
            }
        };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        route_message(msg, &pending, &noise).await;
    }
}

/// Route one decoded JSON-RPC message: requests/notifications by
/// method (server-initiated, EXT-05), responses by id. The method
/// check runs first — a server-initiated request also carries a
/// numeric id, and must never land in the pending-response map.
async fn route_message(
    msg: Value,
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    noise: &mpsc::Sender<ServerMessage>,
) {
    if msg.get("method").is_some() {
        if let Some(message) = classify_frame(msg) {
            // Non-blocking send: a server notification flood must never
            // stall the reader loop and deadlock pending responses.
            let _ = noise.try_send(message);
        }
    } else if let Some(id) = msg["id"].as_u64() {
        if let Some(tx) = pending.lock().await.remove(&id) {
            tx.send(msg).ok();
        }
    }
}

/// Legacy-SSE reader task: consume the long-lived GET stream, capture
/// the `endpoint` event, route subsequent data frames like stdio lines.
async fn sse_read_loop(
    mut resp: reqwest::Response,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    noise: mpsc::Sender<ServerMessage>,
    endpoint_tx: Option<oneshot::Sender<String>>,
) {
    let mut buf = String::new();
    let mut endpoint_tx = endpoint_tx;
    loop {
        match resp.chunk().await {
            Ok(Some(bytes)) => {
                if buf.len().saturating_add(bytes.len()) > MCP_BODY_CAP {
                    buf.clear();
                    fail_pending(&pending, "mcp: SSE frame exceeded the size limit").await;
                    continue;
                }
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
            read_only: self.tool.read_only_hint,
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
            // EXT-04: the per-server timeout + output cap ride this path
            match shared.call_model_tool(&tool, args).await {
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
            listing.push_str(&format!("{} ({} tools), ", s.name(), s.model_tools().len()));
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
                for t in shared.model_tools() {
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
            // EXT-04: a policy-filtered tool refuses here — it never
            // reaches the server
            let Some(target) = shared.find_model_tool(tool) else {
                return crate::hands::ToolOutput::err(format!(
                    "mcp_call: tool {tool:?} is not available on server {server:?} \
                     (filtered by the [[mcp]] enabled_tools/disabled_tools policy, \
                     unknown, or the server is disconnected)"
                ));
            };
            let call_args = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
            // EXT-04: the per-server timeout + output cap ride this path
            match shared.call_model_tool(&target.raw_name, call_args).await {
                Ok(text) if text.trim().is_empty() => {
                    crate::hands::ToolOutput::ok("(empty result)".to_string())
                }
                Ok(text) => crate::hands::ToolOutput::ok(text),
                Err(e) => crate::hands::ToolOutput::err(e),
            }
        })
    }
}

/// EXT-05 elicitation, simplified: what the user is asked and how the
/// chosen option maps back into the MCP result. One question per
/// request — the first schema property drives the options.
#[derive(Debug, Clone, PartialEq)]
pub struct ElicitAsk {
    /// The question text (`message` + property context).
    pub question: String,
    /// The selectable options (index-referenced by the surface).
    pub options: Vec<String>,
    /// The schema property the answer fills (`None` = plain
    /// accept/decline on the message).
    pub prop: Option<String>,
    /// The enum values when the property is a choice.
    pub enum_values: Vec<String>,
    /// `Some(true)`/`Some(false)` when the property is boolean.
    pub boolean: Option<bool>,
}

/// Build the ask for an `elicitation/create` request. The schema is
/// simplified: the FIRST property wins — an `enum` string renders its
/// values as options, a boolean renders true/false, a free-form
/// string renders accept-empty/decline; with no properties the ask is
/// a plain accept/decline on the message.
pub fn elicitation_ask(params: &Value) -> ElicitAsk {
    const MSG_CAP: usize = 2048;
    const OPTIONS_CAP: usize = 32;
    let message: String = params["message"]
        .as_str()
        .unwrap_or("(the server requests input)")
        .chars()
        .take(MSG_CAP)
        .collect();
    let props = params["requestedSchema"]["properties"].as_object();
    let Some((prop, schema)) = props.and_then(|p| p.iter().next()) else {
        return ElicitAsk {
            question: message,
            options: vec!["accept".into(), "decline".into()],
            prop: None,
            enum_values: Vec::new(),
            boolean: None,
        };
    };
    let question = format!("{message} (requested: {prop})");
    let enum_values: Vec<String> = schema["enum"]
        .as_array()
        .map(|a| {
            a.iter()
                .take(OPTIONS_CAP.saturating_sub(1))
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    if !enum_values.is_empty() {
        let mut options = enum_values.clone();
        options.push("decline".into());
        return ElicitAsk {
            question,
            options,
            prop: Some(prop.clone()),
            enum_values,
            boolean: None,
        };
    }
    if schema["type"].as_str() == Some("boolean") {
        return ElicitAsk {
            question,
            options: vec!["true".into(), "false".into(), "decline".into()],
            prop: Some(prop.clone()),
            enum_values: Vec::new(),
            boolean: Some(true),
        };
    }
    ElicitAsk {
        question,
        options: vec!["accept (empty value)".into(), "decline".into()],
        prop: Some(prop.clone()),
        enum_values: Vec::new(),
        boolean: None,
    }
}

/// Map the ask answer back into the MCP elicitation result:
/// `{"action": "accept"|"decline"|"cancel", "content"?: {…}}`.
/// `None` (surface abort) maps to `cancel`.
pub fn elicitation_result(choice: Option<usize>, ask: &ElicitAsk) -> Value {
    let Some(choice) = choice else {
        return json!({"action": "cancel"});
    };
    let decline = json!({"action": "decline"});
    let Some(prop) = &ask.prop else {
        return match choice {
            0 => json!({"action": "accept"}),
            _ => decline,
        };
    };
    let content = |v: Value| {
        let mut map = serde_json::Map::new();
        map.insert(prop.clone(), v);
        json!({"action": "accept", "content": map})
    };
    if !ask.enum_values.is_empty() {
        return match ask.enum_values.get(choice) {
            Some(v) => content(Value::String(v.clone())),
            _ => decline,
        };
    }
    if ask.boolean == Some(true) {
        return match choice {
            0 => content(Value::Bool(true)),
            1 => content(Value::Bool(false)),
            _ => decline,
        };
    }
    match choice {
        0 => content(Value::String(String::new())),
        _ => decline,
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
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            approval: None,
            tool_timeout_secs: None,
            output_limit: None,
            oauth: None,
            oauth_client_id: None,
            oauth_scopes: Vec::new(),
            bearer_token_env: None,
            headers_helper: None,
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
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            approval: None,
            tool_timeout_secs: None,
            output_limit: None,
            oauth: None,
            oauth_client_id: None,
            oauth_scopes: Vec::new(),
            bearer_token_env: None,
            headers_helper: None,
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
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            approval: None,
            tool_timeout_secs: None,
            output_limit: None,
            oauth: None,
            oauth_client_id: None,
            oauth_scopes: Vec::new(),
            bearer_token_env: None,
            headers_helper: None,
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
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            approval: None,
            tool_timeout_secs: None,
            output_limit: None,
            oauth: None,
            oauth_client_id: None,
            oauth_scopes: Vec::new(),
            bearer_token_env: None,
            headers_helper: None,
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

    // ------------------------------------------- EXT-05 server-initiated

    /// EXT-05 fixture: streamable HTTP server whose tools/list response
    /// is an SSE body carrying the tools frame PLUS one injected
    /// server-initiated message (request or notification). Every other
    /// POST is captured verbatim and answered 202.
    struct Ext05State {
        tool_count: std::sync::atomic::AtomicUsize,
        /// (method, params) injected after the next tools/list.
        inject: parking_lot::Mutex<Option<(String, Value)>>,
        /// Raw POSTed bodies (our JSON-RPC responses land here).
        posted: parking_lot::Mutex<Vec<Value>>,
    }

    async fn ext05_server(listener: tokio::net::TcpListener, state: Arc<Ext05State>) {
        serve_http(
            listener,
            Arc::new(move |_head, body| {
                let state = state.clone();
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
                                    "capabilities": {"tools": {}, "elicitation": {}, "sampling": {}, "roots": {"listChanged": true}},
                                    "serverInfo": {"name": "ext05", "version": "0"}
                                }
                            });
                            let resp =
                                http_ok("application/json", "", &result.to_string());
                            String::from_utf8(resp)
                                .unwrap()
                                .replace("Content-Type:", "Mcp-Session-Id: s1\r\nContent-Type:")
                                .into_bytes()
                        }
                        Some("notifications/initialized") => {
                            b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                .to_vec()
                        }
                        Some("tools/list") => {
                            let tools: Vec<Value> =
                                (0..state.tool_count.load(std::sync::atomic::Ordering::Relaxed))
                                .map(|i| {
                                    json!({"name": format!("tool{i}"), "description": "d",
                                           "inputSchema": {"type": "object"}})
                                })
                                .collect();
                            let mut body = format!(
                                "data: {}\n\n",
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": msg["id"],
                                    "result": {"tools": tools}
                                })
                            );
                            if let Some((method, params)) = state.inject.lock().take() {
                                // notifications carry no id; requests
                                // do (string ids echo verbatim)
                                let mut frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
                                if !method.starts_with("notifications/") {
                                    frame["id"] = json!("srv-1");
                                }
                                body.push_str(&format!("data: {frame}\n\n"));
                            }
                            http_ok("text/event-stream", "", &body)
                        }
                        Some("ping") => {
                            let result =
                                json!({"jsonrpc": "2.0", "id": msg["id"], "result": {}});
                            http_ok("application/json", "", &result.to_string())
                        }
                        _ => {
                            state.posted.lock().push(msg);
                            b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                .to_vec()
                        }
                    }
                })
            }),
        )
        .await;
    }

    async fn ext05_setup(
        state: Arc<Ext05State>,
    ) -> (
        McpShared,
        McpServerConfig,
        mpsc::Sender<Maintenance>,
        mpsc::Receiver<Maintenance>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(ext05_server(listener, state.clone()));
        let cfg = McpServerConfig {
            name: "ext05".into(),
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            url: Some(format!("http://{addr}/mcp")),
            headers: Vec::new(),
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            approval: None,
            tool_timeout_secs: None,
            output_limit: None,
            oauth: None,
            oauth_client_id: None,
            oauth_scopes: Vec::new(),
            bearer_token_env: None,
            headers_helper: None,
        };
        let (client, tools) = McpClient::spawn_connect(&cfg).await.unwrap();
        let shared = McpShared::new(cfg.clone(), client, tools);
        let (tx, rx) = mpsc::channel(16);
        (shared, cfg, tx, rx)
    }

    async fn next_maintenance(rx: &mut mpsc::Receiver<Maintenance>) -> Maintenance {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("no server-initiated message arrived")
            .expect("maintenance channel closed")
    }

    /// Wait until the fixture captured a POST matching `pred`.
    async fn wait_posted(state: &Arc<Ext05State>, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            {
                let posted = state.posted.lock();
                if let Some(v) = posted.iter().find(|v| pred(v)) {
                    return v.clone();
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the client never answered the server request"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Elicitation roundtrip: the server's `elicitation/create` request
    /// surfaces as a Maintenance message, the simplified schema maps to
    /// ask options, and the answer routes back as the JSON-RPC result.
    #[tokio::test]
    async fn elicitation_roundtrips_through_the_pump() {
        let state = Arc::new(Ext05State {
            tool_count: std::sync::atomic::AtomicUsize::new(1),
            inject: parking_lot::Mutex::new(Some((
                "elicitation/create".into(),
                json!({
                    "message": "Approve the deployment?",
                    "requestedSchema": {"type": "object", "properties": {
                        "confirm": {"type": "boolean", "description": "ok?"}
                    }}
                }),
            ))),
            posted: parking_lot::Mutex::new(Vec::new()),
        });
        let (shared, _cfg, tx, mut rx) = ext05_setup(state.clone()).await;
        tokio::spawn(crate::mcp::pump_inbound(
            shared.clone(),
            std::env::temp_dir(),
            tx,
        ));
        match next_maintenance(&mut rx).await {
            Maintenance::McpServerRequest {
                server,
                id,
                method,
                params,
            } => {
                assert_eq!(server, "ext05");
                assert_eq!(method, "elicitation/create");
                let ask = elicitation_ask(&params);
                assert_eq!(ask.options, ["true", "false", "decline"]);
                // the user picks the first option (true)
                let result = elicitation_result(Some(0), &ask);
                shared.respond(&id, result).await.unwrap();
            }
            other => panic!("expected the elicitation request, got {other:?}"),
        }
        // the client's answer reached the server as a JSON-RPC result
        let answer = wait_posted(&state, |v| {
            v.get("id").and_then(Value::as_str) == Some("srv-1")
        })
        .await;
        assert_eq!(
            answer["result"],
            json!({"action": "accept", "content": {"confirm": true}})
        );
    }

    /// `roots/list` is answered inside the pump with the cwd — the
    /// engine never sees it.
    #[tokio::test]
    async fn roots_list_answers_with_the_cwd() {
        let state = Arc::new(Ext05State {
            tool_count: std::sync::atomic::AtomicUsize::new(1),
            inject: parking_lot::Mutex::new(Some(("roots/list".into(), json!({})))),
            posted: parking_lot::Mutex::new(Vec::new()),
        });
        let (shared, _cfg, tx, mut rx) = ext05_setup(state.clone()).await;
        let cwd = std::env::temp_dir();
        tokio::spawn(crate::mcp::pump_inbound(shared.clone(), cwd.clone(), tx));
        // nothing is forwarded for roots — give the pump a beat
        let forwarded =
            tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv()).await;
        assert!(
            forwarded.is_err(),
            "roots/list must be answered inline, not forwarded"
        );
        let expected_name = cwd
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| cwd.to_string_lossy().into_owned());
        let answer = wait_posted(&state, |v| {
            v.get("id").and_then(Value::as_str) == Some("srv-1")
        })
        .await;
        assert_eq!(
            answer["result"],
            json!({"roots": [{"uri": format!("file://{}", cwd.display()), "name": expected_name}]})
        );
    }

    /// `notifications/tools/list_changed` arrives as a Maintenance
    /// notification and the same refresh path the explicit refresh
    /// uses swaps the (grown) tool list on the shared handle.
    #[tokio::test]
    async fn tools_list_changed_notification_refreshes_the_catalog() {
        let state = Arc::new(Ext05State {
            tool_count: std::sync::atomic::AtomicUsize::new(1),
            inject: parking_lot::Mutex::new(Some((
                "notifications/tools/list_changed".into(),
                json!({}),
            ))),
            posted: parking_lot::Mutex::new(Vec::new()),
        });
        let (shared, _cfg, tx, mut rx) = ext05_setup(state.clone()).await;
        tokio::spawn(crate::mcp::pump_inbound(
            shared.clone(),
            std::env::temp_dir(),
            tx,
        ));
        match next_maintenance(&mut rx).await {
            Maintenance::McpServerNotification {
                server,
                method,
                params: _,
            } => {
                assert_eq!(server, "ext05");
                assert_eq!(method, "notifications/tools/list_changed");
            }
            other => panic!("expected the notification, got {other:?}"),
        }
        // the engine arm: the server grew a second tool since connect
        state
            .tool_count
            .store(2, std::sync::atomic::Ordering::Relaxed);
        let (tools, delta) = shared.refresh_and_install().await.unwrap();
        assert_eq!(delta, 1);
        assert_eq!(tools.len(), 2);
        assert_eq!(shared.tools().len(), 2, "the catalog swapped in place");
    }

    /// `sampling/createMessage` forwards to the engine (the voice
    /// completes it there — see voice's mcp_sampling tests); unknown
    /// server requests would surface the same way and be answered
    /// "method not supported" by the engine arm.
    #[tokio::test]
    async fn sampling_request_forwards_to_the_engine() {
        let state = Arc::new(Ext05State {
            tool_count: std::sync::atomic::AtomicUsize::new(1),
            inject: parking_lot::Mutex::new(Some((
                "sampling/createMessage".into(),
                json!({
                    "messages": [{"role": "user", "content": {"type": "text", "text": "summarize"}}],
                    "maxTokens": 64
                }),
            ))),
            posted: parking_lot::Mutex::new(Vec::new()),
        });
        let (shared, _cfg, tx, mut rx) = ext05_setup(state.clone()).await;
        tokio::spawn(crate::mcp::pump_inbound(
            shared.clone(),
            std::env::temp_dir(),
            tx,
        ));
        match next_maintenance(&mut rx).await {
            Maintenance::McpServerRequest {
                server,
                id,
                method,
                params,
            } => {
                assert_eq!(server, "ext05");
                assert_eq!(method, "sampling/createMessage");
                assert_eq!(params["maxTokens"], 64);
                assert_eq!(id, json!("srv-1"), "server ids echo verbatim");
                // the engine would complete via the default role; the
                // mcp-layer contract is the verbatim-id response path
                shared
                    .respond(
                        &id,
                        json!({"role": "assistant", "content": {"type": "text", "text": "ok"}}),
                    )
                    .await
                    .unwrap();
            }
            other => panic!("expected the sampling request, got {other:?}"),
        }
        let answer = wait_posted(&state, |v| {
            v.get("id").and_then(Value::as_str) == Some("srv-1")
        })
        .await;
        assert_eq!(answer["result"]["content"]["text"], "ok");
    }

    /// Elicitation ask/result mapping: enum → choices, boolean →
    /// true/false, free string → accept-empty/decline, no props →
    /// accept/decline; abort maps to cancel.
    #[test]
    fn elicitation_mappings_cover_schema_kinds() {
        // boolean property
        let ask = elicitation_ask(&json!({
            "message": "Proceed?",
            "requestedSchema": {"type": "object", "properties": {"go": {"type": "boolean"}}}
        }));
        assert_eq!(ask.question, "Proceed? (requested: go)");
        assert_eq!(ask.options, ["true", "false", "decline"]);
        assert_eq!(
            elicitation_result(Some(1), &ask),
            json!({"action": "accept", "content": {"go": false}})
        );
        assert_eq!(
            elicitation_result(Some(2), &ask),
            json!({"action": "decline"})
        );
        assert_eq!(elicitation_result(None, &ask), json!({"action": "cancel"}));
        // enum choice
        let ask = elicitation_ask(&json!({
            "message": "Pick",
            "requestedSchema": {"type": "object", "properties": {
                "env": {"type": "string", "enum": ["dev", "stage", "prod"]}
            }}
        }));
        assert_eq!(ask.options, ["dev", "stage", "prod", "decline"]);
        assert_eq!(
            elicitation_result(Some(2), &ask),
            json!({"action": "accept", "content": {"env": "prod"}})
        );
        // free-form string
        let ask = elicitation_ask(&json!({
            "message": "Name it",
            "requestedSchema": {"type": "object", "properties": {"name": {"type": "string"}}}
        }));
        assert_eq!(ask.options, ["accept (empty value)", "decline"]);
        assert_eq!(
            elicitation_result(Some(0), &ask),
            json!({"action": "accept", "content": {"name": ""}})
        );
        // no properties: plain accept/decline on the message
        let ask = elicitation_ask(&json!({"message": "Allow?"}));
        assert_eq!(ask.prop, None);
        assert_eq!(
            elicitation_result(Some(0), &ask),
            json!({"action": "accept"})
        );
    }
}
