//! ka wire protocol: the `Command`/`Event` contract between surfaces and the
//! engine. Both enums are NDJSON-serializable — the headless surface literally
//! prints [`Event`] lines to stdout, and any future server reuses the same
//! types. This crate deliberately has no runtime dependencies beyond serde.

use serde::{Deserialize, Serialize};

/// Opaque strand (session file) identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StrandId(pub String);

/// Opaque record identifier within a strand.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecordId(pub String);

/// Opaque ask (interactive question) identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AskId(pub String);

/// Reasoning-effort level for the active model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    /// No reasoning.
    Off,
    /// Low effort.
    Low,
    /// Medium effort.
    Medium,
    /// High effort.
    High,
    /// Maximum effort.
    Max,
}

/// Permission mode for the engine. The names read as permission
/// language — ask before acting, allow everything — and each variant
/// keeps its pre-v0.5 spelling as a deserialization alias so existing
/// configs and strand records keep parsing.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Confirm exec-tier actions and non-allowlisted writes.
    #[default]
    #[serde(alias = "guarded")]
    Ask,
    /// Auto-apply file edits; commands still confirm.
    #[serde(alias = "accept-edits", alias = "accept edits")]
    AcceptEdits,
    /// Auto-approve everything except hardstops.
    #[serde(alias = "free")]
    Allow,
    /// Research mode: read-only except the plans directory.
    #[serde(alias = "plan")]
    Readonly,
}

/// A feature-toggle selector — the one grammar shared by the
/// `--disable`/`--enable` CLI flags, the `[features]` config table, the
/// `/features` command, and the strand's change records. Bare words flip
/// whole tiers; prefixed forms target one item (`mcp:<server>`,
/// `skill:<name>`, `tool:<hand>`, `agent:<name>`). Serializes as the
/// plain string form on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FeatureSpec {
    /// Subagents: the `delegate` + `tasks` hands.
    Agents,
    /// Skills: the skills block of the system prompt.
    Skills,
    /// Every MCP server (tools + prompts).
    Mcp,
    /// Config `[[hooks]]` + `.ka/hooks` file hooks.
    Hooks,
    /// The `web_search` + `web_fetch` hands.
    Web,
    /// The LSP hands tier.
    Lsp,
    /// The debug (DAP) hand.
    Debug,
    /// One MCP server by configured name.
    McpServer(String),
    /// One skill by name.
    Skill(String),
    /// One tool by hand name (`tool:bash`).
    Tool(String),
    /// One markdown subagent by name.
    Agent(String),
}

impl std::fmt::Display for FeatureSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeatureSpec::Agents => f.write_str("agents"),
            FeatureSpec::Skills => f.write_str("skills"),
            FeatureSpec::Mcp => f.write_str("mcp"),
            FeatureSpec::Hooks => f.write_str("hooks"),
            FeatureSpec::Web => f.write_str("web"),
            FeatureSpec::Lsp => f.write_str("lsp"),
            FeatureSpec::Debug => f.write_str("debug"),
            FeatureSpec::McpServer(name) => write!(f, "mcp:{name}"),
            FeatureSpec::Skill(name) => write!(f, "skill:{name}"),
            FeatureSpec::Tool(name) => write!(f, "tool:{name}"),
            FeatureSpec::Agent(name) => write!(f, "agent:{name}"),
        }
    }
}

impl std::str::FromStr for FeatureSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let item = |prefix: &str| s.strip_prefix(prefix).map(str::to_string);
        match s {
            "agents" => Ok(FeatureSpec::Agents),
            "skills" => Ok(FeatureSpec::Skills),
            "mcp" => Ok(FeatureSpec::Mcp),
            "hooks" => Ok(FeatureSpec::Hooks),
            "web" => Ok(FeatureSpec::Web),
            "lsp" => Ok(FeatureSpec::Lsp),
            "debug" => Ok(FeatureSpec::Debug),
            _ => {
                let prefixed = [
                    ("mcp:", FeatureSpec::McpServer as fn(String) -> FeatureSpec),
                    ("skill:", FeatureSpec::Skill),
                    ("tool:", FeatureSpec::Tool),
                    ("agent:", FeatureSpec::Agent),
                ]
                .into_iter()
                .find_map(|(prefix, mk)| item(prefix).filter(|n| !n.is_empty()).map(mk));
                prefixed.ok_or_else(|| {
                    format!(
                        "unknown feature spec `{s}`: expected agents|skills|mcp|hooks|web|lsp|debug \
                         or mcp:<server>|skill:<name>|tool:<name>|agent:<name>"
                    )
                })
            }
        }
    }
}

impl Serialize for FeatureSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for FeatureSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for FeatureSpec {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "FeatureSpec".into()
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "Feature toggle: agents|skills|mcp|hooks|web|lsp|debug, or one item as mcp:<server>, skill:<name>, tool:<name>, agent:<name>."
        })
    }
}

/// Why a turn finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Stop {
    /// The model produced a final reply.
    Done,
    /// Output hit the model's output limit.
    Length,
    /// The user aborted mid-turn.
    Aborted,
    /// The turn ended in an error.
    Error,
}

/// Token usage and cost for one turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Usage {
    /// Input tokens (estimate until the provider's usage arrives).
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cache-read tokens.
    pub cache_read: u64,
    /// Cache-write tokens.
    pub cache_write: u64,
    /// Cost in USD for the turn.
    pub cost: f64,
}

/// One attached image: base64 payload and its IANA media type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImagePart {
    /// Base64-encoded image bytes.
    pub data: String,
    /// Media type, e.g. `image/png`.
    pub media_type: String,
}

/// Snapshot of context-window consumption.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct ContextMeter {
    /// Estimated tokens in context.
    pub used: u64,
    /// Context window of the active model (0 = unknown).
    pub window: u64,
}

/// Engine error classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// Authentication/authorization failure.
    Auth,
    /// Rate limiting / quota.
    RateLimit,
    /// Context window exceeded.
    Overflow,
    /// Transport-level failure.
    Network,
    /// Malformed provider traffic.
    Protocol,
    /// Command not wired in this build/phase.
    Unsupported,
    /// Anything else.
    Internal,
}

/// One streaming delta from the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeltaKind {
    /// Visible assistant text.
    Text(String),
    /// Reasoning/thought text.
    Thought(String),
    /// A tool call started (arguments stream later, per tool).
    Call {
        /// Tool name.
        tool: String,
        /// Call identifier.
        id: String,
    },
}

/// An interactive question posed to the user mid-turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AskQuestion {
    /// The question text.
    pub text: String,
    /// Selectable answers (index-referenced in [`Command::Answer`]).
    pub options: Vec<String>,
    /// Optional rendered detail (e.g. a unified diff) shown above the
    /// options. Additive: absent in older strands, never required.
    #[serde(default)]
    pub detail: Option<String>,
}

/// Surface → engine commands.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Start a turn with a user prompt.
    Prompt {
        /// Prompt text.
        text: String,
        /// Structured-output JSON schema the reply must satisfy
        /// (None = free-form text).
        #[serde(default)]
        schema: Option<serde_json::Value>,
        /// Images attached to the prompt (base64 data + media type).
        #[serde(default)]
        images: Vec<ImagePart>,
        /// Per-turn tool allowlist (custom-command frontmatter
        /// `allowed-tools`); None = the session toolset. Additive.
        #[serde(default)]
        allowed_tools: Option<Vec<String>>,
        /// Per-turn model selector override (custom-command frontmatter
        /// `model`); None = the session model. Additive.
        #[serde(default)]
        model: Option<String>,
        /// Per-turn skill invocation (`/skill:<name>`): the named
        /// skills' full SKILL.md bodies are injected into this turn's
        /// system prompt. Additive.
        #[serde(default)]
        skills: Vec<String>,
    },
    /// Re-run tools/list on every MCP server (watchdog-adjacent).
    RefreshMcp,
    /// Fetch an MCP prompt and start a turn with its rendered text.
    CallPrompt {
        /// Owning server name.
        server: String,
        /// Prompt name on the server.
        name: String,
        /// Prompt arguments.
        #[serde(default)]
        args: std::collections::HashMap<String, String>,
    },
    /// Deliver user input mid-turn (steering), between tool batches.
    /// Wire tag stays `interject` (recorded strands keep their shape).
    #[serde(rename = "interject")]
    Steer {
        /// Steering text.
        text: String,
    },
    /// Queue user input for after the current turn settles. Wire tag
    /// stays `defer` (recorded strands keep their shape).
    #[serde(rename = "defer")]
    Queue {
        /// Queued text.
        text: String,
    },
    /// Run a user-typed shell command directly (the TUI's `!`
    /// passthrough). Output shows in the transcript and rides the next
    /// prompt as context. No permission gate — the user typed it.
    ///
    /// TRUST INVARIANT: only the local TUI may send this. Remote
    /// surfaces (serve, ACP) construct a fixed command subset and must
    /// never forward arbitrary protocol commands, or this becomes an
    /// ungated exec primitive for whoever can reach the transport.
    Shell {
        /// Command line for `sh -c`.
        command: String,
    },
    /// Abort the current turn (partial work is kept).
    Abort,
    /// Snapshot background tasks/jobs for the /tasks dashboard.
    ListTasks,
    /// Full result of background task `t-<id>` for the TUI pager
    /// (uncapped).
    TaskDetail {
        /// Task id (the `t-<id>` roster prefix).
        id: u64,
    },
    /// Live DAP session roster for the `/debug` overlay.
    DebugRoster,
    /// Switch the active model.
    SetModel {
        /// Model selector, e.g. `vendor/model:effort`.
        selector: String,
    },
    /// Set the reasoning effort.
    SetEffort {
        /// Effort level.
        level: Effort,
    },
    /// Switch permission mode.
    SetMode {
        /// New mode.
        mode: Mode,
    },
    /// Toggle a feature or capability for the rest of the session and the
    /// strand (restored on resume). Disabling hides the capability from
    /// the model's tool list and rejects stray calls; enabling may spawn
    /// MCP servers / the LSP or debug tier on the spot. Never honored
    /// under `--safe-mode` (bare mode pins everything off).
    ///
    /// TRUST INVARIANT: only the local surface may send this, like
    /// [`Command::Shell`]. Remote surfaces (serve, ACP) must never
    /// forward it — a forwarded enable/disable is configuration control
    /// for whoever can reach the transport.
    SetFeature {
        /// What to toggle.
        spec: FeatureSpec,
        /// New state.
        enabled: bool,
    },
    /// Switch the bash sandbox mode (`"off"` | `"fs"`). The `fs` policy is
    /// recomputed from the config `[sandbox]` allowlist — unpersisted
    /// session grants drop on `off`→`fs` (fresh-policy semantics).
    ///
    /// TRUST INVARIANT: local surface only, like [`Command::Shell`].
    SetSandbox {
        /// New mode, `"off"` or `"fs"`.
        mode: String,
    },
    /// Trigger a digest (manual compaction).
    Compact {
        /// Optional focus instructions for the summary.
        focus: Option<String>,
    },
    /// Name the current session with a memorable alias, appended to the
    /// strand as a `Name` record. `ka --session <name>` resolves on it
    /// (exact, then unique prefix) before title matching.
    SetName {
        /// The alias (1–64 chars, no whitespace).
        name: String,
    },
    /// Transfer: digest the live context, then attach a FRESH child
    /// strand that carries only the digest — a clean handoff session
    /// with the summary as its memory. The old strand is untouched on
    /// disk (parent link set).
    TransferStrand {
        /// Optional focus instructions for the summary.
        focus: Option<String>,
    },
    /// Rewind the conversation to before the Nth-last user message.
    Rewind {
        /// How many user turns back (1 = erase the last exchange).
        turns: u32,
    },
    /// Copy the current strand into a NEW strand file truncated to drop
    /// the last `turns` user turns (`0` = exact copy) and switch to it.
    ForkStrand {
        /// How many trailing user turns the fork drops.
        turns: u32,
    },
    /// Snapshot the working tree (git-based, non-destructive: never
    /// touches the index, stash, or HEAD).
    Checkpoint,
    /// Restore a checkpoint made with [`Command::Checkpoint`]. The id
    /// `"list"` asks for a note listing known ids instead.
    RestoreCheckpoint {
        /// Checkpoint id, or `list`.
        id: String,
    },
    /// Switch the engine to another strand ("new" = fresh session, else a
    /// strand id / id prefix / existing file path).
    SwitchStrand {
        /// Target session reference.
        id: String,
    },
    /// Restore the latest pre-mutation snapshot of this session.
    UndoFile,
    /// Save an API key to the user env layer (~/.config/ka/.env) and the
    /// live process (used by the TUI key prompt).
    SaveApiKey {
        /// Env var name, e.g. `ZHIPU_API_KEY`.
        env_var: String,
        /// The key value (never echoed to the transcript).
        value: String,
    },
    /// Export this session to markdown. `out` overrides the default
    /// `ka-session-<tail>.md` file in the working directory.
    ExportMarkdown {
        /// Output file path (None = default name in cwd).
        out: Option<std::path::PathBuf>,
        /// Render a self-contained HTML page instead of markdown.
        #[serde(default)]
        html: bool,
    },
    /// Persist settings to the user config layer (~/.config/ka/ka.toml).
    SaveSettings {
        /// Default model selector.
        model: Option<String>,
        /// Default reasoning effort.
        effort: Option<Effort>,
        /// Default permission mode.
        mode: Option<Mode>,
        /// TUI mouse mode (`"capture"` = SGR button reporting, wheel
        /// scrolls, ⇧drag selects; `"native"` = no capture, plain drag
        /// selects, the wheel rides DECSET 1007 as ↑/↓). None leaves
        /// the stored value alone.
        #[serde(default)]
        mouse: Option<String>,
        /// Default feature toggles to persist as `[features] disable` in
        /// the user layer — the disabled specs, in the shared grammar.
        /// None leaves the stored value alone. Additive.
        #[serde(default)]
        features: Option<Vec<String>>,
    },
    /// Ask for a context-usage breakdown (see [`Event::ContextBreakdown`]).
    ContextBreakdown,
    /// Answer an outstanding ask.
    Answer {
        /// Which ask is being answered.
        question: AskId,
        /// Chosen option index.
        choice: usize,
    },
}

/// One replayed tool call: the header the live turn showed plus its
/// merged result note (filled in from the following tool message).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReplayedCall {
    /// Engine call id.
    pub id: String,
    /// Tool name.
    pub tool: String,
    /// Argument summary (the CallStarted detail).
    pub detail: String,
    /// First-line result excerpt once the tool message landed.
    pub result: Option<String>,
    /// Whether the tool reported an error.
    pub is_error: bool,
}

/// A replayed historical message (emitted on resume so surfaces can
/// reconstruct the transcript).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReplayedMessage {
    /// `user`, `assistant`, or `digest` (a compaction divider).
    pub role: String,
    /// Message text.
    pub content: String,
    /// Whether this row is a digest divider (additive).
    #[serde(default)]
    pub digest: bool,
    /// Assistant reasoning captured with the message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Tool calls issued with this assistant message; their results
    /// merge in from the following tool-role messages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<ReplayedCall>,
}

/// Per-MCP-server line of the bootstrap [`Event::Inventory`] card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpSummary {
    /// Configured server name.
    pub name: String,
    /// Whether spawn + handshake + tool listing succeeded.
    pub ok: bool,
    /// Tools the server advertises (0 when `ok` is false).
    pub tools: usize,
}

/// One entry of the model-maintained todo list ([`Event::Todos`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoItem {
    /// What to do.
    pub text: String,
    /// Whether it is finished.
    pub state: TodoState,
    /// Optional self-rated confidence, 0–100. Rated when the item is
    /// assigned and re-rated when it is marked done; a low→high spike
    /// at completion is treated as a claim, not a measurement (the
    /// harness asks for verification). Absent = unrated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<u8>,
}

/// Completion state of a [`TodoItem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoState {
    /// Still to do.
    Pending,
    /// Finished.
    Done,
}

/// One component of the context breakdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContextPart {
    /// Component name (`system`, `user`, `assistant`, `tools`).
    pub name: String,
    /// Estimated tokens in this component.
    pub tokens: u64,
}

/// Engine → surface events.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Resumed history replay (emitted at startup and on session switch;
    /// surfaces rebuild the transcript from it).
    Replay {
        /// Prior messages, oldest first.
        messages: Vec<ReplayedMessage>,
    },
    /// Live context meter during a turn (emitted per model step so
    /// surfaces show usage while the turn runs, not only at the end).
    ContextMeter {
        /// Tokens currently in context.
        used: u64,
        /// Window size (0 = unknown).
        window: u64,
    },
    /// Reasoning effort changed.
    EffortChanged {
        /// Effort level.
        level: Effort,
    },
    /// The active session (strand) changed or was announced. Surfaces use
    /// it to label the session and the /session picker.
    SessionInfo {
        /// Active strand id.
        id: String,
    },
    /// The strand's display title (stored `Record::Title` or the
    /// auto-generated one). Emitted at bootstrap when the strand already
    /// has a title and after a live auto-title lands, so surfaces can
    /// relabel the session without replay. Additive: absent `title`
    /// deserializes empty (surfaces ignore empty titles).
    Title {
        /// The display title.
        #[serde(default)]
        title: String,
    },
    /// A turn began.
    TurnStarted {
        /// Context consumption snapshot.
        context: ContextMeter,
    },
    /// A streaming delta arrived.
    Delta {
        /// What kind of delta.
        kind: DeltaKind,
    },
    /// A tool call began executing.
    CallStarted {
        /// Tool name.
        tool: String,
        /// Call identifier.
        id: String,
        /// Short argument summary for transcript headers (empty when the
        /// tool has nothing worth showing or the sender predates the
        /// field). Additive: absent JSON deserializes as empty.
        #[serde(default)]
        detail: String,
    },
    /// A tool call finished.
    CallFinished {
        /// Tool name.
        tool: String,
        /// Call identifier.
        id: String,
        /// Whether the call succeeded.
        ok: bool,
    },
    /// A tool call produced output (capped excerpt for surfaces).
    CallOutput {
        /// Tool name.
        tool: String,
        /// Call identifier.
        id: String,
        /// Capped output excerpt (full output may live in a spill file).
        excerpt: String,
        /// Whether the output represents an error result.
        is_error: bool,
        /// Spill pointer when the full output was parked on disk.
        spill: Option<String>,
    },
    /// The engine needs user input.
    Ask {
        /// Ask identifier.
        id: AskId,
        /// Questions to present.
        questions: Vec<AskQuestion>,
    },
    /// A turn finished.
    TurnFinished {
        /// Why it finished.
        stop: Stop,
        /// Usage accounting for the turn.
        usage: Usage,
    },
    /// A digest (compaction) started.
    DigestStarted,
    /// A digest finished; kept history starts at `kept`.
    DigestFinished {
        /// First kept record.
        kept: RecordId,
    },
    /// Permission mode changed.
    ModeChanged {
        /// New mode.
        mode: Mode,
    },
    /// Feature toggles changed (also emitted at attach when the strand
    /// restores a non-default set). `disabled` is the full snapshot in
    /// the shared spec grammar; `sandbox` is the live sandbox-mode
    /// override for this session (None = follows config).
    FeaturesChanged {
        /// Disabled specs (full snapshot).
        disabled: Vec<String>,
        /// Live sandbox mode override, if any.
        sandbox: Option<String>,
    },
    /// Active model changed.
    ModelChanged {
        /// Selector that was applied.
        selector: String,
    },
    /// The full prompt cycle (turn + settling) is complete; the engine is
    /// idle. Long-lived surfaces keep running; one-shot surfaces may exit.
    Idle,
    /// Session bootstrap inventory: what this conversation can use.
    Inventory {
        /// Built-in + MCP tool names as the model sees them.
        tools: Vec<String>,
        /// Per configured MCP server: name, connect ok, tool count.
        mcp: Vec<McpSummary>,
        /// Discovered subagent names.
        agents: Vec<String>,
        /// Discovered skill names.
        skills: Vec<String>,
        /// Advertised MCP prompts as `server/name (args)` (additive).
        #[serde(default)]
        prompts: Vec<String>,
        /// Specs currently disabled by feature toggles, in the shared
        /// grammar (additive: absent deserializes empty).
        #[serde(default)]
        disabled: Vec<String>,
    },
    /// The model's live todo list (the `todo` hand). Whole-list
    /// replacement: each event supersedes the previous one.
    Todos {
        /// Current items, in display order.
        items: Vec<TodoItem>,
    },
    /// Informational note (pruning/digest notices, etc.).
    Note {
        /// Note text.
        message: String,
    },
    /// Observer presence changed on a `ka serve` session (roadmap 9.6):
    /// emitted to subscribers when an attach client arrives or leaves.
    /// The writing surface never sees it (the engine does not emit it —
    /// the server does); older consumers ignore it as an unknown type.
    Presence {
        /// A turn is in flight on the session.
        busy: bool,
        /// Live SSE observers right now (the writer is not counted).
        attached: usize,
    },
    /// Output of a user `!` shell passthrough (redacted, capped).
    ShellOutput {
        /// The command line that ran.
        command: String,
        /// Capped combined stdout+stderr (may be empty).
        output: String,
        /// Failure note (spawn error / non-zero exit / timeout), when
        /// there is one.
        #[serde(default)]
        note: Option<String>,
    },
    /// /tasks dashboard snapshot: background delegate tasks and bash
    /// jobs, pre-rendered rows.
    Tasks {
        /// Rendered rows.
        rows: Vec<String>,
    },
    /// Uncapped task result (answer to [`Command::TaskDetail`]).
    TaskDetail {
        /// Task id.
        id: u64,
        /// Full result text.
        text: String,
    },
    /// Pre-rendered DAP session rows (answer to [`Command::DebugRoster`]).
    DebugRoster {
        /// Rendered rows.
        rows: Vec<String>,
    },
    /// Context-usage breakdown (/context): estimated tokens per
    /// component plus the active window.
    ContextBreakdown {
        /// Estimated tokens per component.
        parts: Vec<ContextPart>,
        /// Context window of the active model (0 = unknown).
        window: u64,
    },
    /// Engine-level error report.
    Error {
        /// Error classification.
        class: ErrorClass,
        /// Whether retrying could help.
        retryable: bool,
        /// Human-readable detail.
        message: String,
    },
}

/// Serialize a value as one NDJSON line (trailing newline included).
pub fn to_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    Ok(line)
}

/// Parse one NDJSON line into `T`.
pub fn from_line<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn roundtrip_command(cmd: Command) {
        let line = to_line(&cmd).unwrap();
        assert!(line.ends_with('\n'));
        let back: Command = from_line(&line).unwrap();
        assert_eq!(
            serde_json::to_string(&cmd).unwrap(),
            serde_json::to_string(&back).unwrap()
        );
    }

    fn roundtrip_event(evt: Event) {
        let line = to_line(&evt).unwrap();
        let back: Event = from_line(&line).unwrap();
        assert_eq!(
            serde_json::to_string(&evt).unwrap(),
            serde_json::to_string(&back).unwrap()
        );
    }

    #[test]
    fn ids_serialize_as_bare_strings() {
        let line = to_line(&Event::DigestFinished {
            kept: RecordId("r7".into()),
        })
        .unwrap();
        assert!(line.contains("\"kept\":\"r7\""), "got: {line}");
    }

    #[test]
    fn prompt_decodes_from_legacy_wire_line() {
        // allowed_tools/model are additive wire fields: an NDJSON line
        // recorded before they existed must keep decoding to None —
        // the None-on-absent contract recorded strands rely on
        let line = r#"{"type":"prompt","text":"hi"}"#;
        let cmd: Command = from_line(line).unwrap();
        match cmd {
            Command::Prompt {
                allowed_tools,
                model,
                text,
                skills,
                ..
            } => {
                assert_eq!(allowed_tools, None);
                assert_eq!(model, None);
                assert_eq!(text, "hi");
                assert!(skills.is_empty(), "absent skills decode empty");
            }
            other => panic!("expected prompt, got {other:?}"),
        }
    }

    #[test]
    fn commands_roundtrip() {
        roundtrip_command(Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "hi".into(),
            schema: None,
            images: Vec::new(),
            skills: Vec::new(),
        });
        roundtrip_command(Command::Steer {
            text: "use tabs".into(),
        });
        roundtrip_command(Command::Queue {
            text: "then run tests".into(),
        });
        roundtrip_command(Command::Abort);
        roundtrip_command(Command::SetModel {
            selector: "anthropic/claude-sonnet-5:high".into(),
        });
        roundtrip_command(Command::SetEffort { level: Effort::Max });
        roundtrip_command(Command::SetMode { mode: Mode::Allow });
        roundtrip_command(Command::Compact {
            focus: Some("keep API notes".into()),
        });
        roundtrip_command(Command::SetName { name: "fox".into() });
        roundtrip_command(Command::TransferStrand { focus: None });
        roundtrip_command(Command::TransferStrand {
            focus: Some("keep the API notes".into()),
        });
        roundtrip_command(Command::Answer {
            question: AskId("q1".into()),
            choice: 0,
        });
        roundtrip_command(Command::ExportMarkdown {
            out: Some(std::path::PathBuf::from("x.md")),
            html: false,
        });
        roundtrip_command(Command::ExportMarkdown {
            out: None,
            html: false,
        });
        roundtrip_command(Command::ExportMarkdown {
            out: None,
            html: true,
        });
    }

    #[test]
    fn steer_and_queue_keep_wire_tags() {
        // the vocabulary became steer/queue, but the wire stays
        // `interject`/`defer` — recorded strands must keep parsing
        let steer = to_line(&Command::Steer {
            text: "use tabs".into(),
        })
        .unwrap();
        assert!(
            steer.contains("\"type\":\"interject\""),
            "wire tag: {steer}"
        );
        let queue = to_line(&Command::Queue {
            text: "then run tests".into(),
        })
        .unwrap();
        assert!(queue.contains("\"type\":\"defer\""), "wire tag: {queue}");
    }

    #[test]
    fn mode_accept_edits_wire_name_and_roundtrip() {
        let cmd = Command::SetMode {
            mode: Mode::AcceptEdits,
        };
        let line = to_line(&cmd).unwrap();
        assert!(line.contains("\"accept_edits\""), "wire name: {line}");
        let back: Command = from_line(&line).unwrap();
        assert_eq!(
            serde_json::to_string(&back).unwrap(),
            serde_json::to_string(&cmd).unwrap()
        );
        let evt = Event::ModeChanged {
            mode: Mode::AcceptEdits,
        };
        let line = to_line(&evt).unwrap();
        let back: Event = from_line(&line).unwrap();
        assert_eq!(
            serde_json::to_string(&back).unwrap(),
            serde_json::to_string(&evt).unwrap()
        );
    }

    #[test]
    fn events_roundtrip() {
        roundtrip_event(Event::Replay {
            messages: vec![ReplayedMessage {
                role: "user".into(),
                content: "before the crash".into(),
                digest: false,
                thinking: None,
                calls: Vec::new(),
            }],
        });
        roundtrip_event(Event::Replay {
            messages: vec![ReplayedMessage {
                role: "assistant".into(),
                content: String::new(),
                digest: false,
                thinking: Some("weighing options".into()),
                calls: vec![ReplayedCall {
                    id: "c1".into(),
                    tool: "read".into(),
                    detail: "lib.rs".into(),
                    result: Some("fn main() {}".into()),
                    is_error: false,
                }],
            }],
        });
        roundtrip_event(Event::TurnStarted {
            context: ContextMeter {
                used: 12,
                window: 200_000,
            },
        });
        roundtrip_event(Event::Delta {
            kind: DeltaKind::Text("hello".into()),
        });
        roundtrip_event(Event::Delta {
            kind: DeltaKind::Thought("thinking...".into()),
        });
        roundtrip_event(Event::Delta {
            kind: DeltaKind::Call {
                tool: "read".into(),
                id: "c1".into(),
            },
        });
        roundtrip_event(Event::CallStarted {
            tool: "bash".into(),
            id: "c2".into(),
            detail: "cargo test --workspace".into(),
        });
        roundtrip_event(Event::CallFinished {
            tool: "bash".into(),
            id: "c2".into(),
            ok: true,
        });
        roundtrip_event(Event::Ask {
            id: AskId("q1".into()),
            questions: vec![AskQuestion {
                text: "Proceed?".into(),
                options: vec!["yes".into(), "no".into()],
                detail: Some("--- a/f.rs\n+++ b/f.rs\n@@\n".into()),
            }],
        });
        roundtrip_event(Event::ContextBreakdown {
            parts: vec![ContextPart {
                name: "system".into(),
                tokens: 950,
            }],
            window: 128_000,
        });
        roundtrip_event(Event::TurnFinished {
            stop: Stop::Done,
            usage: Usage {
                input: 100,
                output: 20,
                cache_read: 0,
                cache_write: 0,
                cost: 0.001,
            },
        });
        roundtrip_command(Command::TaskDetail { id: 7 });
        roundtrip_command(Command::DebugRoster);
        roundtrip_command(Command::SaveSettings {
            model: None,
            effort: None,
            mode: None,
            mouse: Some("capture".into()),
            features: None,
        });
        // older surfaces predating the mouse field still parse (additive)
        let legacy: Command =
            from_line(r#"{"type":"save_settings","model":null,"effort":null,"mode":null}"#)
                .unwrap();
        assert!(
            matches!(
                legacy,
                Command::SaveSettings {
                    mouse: None,
                    features: None,
                    ..
                }
            ),
            "{legacy:?}"
        );
        roundtrip_event(Event::Title {
            title: "fix the parser".into(),
        });
        roundtrip_event(Event::DigestStarted);
        roundtrip_event(Event::DigestFinished {
            kept: RecordId("r4".into()),
        });
        roundtrip_event(Event::ModeChanged { mode: Mode::Ask });
        roundtrip_event(Event::ModelChanged {
            selector: "openai/gpt-5.1".into(),
        });
        roundtrip_event(Event::Note {
            message: "pruned ~2k tokens".into(),
        });
        roundtrip_command(Command::Shell {
            command: "cargo test".into(),
        });
        roundtrip_command(Command::ListTasks);
        roundtrip_event(Event::ShellOutput {
            command: "cargo test".into(),
            output: "test result: ok".into(),
            note: Some("exit 101".into()),
        });
        roundtrip_event(Event::Tasks {
            rows: vec!["t-1  running  12s  reviewer — audit".into()],
        });
        roundtrip_event(Event::TaskDetail {
            id: 1,
            text: "full result text".into(),
        });
        roundtrip_event(Event::DebugRoster {
            rows: vec!["no debug sessions (debug action \"start\")".into()],
        });
        roundtrip_event(Event::Error {
            class: ErrorClass::Unsupported,
            retryable: false,
            message: "not wired in phase 0".into(),
        });
        roundtrip_event(Event::Inventory {
            tools: vec!["read".into(), "demo.fetch".into()],
            mcp: vec![
                McpSummary {
                    name: "demo".into(),
                    ok: true,
                    tools: 2,
                },
                McpSummary {
                    name: "jira".into(),
                    ok: false,
                    tools: 0,
                },
            ],
            agents: vec!["coder".into()],
            skills: vec!["rust-docs".into()],
            prompts: Vec::new(),
            disabled: vec!["mcp:jira".into()],
        });
        roundtrip_event(Event::Todos {
            items: vec![
                TodoItem {
                    text: "scaffold the parser".into(),
                    state: TodoState::Done,
                    confidence: Some(90),
                },
                TodoItem {
                    text: "wire the sidebar".into(),
                    state: TodoState::Pending,
                    confidence: None,
                },
            ],
        });
        roundtrip_event(Event::Todos { items: Vec::new() });
        roundtrip_event(Event::Inventory {
            tools: vec!["read".into()],
            mcp: Vec::new(),
            agents: Vec::new(),
            skills: Vec::new(),
            prompts: Vec::new(),
            disabled: Vec::new(),
        });
    }

    #[test]
    fn call_started_detail_defaults_when_absent() {
        // pre-detail senders: the additive field must deserialize empty
        let back: Event = from_line(r#"{"type":"call_started","tool":"bash","id":"c9"}"#).unwrap();
        match back {
            Event::CallStarted { tool, id, detail } => {
                assert_eq!((tool.as_str(), id.as_str()), ("bash", "c9"));
                assert!(detail.is_empty(), "absent detail must default empty");
            }
            other => panic!("wrong event: {other:?}"),
        }
    }
    #[test]
    fn title_defaults_when_absent_and_tags_snake_case() {
        let line = to_line(&Event::Title {
            title: "fix the parser".into(),
        })
        .unwrap();
        assert!(line.contains("\"type\":\"title\""), "got: {line}");
        // pre-title senders: the additive field must deserialize empty
        let back: Event = from_line(r#"{"type":"title"}"#).unwrap();
        match back {
            Event::Title { title } => assert!(title.is_empty()),
            other => panic!("expected title, got {other:?}"),
        }
    }

    #[test]
    fn todos_wire_shape_is_snake_case() {
        let line = to_line(&Event::Todos {
            items: vec![TodoItem {
                text: "ship it".into(),
                state: TodoState::Done,
                confidence: Some(100),
            }],
        })
        .unwrap();
        assert!(line.contains("\"type\":\"todos\""), "got: {line}");
        assert!(line.contains("\"state\":\"done\""), "got: {line}");
        assert!(line.contains("\"confidence\":100"), "got: {line}");
        let back: Event = from_line(&line).unwrap();
        match back {
            Event::Todos { items } => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].text, "ship it");
                assert_eq!(items[0].state, TodoState::Done);
                assert_eq!(items[0].confidence, Some(100));
            }
            other => panic!("expected todos, got: {other:?}"),
        }
        // unrated items must not carry the key: pre-confidence
        // senders/receivers stay wire-compatible
        let plain = to_line(&Event::Todos {
            items: vec![TodoItem {
                text: "old".into(),
                state: TodoState::Pending,
                confidence: None,
            }],
        })
        .unwrap();
        assert!(!plain.contains("confidence"), "got: {plain}");
    }

    #[test]
    fn todos_decode_from_wire_line() {
        let line = concat!(
            r#"{"type":"todos","items":[{"text":"a","state":"pending"},"#,
            r#"{"text":"b","state":"done"}]}"#,
        );
        let evt: Event = from_line(line).unwrap();
        match evt {
            Event::Todos { items } => assert_eq!(
                items,
                vec![
                    TodoItem {
                        text: "a".into(),
                        state: TodoState::Pending,
                        confidence: None
                    },
                    TodoItem {
                        text: "b".into(),
                        state: TodoState::Done,
                        confidence: None
                    },
                ]
            ),
            other => panic!("expected todos, got {other:?}"),
        }
    }
    #[test]
    fn inventory_decodes_from_wire_line() {
        let line = concat!(
            r#"{"type":"inventory","tools":["read","write","#,
            r#""demo.fetch"],"mcp":[{"name":"demo","ok":true,"tools":2}],"#,
            r#""agents":[],"skills":["demo"]}"#,
        );
        let evt: Event = from_line(line).unwrap();
        match evt {
            Event::Inventory {
                tools,
                mcp,
                agents,
                skills,
                prompts: _,
                disabled,
            } => {
                assert_eq!(
                    tools,
                    vec![
                        "read".to_string(),
                        "write".to_string(),
                        "demo.fetch".to_string()
                    ]
                );
                assert_eq!(
                    mcp,
                    vec![McpSummary {
                        name: "demo".into(),
                        ok: true,
                        tools: 2,
                    }]
                );
                assert!(agents.is_empty());
                assert_eq!(skills, vec!["demo".to_string()]);
                assert!(
                    disabled.is_empty(),
                    "pre-toggle inventory lines must decode with no disabled specs"
                );
            }
            other => panic!("expected inventory, got {other:?}"),
        }
    }

    #[test]
    fn inventory_tags_snake_case_on_wire() {
        let line = to_line(&Event::Inventory {
            tools: Vec::new(),
            mcp: Vec::new(),
            agents: Vec::new(),
            skills: Vec::new(),
            prompts: Vec::new(),
            disabled: Vec::new(),
        })
        .unwrap();
        assert!(line.contains("\"type\":\"inventory\""), "got: {line}");
    }

    #[test]
    fn tagging_is_snake_case() {
        let line = to_line(&Command::Abort).unwrap();
        assert!(line.contains("\"type\":\"abort\""), "got: {line}");
        let line = to_line(&Event::TurnStarted {
            context: ContextMeter { used: 1, window: 2 },
        })
        .unwrap();
        assert!(line.contains("\"type\":\"turn_started\""), "got: {line}");
    }

    #[test]
    fn feature_spec_grammar_roundtrips() {
        use std::str::FromStr;
        for s in [
            "agents",
            "skills",
            "mcp",
            "hooks",
            "web",
            "lsp",
            "debug",
            "mcp:github",
            "skill:pdf",
            "tool:bash",
            "agent:reviewer",
        ] {
            let spec = FeatureSpec::from_str(s).unwrap_or_else(|e| panic!("{s}: {e}"));
            assert_eq!(spec.to_string(), s, "display/parse must be inverse");
        }
        for bad in ["", "agent:", "mcp:", "bogus", "tools:bash", "AGENTS"] {
            assert!(
                FeatureSpec::from_str(bad).is_err(),
                "`{bad}` must not parse as a spec"
            );
        }
    }

    #[test]
    fn set_feature_serializes_spec_as_bare_string() {
        let line = to_line(&Command::SetFeature {
            spec: FeatureSpec::McpServer("github".into()),
            enabled: false,
        })
        .unwrap();
        assert!(
            line.contains("\"spec\":\"mcp:github\"") && line.contains("\"enabled\":false"),
            "got: {line}"
        );
        let back: Command = from_line(&line).unwrap();
        assert!(matches!(
            back,
            Command::SetFeature {
                spec: FeatureSpec::McpServer(ref n),
                enabled: false,
            } if n == "github"
        ));
        // an unknown spec must fail decode, not silently pass through
        assert!(
            from_line::<Command>(r#"{"type":"set_feature","spec":"nope","enabled":true}"#).is_err()
        );
    }

    #[test]
    fn set_feature_and_set_sandbox_roundtrip() {
        roundtrip_command(Command::SetFeature {
            spec: FeatureSpec::Agents,
            enabled: false,
        });
        roundtrip_command(Command::SetFeature {
            spec: FeatureSpec::Tool("bash".into()),
            enabled: true,
        });
        roundtrip_command(Command::SetSandbox { mode: "fs".into() });
        roundtrip_command(Command::SetSandbox { mode: "off".into() });
    }

    #[test]
    fn features_changed_wire_shape() {
        let line = to_line(&Event::FeaturesChanged {
            disabled: vec!["agents".into(), "mcp:github".into()],
            sandbox: Some("fs".into()),
        })
        .unwrap();
        assert!(
            line.contains("\"type\":\"features_changed\""),
            "got: {line}"
        );
        let back: Event = from_line(&line).unwrap();
        match back {
            Event::FeaturesChanged { disabled, sandbox } => {
                assert_eq!(
                    disabled,
                    vec!["agents".to_string(), "mcp:github".to_string()]
                );
                assert_eq!(sandbox.as_deref(), Some("fs"));
            }
            other => panic!("expected features_changed, got {other:?}"),
        }
    }
}
