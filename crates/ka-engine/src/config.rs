//! Layered strict-TOML configuration.
//!
//! Chain (lowest → highest): built-in defaults → user `~/.config/ka/ka.toml`
//! → project `.ka/ka.toml` → environment (`KA_MODEL`, `KA_MODE`) → CLI flags.
//! Every textual layer is parsed strictly: unknown keys are hard errors that
//! carry the TOML position, so typos never silently pass.
//!
//! Optional spend/context guards live under `[guards]` (both default off):
//!
//! ```toml
//! [guards]
//! spend_usd = 2.0   # ask before the session's total cost crosses $2.00
//! context_pct = 90  # ask when the context meter first passes 90%
//! ```
//!
//! On the first crossing per session per guard the engine poses the
//! existing `Event::Ask` flow with `[continue, stop]`; `stop` aborts the
//! current turn cleanly. Each guard latches once per session.

use ka_protocol::{Effort, Mode};
use serde::{Deserialize, Serialize};

/// One permission rule: first matching rule wins at gate time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Tool the rule applies to (`bash`, `read`, ...).
    pub tool: String,
    /// Glob pattern matched against the call's primary argument (bash:
    /// command line; file tools: path; search tools: pattern). `None`
    /// matches every call of the tool.
    #[serde(default)]
    pub pattern: Option<String>,
    /// What to do when it matches.
    pub verdict: Verdict,
}

/// One hook: a shell command run around tool calls or at turn end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    /// When to run.
    pub event: HookEvent,
    /// Only this tool (None = every tool).
    #[serde(default)]
    pub tool: Option<String>,
    /// Shell command. Tool-call hooks receive the tool name + arguments
    /// on stdin; `stop` hooks receive `{"event": "stop", "stop": "done"
    /// | "aborted" | "error"}`. Exit 2 blocks the call (pre_tool_use)
    /// with stderr as the reason.
    pub command: String,
}

/// Hook trigger points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// Before a tool executes; exit 2 blocks it.
    PreToolUse,
    /// After a tool finished; exit 2 marks the result an error.
    PostToolUse,
    /// After a turn finishes, any stop kind (`{"event":"stop",
    /// "stop":"done"|"aborted"|"error"}`).
    Stop,
    /// After a turn finishes — same point as `stop`, claude-compat
    /// spelling (`{"event":"turn_end","stop":…}`).
    TurnEnd,
    /// Once when a session (strand) attaches (`{"event":
    /// "session_start","cwd":…}`).
    SessionStart,
    /// Once when the session ends — the engine's command channel closed
    /// (`{"event":"session_end","cwd":…}`).
    SessionEnd,
    /// A user prompt was submitted, before the turn starts
    /// (`{"event":"user_prompt_submit","prompt":…}`).
    UserPromptSubmit,
    /// Before a digest/compaction runs (`{"event":"pre_compact",
    /// "focus":…}`). Only advisory steering; cannot stop the digest.
    PreCompact,
}

impl HookEvent {
    /// The `"event"` value hooks receive on stdin.
    pub fn wire_name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "pre_tool_use",
            HookEvent::PostToolUse => "post_tool_use",
            HookEvent::Stop => "stop",
            HookEvent::TurnEnd => "turn_end",
            HookEvent::SessionStart => "session_start",
            HookEvent::SessionEnd => "session_end",
            HookEvent::UserPromptSubmit => "user_prompt_submit",
            HookEvent::PreCompact => "pre_compact",
        }
    }
}

/// Rule verdicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Run without asking.
    Allow,
    /// Always ask, regardless of mode.
    Ask,
    /// Refuse outright.
    Deny,
}

/// Persistent tool allowlist (`[permissions] allow = [...]` in a ka.toml
/// layer). Listed tools skip the permission ask for the rest of the
/// session; picking "always" on an ask appends here (project layer).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Permissions {
    /// Tool names auto-allowed without asking.
    pub allow: Vec<String>,
}

/// Spend/context guard thresholds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Guards {
    /// USD total for the session at which the engine asks to continue
    /// (None = disabled).
    pub spend_usd: Option<f64>,
    /// Context-window percentage (1–100) at which the engine asks to
    /// continue (None = disabled).
    pub context_pct: Option<u64>,
    /// Opt-in auto-review of exec-tier permission asks (roadmap 9.4,
    /// default false): the `fast`-role model reviews each ask and
    /// auto-allows only unmistakably safe commands — it can never deny
    /// (anything short of confident falls through to you) and every
    /// auto-allow surfaces as a transcript note. Inert in --safe-mode.
    pub auto_review: Option<bool>,
}

/// Git automation settings.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields, default)]
pub struct Git {
    /// After each completed engine turn (stop = done), stage the whole
    /// worktree — untracked files and anything you staged yourself
    /// included (`git add -A`) — and commit it as `ka: <first prompt
    /// line>`. Off by default.
    pub auto_commit: Option<bool>,
}

/// Role → model-selector mappings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Roles {
    /// Selector for the default (main) model.
    pub default: Option<String>,
    /// Selector for the fast (cheap) role.
    pub fast: Option<String>,
}

/// Per-tool settings (`[tools]` in a ka.toml layer).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Tools {
    /// Bash tool settings.
    pub bash: BashTools,
    /// Read tool settings.
    pub read: ReadTools,
    /// Web tool settings.
    pub web: WebTools,
    /// MCP tool settings.
    pub mcp: McpTools,
}

/// MCP tool tuning (`[tools.mcp]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct McpTools {
    /// `"eager"` (default): every server tool becomes its own hand in
    /// context. `"lazy"`: one `mcp_call` hand stands in for all of them
    /// — the model lists a server's tools on demand. Prefer lazy when
    /// servers expose many tools.
    pub discovery: Option<String>,
}

/// Read tool tuning (`[tools.read]`).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields, default)]
pub struct ReadTools {
    /// Image size cap in MB for the read hand (None = 5; 0 = unlimited).
    pub max_image_mb: Option<u32>,
}

/// Web tool tuning (`[tools.web]`).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields, default)]
pub struct WebTools {
    /// Allow fetching private/loopback addresses (None = false).
    pub allow_private_hosts: Option<bool>,
}

/// Bash tool tuning (`[tools.bash]`).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields, default)]
pub struct BashTools {
    /// Auto-background a bash command still running after this many
    /// milliseconds: it keeps running, the model gets a normal result
    /// pointing at the `jobs` tool. None = the default (30000); 0 = never
    /// background.
    pub background_after_ms: Option<u64>,
}

/// Default image size cap for the read hand (5 MB).
pub const DEFAULT_MAX_IMAGE_MB: u32 = 5;

/// Default bash auto-background threshold (30s).
pub const DEFAULT_BACKGROUND_AFTER_MS: u64 = 30_000;

/// Self-update settings ([update]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Update {
    /// GitHub repo (`owner/name`) releases are fetched from.
    pub repo: Option<String>,
    /// Whether the TUI checks for a newer release on startup
    /// (rate-limited to one network hit per 24 h; never installs —
    /// `ka update` stays the only install path).
    pub check: Option<UpdateCheck>,
}

/// `[update] check`: the startup availability check.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum UpdateCheck {
    /// Check on TUI startup (the default).
    #[default]
    Startup,
    /// Never check.
    Off,
}

impl Update {
    /// The effective startup-check policy (default: on).
    pub fn effective_check(&self) -> UpdateCheck {
        self.check.unwrap_or_default()
    }
}

/// One web-search provider (`[[search]]`; first entry is active).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct SearchProvider {
    /// `tavily` | `brave` | `bing`.
    pub provider: String,
    /// Env var holding the API key.
    pub api_key_env: String,
    /// API base override (fixture tests, proxies).
    pub base_url: Option<String>,
}

/// Sandbox policy ([sandbox]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Sandbox {
    /// `"off"` (default) or `"fs"`.
    pub mode: Option<String>,
    /// Extra writable directories on top of the default allowlist
    /// (cwd, /tmp, XDG state/cache). An "always" answer on a
    /// sandbox-expansion ask appends the granted write paths here
    /// (project layer).
    pub allow_write: Vec<String>,
}

/// TUI appearance and notifications ([tui]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Tui {
    /// Title glyph for the transcript window (default ◆).
    pub header_glyph: Option<String>,
    /// Ring the terminal bell when a turn finishes or a permission ask
    /// appears (None = true; terminals mute bells by user choice).
    pub bell: Option<bool>,
    /// Command run when a turn finishes, JSON on stdin:
    /// `{"event":"turn_finished","stop":"done"}`. Example:
    /// `notify-send ka "turn done"`.
    pub notify: Option<String>,
    /// Mouse mode: `"native"` (default) — no capture at all, so plain
    /// drag selects and pastes with the terminal's own bindings and the
    /// wheel scrolls the chat via alternate-scroll; or `"capture"` —
    /// SGR button reporting: the wheel scrolls and the bottom-strip
    /// buttons become clickable, ⇧drag still selects. Ctrl+M toggles
    /// at runtime either way. kitty has no alternate-scroll — keep
    /// `"capture"` there for the wheel.
    pub mouse: Option<String>,
    /// Report this agent's state (`idle`/`working`/`blocked`), resume
    /// command, and exit to the Herdr pane hosting it (opt-in; requires
    /// `HERDR_ENV=1` and its pane variables, so it is inert outside
    /// Herdr). Default off.
    pub herdr: Option<bool>,
}

impl Sandbox {
    /// Convert to the sandbox crate's policy config.
    pub fn to_policy_config(&self) -> ka_sandbox::SandboxConfig {
        ka_sandbox::SandboxConfig {
            mode: self.mode.clone(),
            allow_write: self.allow_write.clone(),
        }
    }
}

/// Language-server diagnostics ([lsp]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Lsp {
    /// Enable diagnostics feedback (default false).
    pub enable: Option<bool>,
    /// language name → server command (stdio). Only configured
    /// languages spawn, e.g. { rust = "rust-analyzer", python =
    /// "pyright-langserver --stdio", typescript =
    /// "typescript-language-server --stdio" }.
    pub commands: Option<std::collections::BTreeMap<String, String>>,
    /// Act through the server, not just read: adds the `lsp_rename` and
    /// `lsp_actions` hands (Write tier). Default false.
    pub write_through: Option<bool>,
}

/// Prompt assembly ([prompt]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct PromptSettings {
    /// A file whose text replaces ka's built-in identity sentence in
    /// the system prompt (free-form instructions; re-read every turn,
    /// capped at 8 KiB). Ignored in safe mode.
    pub identity_file: Option<String>,
}

/// DAP debugging probe ([debug]): off by default — spawning debug
/// adapters is the same trust class as stdio MCP. Inert in --safe-mode.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Debug {
    /// Enable the `debug` hand (default false).
    pub enable: Option<bool>,
    /// Adapter overrides: name → stdio launch command, merged over the
    /// embedded catalog. Example: { codelldb = "/opt/codelldb/adapter" }.
    pub adapters: Option<std::collections::BTreeMap<String, String>>,
}

/// Post-edit verification ([verify]): the aider auto-lint/auto-test
/// loop. Lint commands run per edited file; the test command runs once
/// after a turn that edited files — a non-zero exit feeds the output
/// back to the model for one automatic fix round before surfacing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Verify {
    /// Test command (run in cwd, 15-minute cap). Examples: `cargo test`,
    /// `npm test`.
    pub test: Option<String>,
    /// Lint rules — first matching rule runs per edited file.
    pub lints: Vec<LintRule>,
}

/// One lint rule ([[verify.lints]]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LintRule {
    /// Glob matched against the edited file's relative path and its
    /// basename (`*.rs` matches `src/a.rs`).
    pub pattern: String,
    /// Command to run. `{file}` is substituted with the edited path;
    /// without the placeholder the path is appended. Examples:
    /// `rustfmt --check {file}`, `cargo fmt --check {file}`.
    pub command: String,
}

/// Auto-poke persistence ([pokes]). Most agent failures are early
/// exits, not wrong answers: when a turn settles while the model's own
/// todo list still has pending items, the harness continues the work
/// itself with a bounded auto-continuation round. Pokes stop on
/// non-Done stops (aborts, errors — wire retries have already run),
/// in plan mode, and when the pending set stops moving.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Pokes {
    /// Maximum auto-poke rounds per user prompt (0 disables; unset
    /// layers keep the lower layer, default 3).
    pub max: Option<u32>,
}

/// Context-window policy ([context]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Context {
    /// Auto-promote to a bigger-context sibling on overflow
    /// (None = default true; false opts out).
    pub promote: Option<bool>,
}

/// Fallback model chain ([fallback]). When a turn fails on the active
/// model with a provider/auth error after retries are exhausted, the
/// engine re-dispatches the same messages on the next entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Fallback {
    /// Fallback selectors (`vendor/model[:effort]`) in try-order; at
    /// most 2 hops are taken per turn.
    pub models: Vec<String>,
}

/// Runtime feature toggles ([features]). The same spec grammar as the
/// `--disable`/`--enable` CLI flags and the `/features` command.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Features {
    /// Specs to disable at startup: `agents`, `skills`, `mcp`, `hooks`,
    /// `web`, `lsp`, `debug`, or one item as `mcp:<server>`,
    /// `skill:<name>`, `tool:<hand>`, `agent:<name>`. Unknown specs are
    /// a hard startup error.
    pub disable: Vec<String>,
}

/// Engine configuration. All fields optional at the data level; resolution
/// order is applied by [`Config::overlay`] consumers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Default model selector (`vendor/model:effort`).
    pub model: Option<String>,
    /// Default reasoning effort.
    pub effort: Option<Effort>,
    /// Permission mode: `ask` | `accept_edits` | `allow` | `readonly`
    /// (defaults to allow when absent everywhere; the pre-v0.5 names
    /// `guarded`/`free`/`plan` still parse as aliases).
    pub mode: Option<Mode>,
    /// Role mappings.
    pub roles: Roles,
    /// Maximum tool-execution steps per prompt before forcing a text reply.
    #[serde(default)]
    pub max_steps: Option<u32>,
    /// Working directory override (default: process cwd).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Permission rules, evaluated first-match-wins before mode logic.
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// Tool-call hooks (exit-2 block contract).
    #[serde(default)]
    pub hooks: Vec<Hook>,
    /// MCP servers (stdio): tools appear as `<name>.<tool>` hands.
    #[serde(default)]
    pub mcp: Vec<crate::mcp::McpServerConfig>,
    /// Persistent tool allowlist ([permissions] allow).
    #[serde(default)]
    pub permissions: Permissions,
    /// Spend/context guard thresholds ([guards]; both default off).
    #[serde(default)]
    pub guards: Guards,
    /// Fallback model chain ([fallback]).
    #[serde(default)]
    pub fallback: Fallback,
    /// Self-update settings ([update]).
    #[serde(default)]
    pub update: Update,
    /// Context-window policy ([context]).
    #[serde(default)]
    pub context: Context,
    /// Web-search providers ([[search]]; first entry is active).
    #[serde(default)]
    pub search: Vec<SearchProvider>,
    /// Sandbox policy ([sandbox]).
    #[serde(default)]
    pub sandbox: Sandbox,
    /// Auto-poke persistence ([pokes]; default max 3, 0 disables).
    #[serde(default)]
    pub pokes: Pokes,
    /// Language-server diagnostics ([lsp]).
    #[serde(default)]
    pub lsp: Lsp,
    /// DAP debugging probe ([debug]).
    #[serde(default)]
    pub debug: Debug,
    /// TUI appearance ([tui]).
    #[serde(default)]
    pub tui: Tui,
    /// Per-tool settings ([tools]).
    #[serde(default)]
    pub tools: Tools,
    /// Git automation ([git]).
    #[serde(default)]
    pub git: Git,
    /// Post-edit verification ([verify]).
    #[serde(default)]
    pub verify: Verify,
    /// Runtime feature toggles ([features]).
    #[serde(default)]
    pub features: Features,
    /// Prompt assembly ([prompt]).
    #[serde(default)]
    pub prompt: PromptSettings,
}

impl Config {
    /// Parse one strict TOML layer. `origin` names the source in errors.
    pub fn parse_layer(text: &str, origin: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError::Parse {
            origin: origin.to_string(),
            message: e.to_string(),
        })
    }

    /// Apply `other` on top of `self`: set fields win, unset fields keep.
    pub fn overlay(&mut self, other: Config) {
        if other.model.is_some() {
            self.model = other.model;
        }
        if other.effort.is_some() {
            self.effort = other.effort;
        }
        if other.mode.is_some() {
            self.mode = other.mode;
        }
        if other.roles.default.is_some() {
            self.roles.default = other.roles.default;
        }
        if other.roles.fast.is_some() {
            self.roles.fast = other.roles.fast;
        }
        if other.max_steps.is_some() {
            self.max_steps = other.max_steps;
        }
        if other.cwd.is_some() {
            self.cwd = other.cwd;
        }
        if !other.rules.is_empty() {
            self.rules = other.rules;
        }
        if !other.permissions.allow.is_empty() {
            self.permissions.allow = other.permissions.allow;
        }
        if other.guards.context_pct.is_some() {
            self.guards.context_pct = other.guards.context_pct;
        }
        if !other.mcp.is_empty() {
            self.mcp = other.mcp;
        }
        if other.tools.bash.background_after_ms.is_some() {
            self.tools.bash.background_after_ms = other.tools.bash.background_after_ms;
        }
        if other.tools.read.max_image_mb.is_some() {
            self.tools.read.max_image_mb = other.tools.read.max_image_mb;
        }
        if !other.fallback.models.is_empty() {
            self.fallback.models = other.fallback.models;
        }
        if other.update.repo.is_some() {
            self.update.repo = other.update.repo;
        }
        if other.update.check.is_some() {
            self.update.check = other.update.check;
        }
        if other.context.promote.is_some() {
            self.context.promote = other.context.promote;
        }
        if !other.hooks.is_empty() {
            self.hooks = other.hooks;
        }
        if other.git.auto_commit.is_some() {
            self.git.auto_commit = other.git.auto_commit;
        }
        if other.lsp.enable.is_some() {
            self.lsp.enable = other.lsp.enable;
        }
        if other.lsp.commands.as_ref().is_some_and(|c| !c.is_empty()) {
            self.lsp.commands = other.lsp.commands;
        }
        if other.sandbox.mode.is_some() {
            self.sandbox.mode = other.sandbox.mode;
        }
        if !other.sandbox.allow_write.is_empty() {
            self.sandbox.allow_write = other.sandbox.allow_write;
        }
        if other.guards.auto_review.is_some() {
            self.guards.auto_review = other.guards.auto_review;
        }
        if other.tui.bell.is_some() {
            self.tui.bell = other.tui.bell;
        }
        if other.tui.notify.is_some() {
            self.tui.notify = other.tui.notify;
        }
        if other.tui.mouse.is_some() {
            self.tui.mouse = other.tui.mouse;
        }
        if other.tui.herdr.is_some() {
            self.tui.herdr = other.tui.herdr;
        }
        if other.tools.mcp.discovery.is_some() {
            self.tools.mcp.discovery = other.tools.mcp.discovery;
        }
        if other.verify.test.is_some() {
            self.verify.test = other.verify.test;
        }
        if other.pokes.max.is_some() {
            self.pokes.max = other.pokes.max;
        }
        if !other.verify.lints.is_empty() {
            self.verify.lints = other.verify.lints;
        }
        if other.guards.spend_usd.is_some() {
            self.guards.spend_usd = other.guards.spend_usd;
        }
        if !other.search.is_empty() {
            self.search = other.search;
        }
        if other.tools.web.allow_private_hosts.is_some() {
            self.tools.web.allow_private_hosts = other.tools.web.allow_private_hosts;
        }
        if other.tui.header_glyph.is_some() {
            self.tui.header_glyph = other.tui.header_glyph;
        }
        if !other.features.disable.is_empty() {
            self.features.disable = other.features.disable;
        }
        if other.prompt.identity_file.is_some() {
            self.prompt.identity_file = other.prompt.identity_file;
        }
    }
    /// Effective step cap (default 20).
    pub fn effective_max_steps(&self) -> u32 {
        self.max_steps.unwrap_or(20)
    }

    /// Effective bash auto-background threshold (default 30000, 0 = off).
    pub fn effective_bash_background_after_ms(&self) -> u64 {
        self.tools
            .bash
            .background_after_ms
            .unwrap_or(DEFAULT_BACKGROUND_AFTER_MS)
    }
    /// The title glyph (default ◆).
    pub fn effective_header_glyph(&self) -> String {
        self.tui
            .header_glyph
            .clone()
            .unwrap_or_else(|| "\u{25c6}".to_string())
    }

    /// Whether the bell rings on turn completion and asks (default true).
    pub fn effective_bell(&self) -> bool {
        self.tui.bell.unwrap_or(true)
    }

    /// Whether the TUI starts with the mouse captured. Default is
    /// capture (unset or `"capture"`): clickable tool cards, jump
    /// arrows and strip buttons, wheel scroll at line granularity,
    /// ↑/↓ recall history, and plain left-drag over the transcript
    /// selects rows and copies them to the clipboard (OSC 52, toast
    /// confirms); ⇧drag still selects natively on every major
    /// terminal. `[tui] mouse = "native"` opts out entirely (plain
    /// drag select/paste, wheel via alternate-scroll, keyboard-only
    /// affordances — though overlay clicks still work: tracking turns
    /// on while a popup or modal is open).
    pub fn effective_mouse_capture(&self) -> bool {
        self.tui.mouse.as_deref() != Some("native")
    }

    /// Whether web fetches may target private hosts (default false).
    pub fn effective_web_allow_private(&self) -> bool {
        self.tools.web.allow_private_hosts.unwrap_or(false)
    }

    /// The active search provider, if any.
    pub fn effective_search(&self) -> Option<SearchProvider> {
        self.search.first().cloned()
    }

    /// Whether overflow promotion is enabled (default true).
    pub fn effective_context_promote(&self) -> bool {
        self.context.promote.unwrap_or(true)
    }

    /// Effective read-hand image cap in MB (default 5, 0 = unlimited).
    pub fn effective_max_image_mb(&self) -> u32 {
        self.tools.read.max_image_mb.unwrap_or(DEFAULT_MAX_IMAGE_MB)
    }

    /// Whether MCP tool discovery is lazy (`[tools.mcp] discovery`).
    pub fn effective_mcp_lazy(&self) -> bool {
        matches!(self.tools.mcp.discovery.as_deref(), Some("lazy"))
    }

    /// The effective permission mode (free unless set in a layer).
    pub fn effective_mode(&self) -> Mode {
        self.mode.unwrap_or(Mode::Allow)
    }

    /// Effective auto-poke rounds per user prompt (default 3; 0
    /// disables — the documented opt-out must survive layering).
    pub fn effective_pokes_max(&self) -> u32 {
        self.pokes.max.unwrap_or(3)
    }

    /// JSON schema for editor integration (`ka config schema`).
    pub fn schema_json() -> Result<String, serde_json::Error> {
        let schema = schemars::schema_for!(Config);
        serde_json::to_string_pretty(&schema)
    }
}

/// The project layer as a writer needs it: `Some(default)` when the
/// file does not exist yet, `None` when it exists but fails strict
/// parse (a writer must never silently default from — and then
/// overwrite — a file whose settings it could not read).
fn read_project_layer(path: &std::path::Path) -> Option<Config> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Config::default()),
        Err(_) => None,
        Ok(text) => Config::parse_layer(&text, "project").ok(),
    }
}

/// Append `tool` to `[permissions] allow` in the PROJECT config layer
/// (`<project root>/.ka/ka.toml` — the nearest `.git` ancestor of
/// `cwd`, else `cwd` itself), preserving all other keys. Already-listed
/// tools are a no-op (`None`). Best-effort: parse or write failures
/// return `None` rather than clobbering a file we could not read.
pub fn save_project_permission(cwd: &std::path::Path, tool: &str) -> Option<std::path::PathBuf> {
    let path = crate::project_root(cwd).join(".ka/ka.toml");
    let mut layer = read_project_layer(&path)?;
    if layer.permissions.allow.iter().any(|t| t == tool) {
        return None;
    }
    layer.permissions.allow.push(tool.to_string());
    let mut text = String::from("# ka project config — extended by an \"always\" permission\n\n");
    text.push_str(&toml::to_string_pretty(&layer).ok()?);
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(&path, text).ok()?;
    Some(path)
}

/// Append sandbox write grants to `[sandbox] allow_write` in the PROJECT
/// config layer, preserving all other keys (the "always" answer on a
/// sandbox-expansion ask). Already-listed paths are a no-op (`None`).
/// Best-effort like [`save_project_permission`]: failures return `None`
/// rather than clobbering a file we could not read — in particular an
/// existing layer that fails strict parse is left untouched (defaulting
/// from it would silently destroy the user's settings).
pub fn save_project_sandbox_grants(
    cwd: &std::path::Path,
    write_paths: &[std::path::PathBuf],
) -> Option<std::path::PathBuf> {
    let path = crate::project_root(cwd).join(".ka/ka.toml");
    let mut layer = read_project_layer(&path)?;
    let mut added = false;
    for p in write_paths {
        let text = p.display().to_string();
        if !layer.sandbox.allow_write.contains(&text) {
            layer.sandbox.allow_write.push(text);
            added = true;
        }
    }
    if !added {
        return None;
    }
    let mut text = String::from("# ka project config — extended by a sandbox \"always\" grant\n\n");
    text.push_str(&toml::to_string_pretty(&layer).ok()?);
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(&path, text).ok()?;
    Some(path)
}

/// The user config layer path (`~/.config/ka/ka.toml`, XDG-aware).
/// Upsert an API key into `~/.config/ka/.env` (created 0600) and the
/// live dotenv layer, so the key works immediately without a restart.
pub fn save_api_key(env_var: &str, value: &str) -> std::io::Result<std::path::PathBuf> {
    let path = user_config_path().with_file_name(".env");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let pattern = format!("{env_var}=");
    match lines
        .iter_mut()
        .find(|l| l.trim_start().starts_with(&pattern))
    {
        Some(slot) => *slot = format!("{env_var}={value}"),
        None => lines.push(format!("{env_var}={value}")),
    }
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::write(&path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    ka_dialect::auth::set_dotenv_key(env_var, value);
    Ok(path)
}

pub fn user_config_path() -> std::path::PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("ka/ka.toml")
}

/// Persist default settings to the user layer, preserving unrelated keys
/// (rules, hooks, roles) from an existing file. Returns the path written.
pub fn save_user_settings(
    model: Option<&str>,
    effort: Option<Effort>,
    mode: Option<Mode>,
    mouse: Option<&str>,
    features: Option<&[String]>,
) -> Result<std::path::PathBuf, String> {
    save_settings_to(&user_config_path(), model, effort, mode, mouse, features)
}

/// [`save_user_settings`] against an explicit path (tests, layers).
pub fn save_settings_to(
    path: &std::path::Path,
    model: Option<&str>,
    effort: Option<Effort>,
    mode: Option<Mode>,
    mouse: Option<&str>,
    features: Option<&[String]>,
) -> Result<std::path::PathBuf, String> {
    let mut layer = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| Config::parse_layer(&text, "user").ok())
        .unwrap_or_default();
    if model.is_some() {
        layer.model = model.map(str::to_string);
    }
    if effort.is_some() {
        layer.effort = effort;
    }
    if mode.is_some() {
        layer.mode = mode;
    }
    if let Some(mouse) = mouse {
        // strict-TOML philosophy at the writer too: only the two
        // documented values ever land in the layer (an unknown value
        // would parse back as capture silently)
        if mouse != "capture" && mouse != "native" {
            return Err(format!(
                "invalid [tui] mouse {mouse:?} — expected \"capture\" or \"native\""
            ));
        }
        layer.tui.mouse = Some(mouse.to_string());
    }
    if let Some(features) = features {
        layer.features.disable = features.to_vec();
    }
    let mut text = String::from("# ka user config — written by /settings\n\n");
    text.push_str(&toml::to_string_pretty(&layer).map_err(|e| e.to_string())?);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path.to_path_buf())
}

/// Configuration failure.
#[derive(Debug)]
pub enum ConfigError {
    /// A layer failed strict parsing.
    Parse {
        /// Which layer (path or description).
        origin: String,
        /// Parser detail, including TOML position.
        message: String,
    },
    /// A layer file could not be read.
    Io {
        /// Which layer.
        origin: String,
        /// I/O detail.
        message: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Parse { origin, message } => write!(f, "{origin}: {message}"),
            ConfigError::Io { origin, message } => write!(f, "{origin}: {message}"),
        }
    }
}
impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    #[test]
    fn project_layer_parse_failure_is_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("ka-cfg-sbx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ka")).unwrap();
        let path = dir.join(".ka/ka.toml");
        let broken = "model = \"a/x\"\nunknown_key = true\n";
        std::fs::write(&path, broken).unwrap();
        // strict parse fails: both writers must refuse (None) and the
        // user's file must survive byte-for-byte
        assert!(
            save_project_sandbox_grants(&dir, &[std::path::PathBuf::from("/opt/data")]).is_none()
        );
        assert!(save_project_permission(&dir, "bash").is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        // a missing layer starts from defaults and saves fine
        std::fs::remove_file(&path).unwrap();
        let saved = save_project_sandbox_grants(&dir, &[std::path::PathBuf::from("/opt/data")]);
        let path = saved.expect("missing layer saves");
        let layer = Config::parse_layer(&std::fs::read_to_string(&path).unwrap(), "check").unwrap();
        assert_eq!(layer.sandbox.allow_write, vec!["/opt/data".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_settings_preserves_unrelated_keys() {
        let dir = std::env::temp_dir().join(format!("ka-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ka.toml");
        std::fs::write(&path, "model = \"old/model\"\nmax_steps = 7\n").unwrap();
        save_settings_to(
            &path,
            Some("groq/llama-3.3-70b"),
            Some(Effort::High),
            Some(Mode::Allow),
            None,
            None,
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let layer = Config::parse_layer(&text, "saved").unwrap();
        assert_eq!(layer.model.as_deref(), Some("groq/llama-3.3-70b"));
        assert_eq!(layer.effort, Some(Effort::High));
        assert_eq!(layer.mode, Some(Mode::Allow));
        assert_eq!(layer.max_steps, Some(7), "unrelated keys preserved");
        assert_eq!(layer.tui.mouse, None, "mouse left alone when None");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_check_parses_strictly_and_defaults_on() {
        // default: the startup availability check is on
        let layer = Config::parse_layer("", "t").unwrap();
        assert_eq!(layer.update.effective_check(), UpdateCheck::Startup);
        assert_eq!(layer.update.check, None);
        // off parses; the effective policy follows
        let layer = Config::parse_layer("[update]\ncheck = \"off\"\n", "t").unwrap();
        assert_eq!(layer.update.check, Some(UpdateCheck::Off));
        assert_eq!(layer.update.effective_check(), UpdateCheck::Off);
        // explicit startup + repo ride along
        let layer =
            Config::parse_layer("[update]\ncheck = \"startup\"\nrepo = \"a/b\"\n", "t").unwrap();
        assert_eq!(layer.update.effective_check(), UpdateCheck::Startup);
        assert_eq!(layer.update.repo.as_deref(), Some("a/b"));
        // an overlay without `check` keeps the base policy
        let mut base = Config::parse_layer("[update]\ncheck = \"off\"\n", "t").unwrap();
        let over = Config::parse_layer("[update]\nrepo = \"x/y\"\n", "t").unwrap();
        base.overlay(over);
        assert_eq!(base.update.effective_check(), UpdateCheck::Off);
        // a typo'd value is a hard error (strict TOML, like every key)
        assert!(Config::parse_layer("[update]\ncheck = \"of\"\n", "t").is_err());
        // unknown keys in [update] stay hard errors
        assert!(Config::parse_layer("[update]\nchek = \"off\"\n", "t").is_err());
    }

    #[test]
    fn save_settings_persists_mouse_mode() {
        let dir = std::env::temp_dir().join(format!("ka-cfg-mouse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ka.toml");
        std::fs::write(&path, "model = \"a/x\"\n[tui]\nbell = false\n").unwrap();
        // /mouse toggled to native: the key lands under [tui], the
        // unrelated bell setting survives
        save_settings_to(&path, None, None, None, Some("native"), None).unwrap();
        let layer = Config::parse_layer(&std::fs::read_to_string(&path).unwrap(), "saved").unwrap();
        assert_eq!(layer.tui.mouse.as_deref(), Some("native"));
        assert_eq!(layer.tui.bell, Some(false), "unrelated [tui] keys survive");
        assert_eq!(layer.model.as_deref(), Some("a/x"));
        assert!(
            !layer.effective_mouse_capture(),
            "native parses back as uncaptured"
        );
        // toggling back rewrites the value (no [tui] duplication)
        save_settings_to(&path, None, None, None, Some("capture"), None).unwrap();
        let layer = Config::parse_layer(&std::fs::read_to_string(&path).unwrap(), "saved").unwrap();
        assert_eq!(layer.tui.mouse.as_deref(), Some("capture"));
        assert!(layer.effective_mouse_capture());
        // mouse=None is the model/mode-picker path: an existing mouse
        // choice (here "capture") must survive untouched
        save_settings_to(&path, Some("b/y"), None, None, None, None).unwrap();
        let layer = Config::parse_layer(&std::fs::read_to_string(&path).unwrap(), "saved").unwrap();
        assert_eq!(layer.tui.mouse.as_deref(), Some("capture"));
        assert_eq!(layer.model.as_deref(), Some("b/y"));
        // non-canonical values are rejected at the writer
        let err = save_settings_to(&path, None, None, None, Some("bogus"), None).unwrap_err();
        assert!(err.contains("invalid [tui] mouse"), "{err}");
        assert!(
            Config::parse_layer(&std::fs::read_to_string(&path).unwrap(), "check")
                .unwrap()
                .tui
                .mouse
                .as_deref()
                == Some("capture"),
            "failed save leaves the layer untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    #[test]
    fn defaults_are_empty() {
        let c = Config::default();
        assert_eq!(c.effective_mode(), Mode::Allow);
        assert!(c.model.is_none());
        assert!(c.effective_mouse_capture(), "capture is the default mode");
    }

    #[test]
    fn mode_defaults_to_allow_but_explicit_still_wins() {
        assert_eq!(Config::default().effective_mode(), Mode::Allow);
        let ask = Config::parse_layer("mode = \"ask\"\n", "user").unwrap();
        assert_eq!(ask.effective_mode(), Mode::Ask);
        // pre-v0.5 spelling keeps parsing (old configs, old strands)
        let legacy = Config::parse_layer("mode = \"guarded\"\n", "user").unwrap();
        assert_eq!(legacy.effective_mode(), Mode::Ask);
    }

    #[test]
    fn layer_overlay_set_fields_win() {
        let mut base = Config::parse_layer("model = \"a/x\"\nmode = \"ask\"\n", "base").unwrap();
        let over = Config::parse_layer("mode = \"allow\"\n", "over").unwrap();
        base.overlay(over);
        assert_eq!(base.model.as_deref(), Some("a/x"));
        assert_eq!(base.effective_mode(), Mode::Allow);
    }

    #[test]
    fn unknown_key_rejected() {
        let err = Config::parse_layer("modle = \"a/x\"\n", "user")
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("user:"), "got: {err}");
        assert!(
            err.contains("unknown field") || err.contains("modle"),
            "got: {err}"
        );
    }

    #[test]
    fn unknown_nested_key_rejected() {
        let err = Config::parse_layer("[roles]\ndefualt = \"a/x\"\n", "user")
            .unwrap_err()
            .to_string();
        assert!(err.contains("defualt"), "got: {err}");
    }
    #[test]
    fn roles_parse_overlay_and_land_in_schema() {
        // both role selectors parse
        let c = Config::parse_layer("[roles]\ndefault = \"a/x\"\nfast = \"b/y@low\"\n", "user")
            .unwrap();
        assert_eq!(c.roles.default.as_deref(), Some("a/x"));
        assert_eq!(c.roles.fast.as_deref(), Some("b/y@low"));
        // empty table is valid: roles default to None
        let empty = Config::parse_layer("[roles]\n", "user").unwrap();
        assert_eq!(empty.roles, Roles::default());
        // layered overlay: set fields win, unset fields keep
        let mut base =
            Config::parse_layer("[roles]\ndefault = \"a/x\"\nfast = \"b/y\"\n", "base").unwrap();
        let over = Config::parse_layer("[roles]\nfast = \"c/z\"\n", "over").unwrap();
        base.overlay(over);
        assert_eq!(base.roles.default.as_deref(), Some("a/x"));
        assert_eq!(base.roles.fast.as_deref(), Some("c/z"));
        // unknown keys under [roles] hard-error (typo safety)
        let err = Config::parse_layer("[roles]\nfassst = \"b/y\"\n", "user")
            .unwrap_err()
            .to_string();
        assert!(err.contains("fassst"), "got: {err}");
        // the editor schema exposes the roles table with both keys
        let schema = Config::schema_json().unwrap();
        assert!(schema.contains("\"Roles\""), "got: {schema}");
        let roles_part = schema.split("\"Roles\"").nth(1).expect("Roles definition");
        assert!(roles_part.contains("default"), "got: {roles_part}");
        assert!(roles_part.contains("fast"), "got: {roles_part}");
    }

    #[test]
    fn env_shaped_layer_parses() {
        // what the CLI builds from KA_MODEL/KA_MODE before overlaying
        let c =
            Config::parse_layer("model = \"openai/gpt-5.1\"\nmode = \"allow\"\n", "env").unwrap();
        assert_eq!(c.model.as_deref(), Some("openai/gpt-5.1"));
        assert_eq!(c.effective_mode(), Mode::Allow);
    }

    #[test]
    fn rules_parse_and_match_shape() {
        let c = Config::parse_layer(
            "[[rules]]\ntool = \"bash\"\npattern = \"cargo *\"\nverdict = \"allow\"\n\n[[rules]]\ntool = \"write\"\nverdict = \"deny\"\n",
            "user",
        )
        .unwrap();
        assert_eq!(c.rules.len(), 2);
        assert_eq!(c.rules[0].verdict, Verdict::Allow);
        assert_eq!(c.rules[1].pattern, None);
        assert_eq!(c.rules[1].verdict, Verdict::Deny);
        // unknown verdict rejected
        let bad = Config::parse_layer("[[rules]]\ntool = \"bash\"\nverdict = \"maybe\"\n", "user");
        assert!(bad.is_err());
    }

    #[test]
    fn hooks_parse() {
        let c = Config::parse_layer(
            "[[hooks]]\nevent = \"pre_tool_use\"\ntool = \"bash\"\ncommand = \"guard.sh\"\n",
            "user",
        )
        .unwrap();
        assert_eq!(c.hooks.len(), 1);
        assert_eq!(c.hooks[0].event, HookEvent::PreToolUse);
        assert_eq!(c.hooks[0].tool.as_deref(), Some("bash"));
        let stop = Config::parse_layer(
            "[[hooks]]\nevent = \"stop\"\ncommand = \"notify.sh\"\n",
            "user",
        )
        .unwrap();
        assert_eq!(stop.hooks[0].event, HookEvent::Stop);
        assert!(stop.hooks[0].tool.is_none());
        let bad = Config::parse_layer("[[hooks]]\nevent = \"whenever\"\ncommand = \"x\"\n", "u");
        assert!(bad.is_err());
    }

    #[test]
    fn lsp_parses_and_overlays() {
        let c = Config::parse_layer(
            "[lsp]\nenable = true\n[lsp.commands]\nrust = \"rust-analyzer\"\npython = \"pyright-langserver --stdio\"\n",
            "user",
        )
        .unwrap();
        assert_eq!(c.lsp.enable, Some(true));
        let commands = c.lsp.commands.as_ref().expect("commands");
        assert_eq!(commands.len(), 2);
        assert_eq!(commands["rust"], "rust-analyzer");
        // empty upper layer keeps the lower layer's table; set wins
        let mut base = c.clone();
        base.overlay(Config::parse_layer("[lsp]\nenable = false\n", "over").unwrap());
        assert_eq!(base.lsp.enable, Some(false));
        assert_eq!(base.lsp.commands.as_ref().expect("kept").len(), 2);
        base.overlay(Config::parse_layer("[lsp.commands]\nrust = \"ra\"\n", "over2").unwrap());
        assert_eq!(base.lsp.commands.as_ref().expect("replaced")["rust"], "ra");
        // unknown keys hard-error
        let err = Config::parse_layer("[lsp]\nenabled = true\n", "user")
            .unwrap_err()
            .to_string();
        assert!(err.contains("enabled"), "got: {err}");
    }

    #[test]
    fn sandbox_mode_overlays() {
        let mut base = Config::default();
        base.overlay(Config::parse_layer("[sandbox]\nmode = \"fs\"\n", "project").unwrap());
        assert_eq!(
            base.sandbox.to_policy_config().mode.as_deref(),
            Some("fs"),
            "project [sandbox] must survive the overlay merge"
        );
    }

    #[test]
    fn every_documented_setting_survives_the_overlay() {
        // regression for the overlay-drop class of bug (sandbox,
        // spend_usd, [[search]], tools.web, and [tui] were each silently
        // dropped at some point): parse one layer carrying every
        // previously-dropped knob and assert it reaches the merged cfg
        let mut cfg = Config::default();
        cfg.overlay(
            Config::parse_layer(
                "[guards]\nspend_usd = 2.5\ncontext_pct = 80\n\n[[search]]\nprovider = \"tavily\"\napi_key_env = \"TAVILY_KEY\"\n\n[tools.web]\nallow_private_hosts = true\n\n[tools.mcp]\ndiscovery = \"lazy\"\n\n[tui]\nheader_glyph = \"*\"\nbell = false\nnotify = \"notify-send ka done\"\nherdr = true\n\n[verify]\ntest = \"cargo test\"\n\n[[verify.lints]]\npattern = \"*.rs\"\ncommand = \"rustfmt --check {file}\"\n\n[features]\ndisable = [\"agents\", \"mcp:github\"]\n\n[prompt]\nidentity_file = \"/tmp/IDENTITY.md\"\n",
                "project",
            )
            .unwrap(),
        );
        assert_eq!(cfg.guards.spend_usd, Some(2.5), "spend guard is live");
        assert_eq!(cfg.guards.context_pct, Some(80));
        assert_eq!(cfg.search.len(), 1, "[[search]] provider survives");
        assert_eq!(
            cfg.tools.web.allow_private_hosts,
            Some(true),
            "web private-host policy survives"
        );
        assert_eq!(cfg.tui.header_glyph.as_deref(), Some("*"));
        // the newer knob classes survive the same way
        assert!(cfg.effective_mcp_lazy(), "[tools.mcp] discovery survives");
        assert!(!cfg.effective_bell(), "[tui] bell survives");
        assert_eq!(
            cfg.tui.notify.as_deref(),
            Some("notify-send ka done"),
            "[tui] notify survives"
        );
        assert_eq!(cfg.tui.herdr, Some(true), "[tui] herdr survives");
        assert_eq!(
            cfg.prompt.identity_file.as_deref(),
            Some("/tmp/IDENTITY.md"),
            "[prompt] identity_file survives"
        );
        assert_eq!(
            cfg.verify.test.as_deref(),
            Some("cargo test"),
            "[verify] test survives"
        );
        assert_eq!(cfg.verify.lints.len(), 1, "[[verify.lints]] survive");
        assert_eq!(
            cfg.features.disable,
            vec!["agents".to_string(), "mcp:github".to_string()],
            "[features] disable survives the overlay"
        );
    }

    #[test]
    fn schema_emits() {
        let schema = Config::schema_json().unwrap();
        assert!(schema.contains("\"Config\""), "got: {schema}");
    }

    #[test]
    fn bash_background_threshold_defaults_overlays_and_rejects_unknown() {
        // default 30000 when absent everywhere
        assert_eq!(
            Config::default().effective_bash_background_after_ms(),
            DEFAULT_BACKGROUND_AFTER_MS
        );
        // set + layered overlay (set fields win)
        let mut base =
            Config::parse_layer("[tools.bash]\nbackground_after_ms = 1000\n", "base").unwrap();
        assert_eq!(base.effective_bash_background_after_ms(), 1000);
        let over =
            Config::parse_layer("[tools.bash]\nbackground_after_ms = 250\n", "over").unwrap();
        base.overlay(over);
        assert_eq!(base.effective_bash_background_after_ms(), 250);
        // unset upper layer keeps the lower layer's value
        let lower =
            Config::parse_layer("[tools.bash]\nbackground_after_ms = 1000\n", "lower").unwrap();
        let mut merged = Config::default();
        merged.overlay(lower);
        assert_eq!(merged.effective_bash_background_after_ms(), 1000);
        // unknown keys still hard-error
        let err = Config::parse_layer("[tools.bash]\nbackground_after_sec = 1\n", "user")
            .unwrap_err()
            .to_string();
        assert!(err.contains("background_after_sec"), "got: {err}");
        let err = Config::parse_layer("[tools]\nbashh = {}\n", "user")
            .unwrap_err()
            .to_string();
        assert!(err.contains("bashh"), "got: {err}");
    }

    #[test]
    fn schema_contains_background_after_ms() {
        let schema = Config::schema_json().unwrap();
        assert!(schema.contains("background_after_ms"), "got: {schema}");
    }

    #[test]
    fn verify_parses_and_overlays() {
        let c = Config::parse_layer(
            "[verify]\ntest = \"cargo test\"\n\n[[verify.lints]]\npattern = \"*.rs\"\ncommand = \"rustfmt --check {file}\"\n",
            "user",
        )
        .unwrap();
        assert_eq!(c.verify.test.as_deref(), Some("cargo test"));
        assert_eq!(c.verify.lints.len(), 1);
        assert_eq!(c.verify.lints[0].pattern, "*.rs");
        // set-field overlay: test replaced, empty lints keep lower layer
        let mut base = c;
        base.overlay(Config::parse_layer("[verify]\ntest = \"npm test\"\n", "over").unwrap());
        assert_eq!(base.verify.test.as_deref(), Some("npm test"));
        assert_eq!(base.verify.lints.len(), 1, "lints survive an unset layer");
        base.overlay(
            Config::parse_layer(
                "[[verify.lints]]\npattern = \"*.ts\"\ncommand = \"tsc --noEmit\"\n",
                "over2",
            )
            .unwrap(),
        );
        assert_eq!(base.verify.lints.len(), 1, "non-empty lints replace");
        assert_eq!(base.verify.lints[0].pattern, "*.ts");
        // unknown keys hard-error
        let err = Config::parse_layer("[verify]\ntests = \"x\"\n", "user")
            .unwrap_err()
            .to_string();
        assert!(err.contains("tests"), "got: {err}");
        let err = Config::parse_layer("[[verify.lints]]\npattern = \"*.rs\"\n", "user")
            .unwrap_err()
            .to_string();
        assert!(err.contains("command"), "got: {err}");
    }

    #[test]
    fn pokes_default_parse_overlay_schema() {
        // default: 3 rounds per user prompt
        assert_eq!(Config::default().effective_pokes_max(), 3);
        let c = Config::parse_layer("[pokes]\nmax = 0\n", "user").unwrap();
        assert_eq!(c.pokes.max, Some(0));
        // a bare [pokes] header parses (serde default on the table)
        assert_eq!(
            Config::parse_layer("[pokes]\n", "user").unwrap().pokes.max,
            None
        );
        // layering: an upper layer that sets max wins; an unset layer
        // keeps the lower value (the disable must survive layering)
        let mut base = c;
        base.overlay(Config::default());
        assert_eq!(
            base.effective_pokes_max(),
            0,
            "explicit 0 survives an unset layer"
        );
        base.overlay(Config::parse_layer("[pokes]\nmax = 1\n", "proj").unwrap());
        assert_eq!(base.effective_pokes_max(), 1, "upper layer wins");
        // unknown keys hard-error
        let err = Config::parse_layer("[pokes]\nmaximum = 1\n", "user")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unknown field") && err.contains("maximum"),
            "{err}"
        );
        // strict [pokes] lands in the JSON schema
        let schema = Config::schema_json().unwrap();
        assert!(schema.contains("pokes"), "got: {schema}");
    }
}
