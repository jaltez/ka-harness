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

/// One hook: a shell command or an HTTP handler run around tool calls
/// or at turn end. `command` and `url` are mutually exclusive — exactly
/// one must be configured (a config-parse error otherwise).
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
    #[serde(default)]
    pub command: String,
    /// Per-hook timeout in seconds (EXT-02). Default
    /// [`DEFAULT_HOOK_TIMEOUT_SECS`]; hard-capped at
    /// [`MAX_HOOK_TIMEOUT_SECS`] (a config-parse error outside 1–600).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Fire-and-forget (EXT-02, `async = true` in TOML): the hook runs
    /// detached, its exit (or failure) surfaces as a transcript note,
    /// and it can never block the turn — exit 2 does not block, stdout
    /// steering is ignored, the result is discarded.
    #[serde(rename = "async", default)]
    pub async_: Option<bool>,
    /// HTTP handler (EXT-02): the same JSON envelope a command hook
    /// receives on stdin is POSTed to this URL; the JSON response body
    /// follows the same exit/steering contract (`decision: "block"`
    /// with `reason` blocks like exit 2, `decision: "allow"`
    /// pre-approves, `mode`/`note`/`updated_input`/`feedback` steer).
    /// A non-2xx status or a network error is a hook failure.
    #[serde(default)]
    pub url: Option<String>,
}

/// Default per-hook timeout in seconds.
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 10;
/// Hard cap for `timeout_secs` (a chatty hook must not pin a turn for
/// minutes).
pub const MAX_HOOK_TIMEOUT_SECS: u64 = 600;

impl Hook {
    /// The effective timeout: `timeout_secs` when set (the config
    /// validator already bounds it), else the 10 s default.
    pub fn effective_timeout_secs(&self) -> u64 {
        self.timeout_secs
            .unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS)
            .min(MAX_HOOK_TIMEOUT_SECS)
    }

    /// Whether this hook is fire-and-forget.
    pub fn is_async(&self) -> bool {
        self.async_ == Some(true)
    }
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
    /// A `delegate` spawn started (EXT-01): `{"event":"subagent_start",
    /// "agent":…,"task":…}`. Advisory.
    SubagentStart,
    /// A delegate run finished (EXT-01): `{"event":"subagent_stop",
    /// "agent":…,"task":…,"status":"finished"|"failed","summary":…}`.
    /// Advisory; cancelled tasks do not fire it.
    SubagentStop,
    /// A `SetModel` is about to apply (EXT-01):
    /// `{"event":"pre_model_switch","from":…,"to":…}`. Advisory; cannot
    /// veto the switch.
    PreModelSwitch,
    /// A `SetModel` applied (EXT-01): `{"event":"post_model_switch",
    /// "from":…,"to":…}`. Advisory.
    PostModelSwitch,
    /// A digest/compaction finished (EXT-01): `{"event":"post_compact",
    /// "tokens_before":…,"tokens_after":…}`. Advisory.
    PostCompact,
    /// A permission ask is about to be posed (EXT-01):
    /// `{"event":"permission_request","tool":…,"argument":…}`. Fires
    /// BEFORE the ask and is advisory only — it can never block,
    /// answer, or replace the ask.
    PermissionRequest,
    /// A permission decision refused a call (EXT-01): a deny-class gate
    /// verdict or the user answering deny on the ask —
    /// `{"event":"permission_denied","tool":…,"reason":…}`. Advisory.
    PermissionDenied,
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
            HookEvent::SubagentStart => "subagent_start",
            HookEvent::SubagentStop => "subagent_stop",
            HookEvent::PreModelSwitch => "pre_model_switch",
            HookEvent::PostModelSwitch => "post_model_switch",
            HookEvent::PostCompact => "post_compact",
            HookEvent::PermissionRequest => "permission_request",
            HookEvent::PermissionDenied => "permission_denied",
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

/// Ask-vs-deny policy for reviewable permission asks
/// (`[permissions] prompt_policy`). The default asks; `deny` refuses
/// every reviewable ask outright (headless serve/CI runs stay
/// unattended without `--dangerously`). Hardstops, protected paths and
/// rule-authored asks are NOT reviewable — they keep their own course.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum PromptPolicy {
    /// Pose reviewable asks to the surface (the default).
    #[default]
    Ask,
    /// Deny every reviewable ask (`prompt policy denies: …`).
    Deny,
}

/// Which clearance tiers the `[guards] auto_review` fast-role reviewer
/// pre-screens (`[guards] auto_review_tier`).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum AutoReviewTier {
    /// Only exec-tier asks (the default).
    #[default]
    Exec,
    /// Exec-tier asks plus reviewable write-tier asks.
    #[serde(rename = "exec+write")]
    ExecWrite,
}

/// One named permission profile (`[permissions.profiles.<name>]`):
/// a reusable rule set a `[permissions] profile = "<name>"` activation
/// puts ahead of the global `[[rules]]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct PermissionProfile {
    /// Rules of this profile (same shape as `[[rules]]`).
    pub rules: Vec<Rule>,
    /// Inherit another profile's rules (resolved through arbitrary
    /// chains at activation; a cycle or unknown name is a config
    /// error). The extending profile's own rules sort FIRST — a child
    /// overrides its parent under first-match-wins.
    pub extends: Option<String>,
}

/// Persistent tool allowlist (`[permissions] allow = [...]` in a ka.toml
/// layer). Listed tools skip the permission ask for the rest of the
/// session; picking "always" on an ask appends here (project layer).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Permissions {
    /// Tool names auto-allowed without asking.
    pub allow: Vec<String>,
    /// Activate a named profile from `profiles` (None = no profile).
    /// Unknown names are a config error at parse time.
    pub profile: Option<String>,
    /// Named rule sets (`[permissions.profiles.<name>]`). The active
    /// profile's rules evaluate BEFORE `[[rules]]` (more specific
    /// first, CFG-03-coherent).
    pub profiles: std::collections::BTreeMap<String, PermissionProfile>,
    /// Ask-vs-deny policy for reviewable asks (default ask).
    pub prompt_policy: Option<PromptPolicy>,
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
    /// Which tiers auto_review pre-screens: `"exec"` (default) or
    /// `"exec+write"` (reviewable write-tier asks — the plain
    /// "modify files?" asks — also pass the reviewer, same
    /// auto-allow-only contract).
    pub auto_review_tier: Option<AutoReviewTier>,
    /// Opt-in second opinion on flagged web results (default false):
    /// when true, the `fast`-role model classifies every web result the
    /// always-on deterministic heuristic flagged and its verdict rides
    /// the warning. The heuristic itself is always on; this only arms
    /// the classifier. Inert in --safe-mode.
    pub injection_check: Option<bool>,
}

/// Shell environment policy ([shell]): what environment bash (and
/// monitor-spawned) children inherit. Default `all` — today's
/// behavior, the full parent environment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Shell {
    /// `"all"` (default: inherit everything), `"core"` (only the
    /// core shell vars: PATH/HOME/SHELL/TERM/TMPDIR/USER/LOGNAME/LANG)
    /// or `"none"` (empty environment).
    pub env: Option<ShellEnvMode>,
    /// Extra vars to pass through under `"core"`/`"none"` (names, not
    /// values — the parent's value for each name is inherited). Under
    /// `"all"` this is a no-op.
    pub env_keep: Vec<String>,
    /// Vars injected into every child regardless of mode, overriding
    /// inherited values (`env_set = { CARGO_TERM_COLOR = "always" }`).
    pub env_set: std::collections::BTreeMap<String, String>,
}

/// `[shell] env` values.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ShellEnvMode {
    /// The child inherits the full parent environment (the default).
    #[default]
    All,
    /// Only the core shell vars (plus `env_keep`/`env_set`).
    Core,
    /// An empty environment (plus `env_keep`/`env_set`).
    None,
}

/// The vars a `"core"` environment always carries.
pub const CORE_ENV_VARS: &[&str] = &[
    "PATH", "HOME", "SHELL", "TERM", "TMPDIR", "USER", "LOGNAME", "LANG",
];

/// A resolved [shell] environment policy, ready to apply to child
/// processes (bash hand, monitor spawns).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellEnvPolicy {
    /// Inheritance mode.
    pub mode: ShellEnvMode,
    /// Extra passthrough var names (`env_keep`).
    pub keep: Vec<String>,
    /// Injected vars (`env_set`), applied last so they win.
    pub set: std::collections::BTreeMap<String, String>,
}

impl ShellEnvPolicy {
    /// Apply to a tokio child command (bash hand).
    pub fn apply_to(&self, cmd: &mut tokio::process::Command) {
        match self.mode {
            ShellEnvMode::All => {}
            ShellEnvMode::Core => {
                cmd.env_clear();
                for name in CORE_ENV_VARS {
                    if let Some(v) = std::env::var_os(name) {
                        cmd.env(name, v);
                    }
                }
            }
            ShellEnvMode::None => {
                cmd.env_clear();
            }
        }
        if self.mode != ShellEnvMode::All {
            for name in &self.keep {
                if let Some(v) = std::env::var_os(name) {
                    cmd.env(name, v);
                }
            }
        }
        for (k, v) in &self.set {
            cmd.env(k, v);
        }
    }

    /// Apply to a std child command (monitor spawns).
    pub fn apply_to_std(&self, cmd: &mut std::process::Command) {
        match self.mode {
            ShellEnvMode::All => {}
            ShellEnvMode::Core => {
                cmd.env_clear();
                for name in CORE_ENV_VARS {
                    if let Some(v) = std::env::var_os(name) {
                        cmd.env(name, v);
                    }
                }
            }
            ShellEnvMode::None => {
                cmd.env_clear();
            }
        }
        if self.mode != ShellEnvMode::All {
            for name in &self.keep {
                if let Some(v) = std::env::var_os(name) {
                    cmd.env(name, v);
                }
            }
        }
        for (k, v) in &self.set {
            cmd.env(k, v);
        }
    }
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
///
/// EXT-11 TOML shape: the two built-in names stay flat
/// (`[roles] default = "…"`, `fast = "…"`); every *additional* role
/// lives in an explicit sub-table — `[roles.extra.review] =
/// "vendor/model@high"`. An explicit table (rather than
/// `#[serde(flatten)]`) keeps typo rejection intact: an unknown key
/// directly under `[roles]` is a hard config error, while
/// `roles.extra.<anything>` is a role. Session side: `--model @review`
/// and `SetModel "@review"` resolve through this table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Roles {
    /// Selector for the default (main) model.
    pub default: Option<String>,
    /// Selector for the fast (cheap) role.
    pub fast: Option<String>,
    /// Named roles (`[roles.extra.<name>] = "selector"`), resolved by
    /// `@<name>` selectors.
    #[serde(default)]
    pub extra: std::collections::BTreeMap<String, String>,
}

impl Roles {
    /// Resolve an `@<role>` selector to its configured selector.
    /// `@default`/`@fast` read the built-ins; any other name reads
    /// [`Roles::extra`]. Unknown roles resolve to `None` (callers fall
    /// back to the literal selector, the pre-EXT-11 behavior).
    pub fn resolve(&self, selector: &str) -> Option<&str> {
        let name = selector.strip_prefix('@')?;
        if name.is_empty() {
            return None;
        }
        match name {
            "default" => self.default.as_deref(),
            "fast" => self.fast.as_deref(),
            other => self.extra.get(other).map(String::as_str),
        }
    }
}

/// EXT-11 router strategy for `[virtual_models.<name>]`.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RouterStrategy {
    /// Each turn takes the next selector in `models`, wrapping around.
    #[default]
    RoundRobin,
    /// Each turn starts at `models[0]`; a provider/auth/network failure
    /// hops to the next entry (the built-in fallback chain, capped at
    /// 2 hops per turn like `[fallback]`).
    Failover,
}

/// EXT-11 virtual model (`[virtual_models.<name>]`): a named router
/// over several selectors. Addressed as `virtual/<name>` (or
/// `@virtual/<name>`) from `--model`, `SetModel`, or a role mapping;
/// the concrete model is picked per turn by
/// [`VirtualModel::strategy`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct VirtualModel {
    /// Member selectors (`vendor/model[@effort]`), in try-order.
    pub models: Vec<String>,
    /// [`RouterStrategy::RoundRobin`] (the default) or
    /// [`RouterStrategy::Failover`].
    pub strategy: Option<RouterStrategy>,
}

impl VirtualModel {
    /// The effective strategy (default round-robin).
    pub fn effective_strategy(&self) -> RouterStrategy {
        self.strategy.unwrap_or_default()
    }
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

/// `[tools.mcp] discovery`: how server tools reach the model.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum McpDiscovery {
    /// Every server tool becomes its own hand in context (the default).
    #[default]
    Eager,
    /// One `mcp_call` hand stands in for all of them — the model lists
    /// a server's tools on demand. Prefer lazy when servers expose many
    /// tools.
    Lazy,
}

/// MCP tool tuning (`[tools.mcp]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct McpTools {
    /// `"eager"` (default) or `"lazy"`; see [`McpDiscovery`].
    pub discovery: Option<McpDiscovery>,
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

/// One web-search provider (`[[search]]`; entries form a fallback
/// chain — the first entry that fails (no key, network error, parse
/// error) falls through to the next). Layered arrays keep the more
/// specific layer's entries first, so a project `[[search]]` is tried
/// before the user's.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct SearchProvider {
    /// `tavily` | `brave` | `bing` (keyed) | `duckduckgo` | `searxng`
    /// (keyless).
    pub provider: String,
    /// Env var holding the API key. Required for tavily/brave/bing;
    /// ignored by the keyless duckduckgo/searxng backends.
    pub api_key_env: Option<String>,
    /// API base override (fixture tests, proxies). REQUIRED for
    /// `searxng`: the instance URL to query.
    pub base_url: Option<String>,
    /// Optional `[[search.sites]]` entries: each appends `site:<site>`
    /// to this provider's queries.
    pub sites: Vec<SearchSite>,
}

/// One `[[search.sites]]` entry. v1 handlers are site-scoping only —
/// no per-site scraping logic.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct SearchSite {
    /// Host to scope to (`docs.rs` → `site:docs.rs` in the query).
    pub site: String,
    /// Handler id. Only `auto` (plain site-scoping) exists in v1;
    /// anything else is a per-provider search error.
    pub handler: Option<String>,
}

/// Image generation (`[generate_image]`). GATED: the `generate_image`
/// hand registers only when `provider` names a supported provider AND
/// its API key resolves (`api_key_env` override, else the provider's
/// default env ladder) — an unkeyed config keeps the tool hidden.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct GenerateImage {
    /// `gemini` | `openai` | `grok`. Absent = the hand stays unregistered.
    pub provider: Option<String>,
    /// Env var holding the API key. Defaults per provider:
    /// gemini → `GEMINI_API_KEY`; openai → `OPENAI_API_KEY`;
    /// grok → `GROK_API_KEY`, falling back to `XAI_API_KEY`.
    pub api_key_env: Option<String>,
    /// Model id. Defaults per provider (see
    /// `ImageProvider::default_model`).
    pub model: Option<String>,
    /// Where generated images are saved. Default `<cwd>/.ka/images`.
    pub output_dir: Option<String>,
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

/// `[tui] mouse`: terminal mouse handling.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum MouseMode {
    /// SGR button reporting (the default): wheel scrolls, bottom-strip
    /// buttons are clickable, ⇧drag still selects. Ctrl+M toggles at
    /// runtime.
    #[default]
    Capture,
    /// No capture at all: plain drag selects and pastes with the
    /// terminal's own bindings, the wheel scrolls via alternate-scroll.
    /// kitty has no alternate-scroll — keep `capture` there.
    Native,
}

/// TUI appearance and notifications ([tui]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Tui {
    /// Title glyph for the transcript window (default ◆).
    pub header_glyph: Option<String>,
    /// Theme name: `.ka/themes/<name>.toml` (project, trust-gated) or
    /// `~/.config/ka/themes/<name>.toml` (user), TOML with `color` and
    /// optional `symbols` tables over the palette tokens. Unknown = the
    /// built-in Nocturne palette. `/theme` switches live.
    pub theme: Option<String>,
    /// Set the terminal title (tab/window label) to
    /// `ka · <session title>` and update it as the session state
    /// changes. None = true; `false` keeps your title untouched.
    pub title: Option<bool>,
    /// Status-line segments in draw order. Recognized ids: `model`,
    /// `mode`, `session`, `effort`, `context`, `cost`, `tokens`,
    /// `cache`, `toggles`, `update`. None = the built-in order; unknown
    /// ids are a hard config error.
    pub status_line: Option<Vec<String>>,
    /// Which events notify: subset of `turn_finished` | `ask`
    /// (default: both — exactly today's bell/notify behavior).
    pub notify_on: Option<Vec<String>>,
    /// Only notify when the terminal is unfocused (focus reporting via
    /// CSI u / DECSET 1004, where the terminal supports it). None =
    /// false (always notify, like today).
    pub notify_unfocused: Option<bool>,
    /// Ring the terminal bell when a turn finishes or a permission ask
    /// appears (None = true; terminals mute bells by user choice).
    pub bell: Option<bool>,
    /// Command run when a turn finishes, JSON on stdin:
    /// `{"event":"turn_finished","stop":"done"}`. Example:
    /// `notify-send ka "turn done"`.
    pub notify: Option<String>,
    /// Mouse mode: `"capture"` (default) — SGR button reporting, so
    /// the wheel scrolls, the bottom-strip buttons are clickable and
    /// ⇧drag still selects; or `"native"` — no capture at all, plain
    /// drag selects and pastes with the terminal's own bindings and
    /// the wheel scrolls via alternate-scroll. Ctrl+M toggles at
    /// runtime either way. kitty has no alternate-scroll — keep
    /// `"capture"` there for the wheel.
    pub mouse: Option<MouseMode>,
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
    /// capped at 8 KiB). Ignored in safe mode. Precedence over
    /// `identity_inline`.
    pub identity_file: Option<String>,
    /// Inline text replacing ka's built-in identity sentence, for
    /// setups that keep instructions in the config (capped at 8 KiB,
    /// same as `identity_file`). Used when no `identity_file` is
    /// configured (or it is unreadable); otherwise the built-in
    /// identity applies. Ignored in safe mode.
    pub identity_inline: Option<String>,
}

/// Model pins ([[model_pins]]): a per-directory model override. The
/// FIRST pin whose `path` glob matches the session cwd wins and
/// overwrites the effective `model`; when nothing matches the
/// configured model stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPin {
    /// Glob matched against the session working directory — the full
    /// path, or any `/`-boundary suffix of it (`projects/ka/*` matches
    /// `/home/me/projects/ka/sub`). `*` spans path separators, like
    /// everywhere else in ka.
    pub path: String,
    /// Model selector (`vendor/model[@effort]`) applied when the glob
    /// matches.
    pub model: String,
}

/// Strand persistence policy ([history]).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields, default)]
pub struct History {
    /// Whether the session persists at all: `"all"` (default) keeps
    /// the normal record-by-record strand file; `"none"` = the session
    /// leaves no persistent trace — nothing is written for the
    /// strand's lifetime, resume/digest persistence included.
    pub persistence: Option<HistoryPersistence>,
    /// Byte ceiling for the strand: once the persisted strand exceeds
    /// it, persistence stops for the rest of the session (the session
    /// keeps working in memory). `None` = unlimited.
    pub max_bytes: Option<u64>,
}

/// `[history] persistence` values.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum HistoryPersistence {
    /// Persist every record as usual (the default).
    #[default]
    All,
    /// The session leaves no persistent trace: the strand is kept in
    /// memory only.
    None,
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
    /// CTX-12: auto-compact trigger as a percent of the context window —
    /// the digest fires once the meter reaches it (1–100). `None` keeps
    /// the reserve-based default: digest when the free space drops below
    /// max(15% of the window, 16k tokens). Settable live for the session
    /// through the protocol's `SetAutoCompact` command.
    pub autocompact_pct: Option<u64>,
    /// CTX-07: minutes of surface inactivity after which the idle task
    /// runs (recap + optional compaction). `None` (default) disables the
    /// idle task entirely.
    pub idle_minutes: Option<u64>,
    /// CTX-07: minimum idle minutes before the idle recap fires. `None`
    /// inherits `idle_minutes` (the recap fires on the first idle tick
    /// past the idle threshold). Ignored when `idle_minutes` is unset.
    pub recap_minutes: Option<u64>,
    /// CTX-07: idle compaction threshold, as a percent of the context
    /// window — while idle past `idle_minutes`, the digest also fires
    /// when the meter reaches this. `None` disables idle compaction.
    pub idle_compact_pct: Option<u64>,
    /// CTX-08: run a one-shot repository prewalk on the first prompt
    /// of a fresh strand — a cheap bounded walk (depth 2, 200 entries)
    /// plus a fast-role brief (≤5 bullets) injected into the context
    /// before the first turn. `None`/false = off; `--prewalk` wins
    /// over the file value (the integrator overlays it).
    pub prewalk: Option<bool>,
    /// MODEL-05: cache warming in the idle tick. `"off"` (default)
    /// never touches the provider cache; `"idle"` re-sends the current
    /// history with `max_tokens = 1` while the surface is quiet — new
    /// turns since the last warm and a last-turn cost ≥ $0.05 gate it —
    /// refreshing the provider cache TTL without emitting any turn
    /// events. Only fires when the active dialect's `cache` is not
    /// `off`.
    pub cache_warming: Option<String>,
}

/// The cache-warming modes [`Context::cache_warming`] accepts.
pub const CACHE_WARMING_MODES: [&str; 2] = ["off", "idle"];

/// Fallback model chain ([fallback]). When a turn fails on the active
/// model with a provider/auth error after retries are exhausted, the
/// engine re-dispatches the same messages on the next entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Fallback {
    /// Fallback selectors (`vendor/model[@effort]`) in try-order; at
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
    /// Positive allowlist of hands: when set (non-empty), ONLY these
    /// hands are offered — everything else is invisible to the model
    /// and rejected by name at call time (fail-closed, like
    /// `disable`). Entries are hand names (`read`, `bash`,
    /// `github__create_issue`) or a whole server as `mcp:<server>`
    /// (admits that server's tools and the lazy `mcp_call` front
    /// hand). `disable` wins over `allow`.
    pub allow: Vec<String>,
}

/// Engine configuration. All fields optional at the data level; resolution
/// order is applied by [`Config::overlay`] consumers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Default model selector (`vendor/model[@effort]`).
    pub model: Option<String>,
    /// Default reasoning effort.
    pub effort: Option<Effort>,
    /// MODEL-03: `service_tier` hint passed through verbatim to
    /// providers that carry the field (anthropic_messages,
    /// openai_chat, openai_responses requests include it only when
    /// set; ka fixes no values). `None` = the field stays absent.
    pub service_tier: Option<String>,
    /// Permission mode: `ask` | `accept_edits` | `allow` | `readonly` |
    /// `dont_ask` (defaults to allow when absent everywhere; the
    /// pre-v0.5 names `guarded`/`free`/`plan` still parse as aliases).
    /// `dont_ask` refuses every ask-class action instead of prompting
    /// (hardstops keep asking).
    pub mode: Option<Mode>,
    /// Role mappings.
    pub roles: Roles,
    /// EXT-11 model routers ([virtual_models.<name>]): selector
    /// `virtual/<name>` picks a member per turn (round-robin or
    /// failover).
    #[serde(default)]
    pub virtual_models: std::collections::BTreeMap<String, VirtualModel>,
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
    /// MCP servers (stdio): tools appear as `<name>__<tool>` hands.
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
    /// Web-search provider fallback chain ([[search]]; entries tried in
    /// order, first success wins; `[[search.sites]]` scopes queries).
    #[serde(default)]
    pub search: Vec<SearchProvider>,
    /// Image generation ([generate_image]; gated on provider + key).
    #[serde(default)]
    pub generate_image: GenerateImage,
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
    /// Shell environment policy ([shell]).
    #[serde(default)]
    pub shell: Shell,
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
    /// Key remapping ([keybindings]): action id → key chord
    /// (`\"session.new\" = \"ctrl+n\"` — quote action ids that carry dots,
    /// TOML dotted-key syntax otherwise applies). Chords are lowercase modifier
    /// chains (`ctrl`, `alt`, `shift`, `super`) plus a key name
    /// (`letters`, `f1`…, `enter`, `esc`, `tab`, …) joined by `+`.
    /// Values are validated at startup: unknown action ids or chords
    /// are a hard config error.
    #[serde(default)]
    pub keybindings: Option<std::collections::BTreeMap<String, String>>,
    /// Strand persistence policy ([history]).
    #[serde(default)]
    pub history: History,
    /// Model pins ([[model_pins]]): the first pin whose `path` glob
    /// matches the session cwd overwrites `model`.
    #[serde(default)]
    pub model_pins: Vec<ModelPin>,
    /// Additional instruction filenames conventions search per
    /// directory when it carries no `AGENTS.md` (after the built-in
    /// `CLAUDE.md` fallback), e.g. `["CONTEXT.md", ".cursorrules"]`.
    #[serde(default)]
    pub project_doc_fallback_filenames: Vec<String>,
}

impl Config {
    /// Parse one strict TOML layer. `origin` names the source in errors.
    pub fn parse_layer(text: &str, origin: &str) -> Result<Self, ConfigError> {
        let layer: Self = toml::from_str(text).map_err(|e| ConfigError::Parse {
            origin: origin.to_string(),
            message: e.to_string(),
        })?;
        layer.validate().map_err(|message| ConfigError::Parse {
            origin: origin.to_string(),
            message,
        })?;
        Ok(layer)
    }

    /// Cross-field value validation for keys whose VALUES must be from
    /// a known set but whose shape is a plain list of strings
    /// (strict-TOML covers the keys; this covers the vocabulary).
    fn validate(&self) -> Result<(), String> {
        const STATUS_SEGMENTS: &[&str] = &[
            "model", "mode", "session", "effort", "context", "cost", "tokens", "cache", "toggles",
            "update",
        ];
        if let Some(segments) = &self.tui.status_line {
            for segment in segments {
                if !STATUS_SEGMENTS.contains(&segment.as_str()) {
                    return Err(format!(
                        "[tui] status_line: unknown segment {segment:?} (expected one of: {})",
                        STATUS_SEGMENTS.join(", ")
                    ));
                }
            }
        }
        const NOTIFY_EVENTS: &[&str] = &["turn_finished", "ask"];
        if let Some(events) = &self.tui.notify_on {
            for event in events {
                if !NOTIFY_EVENTS.contains(&event.as_str()) {
                    return Err(format!(
                        "[tui] notify_on: unknown event {event:?} (expected one of: {})",
                        NOTIFY_EVENTS.join(", ")
                    ));
                }
            }
        }
        // [permissions] profile graph: unknown extends names and
        // cycles are config errors (strict-TOML philosophy extends to
        // the profile graph, not just the keys), active or not; the
        // activation name itself must exist
        for (name, profile) in &self.permissions.profiles {
            if let Some(parent) = &profile.extends {
                if !self.permissions.profiles.contains_key(parent) {
                    return Err(format!(
                        "[permissions] profiles.{name}: extends unknown profile {parent:?}"
                    ));
                }
            }
        }
        for name in self.permissions.profiles.keys() {
            let mut visited = vec![name.clone()];
            let mut cursor = name.clone();
            while let Some(parent) = self
                .permissions
                .profiles
                .get(&cursor)
                .and_then(|p| p.extends.clone())
            {
                if visited.contains(&parent) {
                    return Err(format!(
                        "[permissions] profiles: extends cycle {} -> {parent}",
                        visited.join(" -> ")
                    ));
                }
                visited.push(parent.clone());
                cursor = parent;
            }
        }
        if let Some(active) = &self.permissions.profile {
            if !self.permissions.profiles.contains_key(active) {
                return Err(format!(
                    "[permissions] profile: unknown profile {active:?} (defined: {})",
                    self.permissions
                        .profiles
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        // [[hooks]] EXT-02: command/url are mutually exclusive (exactly
        // one handler per hook) and timeout_secs stays in 1..=600
        for hook in &self.hooks {
            let event = hook.event.wire_name();
            match (hook.command.trim().is_empty(), hook.url.is_some()) {
                (true, false) => {
                    return Err(format!(
                        "[[hooks]] {event}: one of command or url is required"
                    ));
                }
                (false, true) => {
                    return Err(format!(
                        "[[hooks]] {event}: set either command or url, not both"
                    ));
                }
                _ => {}
            }
            if let Some(secs) = hook.timeout_secs {
                if secs == 0 || secs > crate::config::MAX_HOOK_TIMEOUT_SECS {
                    return Err(format!(
                        "[[hooks]] {event}: timeout_secs {secs} is out of range \
                         (1-{})",
                        crate::config::MAX_HOOK_TIMEOUT_SECS
                    ));
                }
            }
        }
        // [virtual_models] EXT-11: each router needs at least one
        // member selector, and members must parse as selectors so a
        // typo fails at startup, not mid-turn
        for (name, router) in &self.virtual_models {
            if router.models.is_empty() {
                return Err(format!(
                    "[virtual_models.{name}]: at least one entry in models is required"
                ));
            }
            for selector in &router.models {
                if ka_dialect::parse_selector(selector).is_err() {
                    return Err(format!(
                        "[virtual_models.{name}]: models entry {selector:?} is not a \
                         model selector (vendor/model[@effort])"
                    ));
                }
            }
        }
        // [context] cache_warming (MODEL-05): the mode is a closed set
        // ("off" default | "idle") — a typo'd mode must not silently
        // disable warming
        if let Some(mode) = &self.context.cache_warming {
            if !crate::config::CACHE_WARMING_MODES.contains(&mode.as_str()) {
                return Err(format!(
                    "[context] cache_warming: unknown mode {mode:?} (expected one of: {})",
                    crate::config::CACHE_WARMING_MODES.join(", ")
                ));
            }
        }
        Ok(())
    }

    /// Apply `other` on top of `self`: set fields win, unset fields keep.
    pub fn overlay(&mut self, other: Config) {
        if other.model.is_some() {
            self.model = other.model;
        }
        if other.effort.is_some() {
            self.effort = other.effort;
        }
        if other.service_tier.is_some() {
            self.service_tier = other.service_tier;
        }
        if other.mode.is_some() {
            self.mode = other.mode;
        }
        if other.context.cache_warming.is_some() {
            self.context.cache_warming = other.context.cache_warming;
        }
        if other.roles.default.is_some() {
            self.roles.default = other.roles.default;
        }
        if other.roles.fast.is_some() {
            self.roles.fast = other.roles.fast;
        }
        if !other.roles.extra.is_empty() {
            // named roles merge by name; the upper layer wins
            for (name, selector) in other.roles.extra {
                self.roles.extra.insert(name, selector);
            }
        }
        if !other.virtual_models.is_empty() {
            // routers merge by name; the upper layer wins
            for (name, router) in other.virtual_models {
                self.virtual_models.insert(name, router);
            }
        }
        if other.max_steps.is_some() {
            self.max_steps = other.max_steps;
        }
        if other.cwd.is_some() {
            self.cwd = other.cwd;
        }
        if !other.rules.is_empty() {
            // arrays merge across layers (CFG-03): the more specific
            // layer's rules sort FIRST — ka evaluates first-match-wins,
            // so a project deny beats a user allow
            let mut merged = other.rules;
            merged.append(&mut self.rules);
            self.rules = merged;
        }
        if !other.permissions.allow.is_empty() {
            Self::merge_dedupe(&mut self.permissions.allow, &other.permissions.allow);
        }
        if other.permissions.profile.is_some() {
            self.permissions.profile = other.permissions.profile;
        }
        if !other.permissions.profiles.is_empty() {
            // union by name; the upper layer wins a name collision
            let mut merged = other.permissions.profiles;
            for (name, profile) in std::mem::take(&mut self.permissions.profiles) {
                merged.entry(name).or_insert(profile);
            }
            self.permissions.profiles = merged;
        }
        if other.permissions.prompt_policy.is_some() {
            self.permissions.prompt_policy = other.permissions.prompt_policy;
        }
        if other.guards.context_pct.is_some() {
            self.guards.context_pct = other.guards.context_pct;
        }
        if !other.mcp.is_empty() {
            // union by name; the upper layer wins a name collision
            let names: Vec<&str> = other.mcp.iter().map(|m| m.name.as_str()).collect();
            let lower = self
                .mcp
                .drain(..)
                .filter(|m| !names.contains(&m.name.as_str()))
                .collect::<Vec<_>>();
            let mut merged = other.mcp;
            merged.extend(lower);
            self.mcp = merged;
        }
        if other.tools.bash.background_after_ms.is_some() {
            self.tools.bash.background_after_ms = other.tools.bash.background_after_ms;
        }
        if other.tools.read.max_image_mb.is_some() {
            self.tools.read.max_image_mb = other.tools.read.max_image_mb;
        }
        if !other.fallback.models.is_empty() {
            // try-order is priority order: the more specific layer first
            let mut merged = other.fallback.models.clone();
            Self::merge_dedupe(&mut merged, &self.fallback.models);
            self.fallback.models = merged;
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
        if other.context.autocompact_pct.is_some() {
            self.context.autocompact_pct = other.context.autocompact_pct;
        }
        if other.context.idle_minutes.is_some() {
            self.context.idle_minutes = other.context.idle_minutes;
        }
        if other.context.recap_minutes.is_some() {
            self.context.recap_minutes = other.context.recap_minutes;
        }
        if other.context.idle_compact_pct.is_some() {
            self.context.idle_compact_pct = other.context.idle_compact_pct;
        }
        if other.context.prewalk.is_some() {
            self.context.prewalk = other.context.prewalk;
        }
        if !other.hooks.is_empty() {
            self.hooks.extend(other.hooks);
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
        if other.lsp.write_through.is_some() {
            self.lsp.write_through = other.lsp.write_through;
        }
        if other.debug.enable.is_some() {
            self.debug.enable = other.debug.enable;
        }
        if other.debug.adapters.as_ref().is_some_and(|a| !a.is_empty()) {
            self.debug.adapters = other.debug.adapters;
        }
        if other.sandbox.mode.is_some() {
            self.sandbox.mode = other.sandbox.mode;
        }
        if !other.sandbox.allow_write.is_empty() {
            Self::merge_dedupe(&mut self.sandbox.allow_write, &other.sandbox.allow_write);
        }
        if other.guards.auto_review.is_some() {
            self.guards.auto_review = other.guards.auto_review;
        }
        if other.guards.auto_review_tier.is_some() {
            self.guards.auto_review_tier = other.guards.auto_review_tier;
        }
        if other.guards.injection_check.is_some() {
            self.guards.injection_check = other.guards.injection_check;
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
        if other.shell.env.is_some() {
            self.shell.env = other.shell.env;
        }
        if !other.shell.env_keep.is_empty() {
            Self::merge_dedupe(&mut self.shell.env_keep, &other.shell.env_keep);
        }
        if !other.shell.env_set.is_empty() {
            let mut merged = other.shell.env_set;
            for (k, v) in std::mem::take(&mut self.shell.env_set) {
                merged.entry(k).or_insert(v);
            }
            self.shell.env_set = merged;
        }
        if other.verify.test.is_some() {
            self.verify.test = other.verify.test;
        }
        if other.pokes.max.is_some() {
            self.pokes.max = other.pokes.max;
        }
        if !other.verify.lints.is_empty() {
            self.verify.lints.extend(other.verify.lints);
        }
        if other.guards.spend_usd.is_some() {
            self.guards.spend_usd = other.guards.spend_usd;
        }
        if !other.search.is_empty() {
            // the first active provider wins: the more specific layer
            // sorts first
            let mut merged = other.search;
            merged.append(&mut self.search);
            self.search = merged;
        }
        if other.tools.web.allow_private_hosts.is_some() {
            self.tools.web.allow_private_hosts = other.tools.web.allow_private_hosts;
        }
        if other.tui.header_glyph.is_some() {
            self.tui.header_glyph = other.tui.header_glyph;
        }
        if other.tui.theme.is_some() {
            self.tui.theme = other.tui.theme;
        }
        if other.tui.title.is_some() {
            self.tui.title = other.tui.title;
        }
        if other
            .tui
            .status_line
            .as_ref()
            .is_some_and(|s| !s.is_empty())
        {
            self.tui.status_line = other.tui.status_line;
        }
        if other.tui.notify_on.as_ref().is_some_and(|s| !s.is_empty()) {
            self.tui.notify_on = other.tui.notify_on;
        }
        if other.tui.notify_unfocused.is_some() {
            self.tui.notify_unfocused = other.tui.notify_unfocused;
        }
        if other.keybindings.as_ref().is_some_and(|k| !k.is_empty()) {
            let mut merged = other.keybindings.clone().unwrap_or_default();
            if let Some(base) = &mut self.keybindings {
                for (k, v) in base.iter() {
                    merged.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
            self.keybindings = Some(merged);
        }
        if !other.features.disable.is_empty() {
            Self::merge_dedupe(&mut self.features.disable, &other.features.disable);
        }
        if other.prompt.identity_file.is_some() {
            self.prompt.identity_file = other.prompt.identity_file;
        }
        if other.prompt.identity_inline.is_some() {
            self.prompt.identity_inline = other.prompt.identity_inline;
        }
        if other.history.persistence.is_some() {
            self.history.persistence = other.history.persistence;
        }
        if other.history.max_bytes.is_some() {
            self.history.max_bytes = other.history.max_bytes;
        }
        if !other.model_pins.is_empty() {
            // first-match priority: the more specific layer's pins first
            let mut merged = other.model_pins;
            merged.append(&mut self.model_pins);
            self.model_pins = merged;
        }
        if !other.project_doc_fallback_filenames.is_empty() {
            // fallback names are tried in order: the more specific layer
            // sorts first
            let mut merged = other.project_doc_fallback_filenames.clone();
            Self::merge_dedupe(&mut merged, &self.project_doc_fallback_filenames);
            self.project_doc_fallback_filenames = merged;
        }
        if !other.features.allow.is_empty() {
            Self::merge_dedupe(&mut self.features.allow, &other.features.allow);
        }
    }

    /// Merge-dedupe helper for the accumulate-style array keys
    /// (CFG-03): upper-layer entries append in order, exact duplicates
    /// stay single.
    fn merge_dedupe<T: PartialEq + Clone>(base: &mut Vec<T>, upper: &[T]) {
        for item in upper {
            if !base.contains(item) {
                base.push(item.clone());
            }
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
        self.tui.mouse.unwrap_or_default() == MouseMode::Capture
    }

    /// Whether web fetches may target private hosts (default false).
    pub fn effective_web_allow_private(&self) -> bool {
        self.tools.web.allow_private_hosts.unwrap_or(false)
    }

    /// The search provider fallback chain (every `[[search]]` entry, in
    /// layer order — most specific first). Empty = the web_search hand
    /// stays unregistered.
    pub fn effective_search(&self) -> Vec<SearchProvider> {
        self.search.clone()
    }

    /// The resolved image-generation config, `None` while the hand must
    /// stay unregistered: no provider named, an unsupported provider
    /// string, or no API key resolvable for it.
    pub fn effective_generate_image(
        &self,
        cwd: &std::path::Path,
    ) -> Option<crate::hands::generate_image::ResolvedImageGen> {
        crate::hands::generate_image::resolve(
            self.generate_image.provider.as_deref(),
            self.generate_image.api_key_env.as_deref(),
            self.generate_image.model.clone(),
            self.generate_image.output_dir.clone(),
            self.generate_image.base_url.clone(),
            ka_dialect::auth::resolve_token,
            cwd,
        )
    }

    /// Whether overflow promotion is enabled (default true).
    pub fn effective_context_promote(&self) -> bool {
        self.context.promote.unwrap_or(true)
    }

    /// MODEL-05: the cache-warming mode, "off" unless set to "idle"
    /// (validated parse-side; unknown modes never parse).
    pub fn effective_cache_warming(&self) -> &str {
        self.context.cache_warming.as_deref().unwrap_or("off")
    }

    /// Effective read-hand image cap in MB (default 5, 0 = unlimited).
    pub fn effective_max_image_mb(&self) -> u32 {
        self.tools.read.max_image_mb.unwrap_or(DEFAULT_MAX_IMAGE_MB)
    }

    /// Whether MCP tool discovery is lazy (`[tools.mcp] discovery`).
    pub fn effective_mcp_lazy(&self) -> bool {
        self.tools.mcp.discovery.unwrap_or_default() == McpDiscovery::Lazy
    }

    /// The effective permission mode (free unless set in a layer).
    pub fn effective_mode(&self) -> Mode {
        self.mode.unwrap_or(Mode::Allow)
    }

    /// Effective ask-vs-deny policy for reviewable asks (default ask).
    pub fn effective_prompt_policy(&self) -> PromptPolicy {
        self.permissions.prompt_policy.unwrap_or_default()
    }

    /// The gate-order rule list: the active profile's rules (resolved
    /// through its `extends` chain, own rules first) followed by the
    /// global `[[rules]]`. Profiles are more specific than global
    /// rules, so they sort ahead (first-match-wins). Callers without a
    /// parsed-and-validated config (the engine always validates) get a
    /// cycle-safe best effort: a cycle simply stops inheritance.
    pub fn effective_rules(&self) -> Vec<Rule> {
        let mut rules = Vec::new();
        if let Some(active) = &self.permissions.profile {
            let mut visited = vec![active.clone()];
            let mut cursor = active.clone();
            while let Some(profile) = self.permissions.profiles.get(&cursor) {
                rules.extend(profile.rules.iter().cloned());
                let Some(parent) = profile.extends.clone() else {
                    break;
                };
                if visited.contains(&parent) {
                    break;
                }
                visited.push(parent.clone());
                cursor = parent;
            }
        }
        rules.extend(self.rules.iter().cloned());
        rules
    }

    /// Effective [shell] environment policy (default: inherit all).
    pub fn effective_shell_env(&self) -> ShellEnvPolicy {
        ShellEnvPolicy {
            mode: self.shell.env.unwrap_or_default(),
            keep: self.shell.env_keep.clone(),
            set: self.shell.env_set.clone(),
        }
    }

    /// Effective auto-poke rounds per user prompt (default 3; 0
    /// disables — the documented opt-out must survive layering).
    pub fn effective_pokes_max(&self) -> u32 {
        self.pokes.max.unwrap_or(3)
    }

    /// Effective strand persistence (`[history] persistence`; default
    /// all).
    pub fn effective_history_persistence(&self) -> HistoryPersistence {
        self.history.persistence.unwrap_or_default()
    }

    /// Effective strand byte ceiling (`[history] max_bytes`; None =
    /// unlimited).
    pub fn effective_history_max_bytes(&self) -> Option<u64> {
        self.history.max_bytes
    }

    /// The pinned model for `cwd` ([[model_pins]]): the FIRST pin
    /// whose `path` glob matches wins; `None` = no pin matches (the
    /// configured `model` stands). Engine bootstrap applies this over
    /// `model` before any consumer reads it.
    pub fn effective_model_for(&self, cwd: &std::path::Path) -> Option<String> {
        self.model_pins
            .iter()
            .find(|pin| pin_path_matches(&pin.path, cwd))
            .map(|pin| pin.model.clone())
    }

    /// JSON schema for editor integration (`ka config schema`).
    pub fn schema_json() -> Result<String, serde_json::Error> {
        let schema = schemars::schema_for!(Config);
        serde_json::to_string_pretty(&schema)
    }
}

/// First matching rule's verdict for a call — the exact matching the
/// gate uses (shipped semantics): the FIRST rule naming the tool is
/// decisive; when its pattern doesn't match the call's primary
/// argument, NO rule applies (a pattern-less rule always matches).
/// Web tools match the URL host with domain semantics. Public so
/// `ka rules check <command>` can pre-flight a call against the same
/// list the engine will evaluate.
pub fn evaluate_rules(rules: &[Rule], tool: &str, arg: &str) -> Option<Verdict> {
    rules
        .iter()
        .find(|r| r.tool == tool)
        .filter(|r| match &r.pattern {
            None => true,
            Some(pat) => crate::voice::rule_pattern_matches(pat, tool, arg),
        })
        .map(|r| r.verdict)
}

/// Does a [[model_pins]] glob match the session cwd? Full-path match,
/// or any `/`-boundary suffix of the path (`projects/ka/*` matches
/// `/home/me/projects/ka/sub`). Globs reuse the engine's `*`/`?`
/// matcher, where `*` spans path separators.
fn pin_path_matches(glob: &str, cwd: &std::path::Path) -> bool {
    let path = cwd.to_string_lossy().replace('\\', "/");
    if crate::voice::glob_match(glob, &path) {
        return true;
    }
    path.match_indices('/')
        .skip(1)
        .any(|(i, _)| crate::voice::glob_match(glob, &path[i + 1..]))
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
        let mode = match mouse {
            "capture" => MouseMode::Capture,
            "native" => MouseMode::Native,
            other => {
                return Err(format!(
                    "invalid [tui] mouse {other:?} — expected \"capture\" or \"native\""
                ));
            }
        };
        layer.tui.mouse = Some(mode);
    }
    if let Some(features) = features {
        layer.features.disable = features.to_vec();
    }
    // editor support: the JSON schema lands next to the toml (sibling
    // `<stem>.schema.json`) and a `#:schema` directive — already a TOML
    // comment — is prepended as the first line so editors pick the
    // sidecar up. Regenerated from scratch on every write, so the
    // directive never duplicates across saves.
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("ka");
    let schema_name = format!("{stem}.schema.json");
    let schema = Config::schema_json().map_err(|e| format!("schema: {e}"))?;
    let mut text = format!("#:schema {schema_name}\n");
    text.push_str("# ka user config — written by /settings\n\n");
    text.push_str(&toml::to_string_pretty(&layer).map_err(|e| e.to_string())?);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    let schema_path = path.with_file_name(schema_name);
    std::fs::write(&schema_path, schema)
        .map_err(|e| format!("write {}: {e}", schema_path.display()))?;
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
    fn context_keys_parse_and_overlay() {
        let base = Config::parse_layer(
            "[context]\nautocompact_pct = 80\nidle_minutes = 10\nidle_compact_pct = 90\n",
            "user",
        )
        .unwrap();
        assert_eq!(base.context.autocompact_pct, Some(80));
        assert_eq!(base.context.idle_minutes, Some(10));
        assert_eq!(base.context.idle_compact_pct, Some(90));
        // recap_minutes absent → None (inherits idle_minutes at runtime)
        assert_eq!(base.context.recap_minutes, None);

        // a more specific layer overrides only what it sets
        let mut merged = base.clone();
        merged.overlay(
            Config::parse_layer(
                "[context]\nautocompact_pct = 65\nrecap_minutes = 3\n",
                "project",
            )
            .unwrap(),
        );
        assert_eq!(merged.context.autocompact_pct, Some(65));
        assert_eq!(merged.context.recap_minutes, Some(3));
        assert_eq!(
            merged.context.idle_minutes,
            Some(10),
            "unset keys keep the lower layer"
        );

        // defaults: everything off
        let defaults = Config::default();
        assert_eq!(defaults.context.autocompact_pct, None);
        assert_eq!(defaults.context.idle_minutes, None);
        assert_eq!(defaults.context.recap_minutes, None);
        assert_eq!(defaults.context.idle_compact_pct, None);
        // MODEL-05/03: warming off by default, service tier unset
        assert_eq!(defaults.context.cache_warming, None);
        assert_eq!(
            defaults.effective_cache_warming(),
            "off",
            "warming default is off"
        );
        assert_eq!(defaults.service_tier, None);

        // out-of-band keys stay a parse error (deny_unknown_fields)
        assert!(Config::parse_layer("[context]\nnonsense = 1\n", "x").is_err());
    }

    /// MODEL-05: `[context] cache_warming` is a closed mode set, and
    /// the overlay layers compose.
    #[test]
    fn cache_warming_modes_parse_overlay_and_validate() {
        let cfg = Config::parse_layer(
            "[context]\nidle_minutes = 5\ncache_warming = \"idle\"\n",
            "user",
        )
        .unwrap();
        assert_eq!(cfg.effective_cache_warming(), "idle");
        // "off" parses explicitly
        let off = Config::parse_layer("[context]\ncache_warming = \"off\"\n", "u").unwrap();
        assert_eq!(off.effective_cache_warming(), "off");
        // layered overlay: a later layer turns it back off
        let mut merged = cfg;
        merged.overlay(
            Config::parse_layer("[context]\ncache_warming = \"off\"\n", "project").unwrap(),
        );
        assert_eq!(merged.effective_cache_warming(), "off");
        // an unknown mode is a hard config error naming the value
        let err = Config::parse_layer("[context]\ncache_warming = \"eager\"\n", "u")
            .unwrap_err()
            .to_string();
        assert!(err.contains("eager"), "error names the bad mode: {err}");
    }

    /// MODEL-03: top-level `service_tier` parses, overlays and stays a
    /// literal pass-through (ka fixes no values).
    #[test]
    fn service_tier_parses_and_overlays() {
        let base = Config::parse_layer("service_tier = \"flex\"\n", "user").unwrap();
        assert_eq!(base.service_tier.as_deref(), Some("flex"));
        // an arbitrary provider-specific value passes through unchanged
        let base = Config::parse_layer("service_tier = \"priority\"\n", "user").unwrap();
        assert_eq!(base.service_tier.as_deref(), Some("priority"));
        // layered: a later layer overrides, an unset layer keeps
        let mut merged = base;
        merged.overlay(Config::parse_layer("service_tier = \"standard\"\n", "p").unwrap());
        assert_eq!(merged.service_tier.as_deref(), Some("standard"));
        merged.overlay(Config::parse_layer("model = \"x/y\"\n", "p2").unwrap());
        assert_eq!(
            merged.service_tier.as_deref(),
            Some("standard"),
            "unset upper layer keeps the lower value"
        );
    }

    #[test]
    fn schema_carries_lote11_keys() {
        let schema = Config::schema_json().unwrap();
        assert!(schema.contains("service_tier"), "got: {schema}");
        assert!(schema.contains("cache_warming"), "got: {schema}");
    }

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
        // /settings mouse toggle to native: the key lands under [tui],
        // the unrelated bell setting survives
        save_settings_to(&path, None, None, None, Some("native"), None).unwrap();
        let layer = Config::parse_layer(&std::fs::read_to_string(&path).unwrap(), "saved").unwrap();
        assert_eq!(layer.tui.mouse, Some(MouseMode::Native));
        assert_eq!(layer.tui.bell, Some(false), "unrelated [tui] keys survive");
        assert_eq!(layer.model.as_deref(), Some("a/x"));
        assert!(
            !layer.effective_mouse_capture(),
            "native parses back as uncaptured"
        );
        // toggling back rewrites the value (no [tui] duplication)
        save_settings_to(&path, None, None, None, Some("capture"), None).unwrap();
        let layer = Config::parse_layer(&std::fs::read_to_string(&path).unwrap(), "saved").unwrap();
        assert_eq!(layer.tui.mouse, Some(MouseMode::Capture));
        assert!(layer.effective_mouse_capture());
        // mouse=None is the model/mode-picker path: an existing mouse
        // choice (here "capture") must survive untouched
        save_settings_to(&path, Some("b/y"), None, None, None, None).unwrap();
        let layer = Config::parse_layer(&std::fs::read_to_string(&path).unwrap(), "saved").unwrap();
        assert_eq!(layer.tui.mouse, Some(MouseMode::Capture));
        assert_eq!(layer.model.as_deref(), Some("b/y"));
        // non-canonical values are rejected at the writer
        let err = save_settings_to(&path, None, None, None, Some("bogus"), None).unwrap_err();
        assert!(err.contains("invalid [tui] mouse"), "{err}");
        assert!(
            Config::parse_layer(&std::fs::read_to_string(&path).unwrap(), "check")
                .unwrap()
                .tui
                .mouse
                == Some(MouseMode::Capture),
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

    /// CFG-03: arrays merge across layers. Priority-ordered keys
    /// (rules, model_pins, search, fallbacks, fallback filenames) put
    /// the more specific layer's entries FIRST; accumulate keys
    /// (hooks, allow lists, allow_write, lints, features) append with
    /// dedupe; mcp unions by name with the upper layer winning.
    #[test]
    fn arrays_merge_across_layers_with_priority_order() {
        let user = Config::parse_layer(
            "[[rules]]\ntool = \"bash\"\nverdict = \"allow\"\n\n[[search]]\nprovider = \"brave\"\napi_key_env = \"B\"\n\n[permissions]\nallow = [\"read\", \"bash\"]\n\n[features]\ndisable = [\"skills\"]\n\n[[mcp]]\nname = \"github\"\ncommand = \"old\"\n\n[[mcp]]\nname = \"local\"\ncommand = \"keep\"\n",
            "user",
        )
        .unwrap();
        let project = Config::parse_layer(
            "[[rules]]\ntool = \"bash\"\npattern = \"cargo *\"\nverdict = \"deny\"\n\n[[search]]\nprovider = \"tavily\"\napi_key_env = \"T\"\n\n[permissions]\nallow = [\"bash\"]\n\n[features]\ndisable = [\"agents\"]\n\n[[mcp]]\nname = \"github\"\ncommand = \"new\"\n",
            "project",
        )
        .unwrap();
        let mut cfg = user;
        cfg.overlay(project);
        // rules: the project (more specific) layer sorts first, so its
        // deny is evaluated before the user's blanket allow
        assert_eq!(cfg.rules.len(), 2);
        assert_eq!(cfg.rules[0].verdict, Verdict::Deny);
        assert_eq!(cfg.rules[1].verdict, Verdict::Allow);
        // search: project tavily wins the first-active slot
        assert_eq!(cfg.search[0].provider, "tavily");
        assert_eq!(cfg.search.len(), 2);
        // allow lists: union with dedupe
        assert_eq!(
            cfg.permissions.allow,
            vec!["read".to_string(), "bash".to_string()]
        );
        // features.disable: union with dedupe
        assert_eq!(
            cfg.features.disable,
            vec!["skills".to_string(), "agents".to_string()]
        );
        // mcp: union by name — the project's github definition wins,
        // the user's local server survives
        assert_eq!(cfg.mcp.len(), 2);
        assert_eq!(cfg.mcp[0].name, "github");
        assert_eq!(cfg.mcp[0].command.as_deref(), Some("new"));
        assert_eq!(cfg.mcp[1].name, "local");
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

    /// Lote 8 (TOOL-14): keyless backends need no api_key_env, and
    /// `[[search.sites]]` rides inside its `[[search]]` entry. Strict
    /// TOML still rejects unknown keys at every level, including the
    /// new sites table.
    #[test]
    fn keyless_search_and_sites_parse_strictly() {
        let layer = Config::parse_layer(
            "[[search]]\nprovider = \"duckduckgo\"\n\n[[search.sites]]\nsite = \"docs.rs\"\n\n[[search]]\nprovider = \"searxng\"\nbase_url = \"https://searx.example.org\"\n",
            "user",
        )
        .unwrap();
        assert_eq!(layer.search.len(), 2);
        let ddg = &layer.search[0];
        assert_eq!(ddg.provider, "duckduckgo");
        assert!(ddg.api_key_env.is_none(), "keyless backends omit the key");
        assert_eq!(ddg.sites.len(), 1);
        assert_eq!(ddg.sites[0].site, "docs.rs");
        assert_eq!(ddg.sites[0].handler, None, "handler defaults to auto");
        assert_eq!(
            layer.search[1].base_url.as_deref(),
            Some("https://searx.example.org")
        );
        // sites entries are strict too
        let err = Config::parse_layer(
            "[[search]]\nprovider = \"duckduckgo\"\n[[search.sites]]\nsite = \"docs.rs\"\nscraper = true\n",
            "user",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("unknown field") || err.contains("scraper"),
            "{err}"
        );
        // and so are providers
        let err = Config::parse_layer("[[search]]\nprovider = \"ddg\"\nkay = \"X\"\n", "user")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unknown field") || err.contains("kay"),
            "{err}"
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

    /// EXT-11: named roles via `[roles.extra.<name>]`, `@role`
    /// resolution, old-config compatibility, and router parsing.
    #[test]
    fn roles_extra_and_virtual_models_parse_overlay_and_resolve() {
        // existing configs keep parsing byte-identically
        let c =
            Config::parse_layer("[roles]\ndefault = \"a/x\"\nfast = \"b/y\"\n", "user").unwrap();
        assert!(c.roles.extra.is_empty());
        // named roles: [roles.extra.<name>] = "selector"
        let c = Config::parse_layer(
            "[roles]\ndefault = \"a/x\"\nfast = \"b/y@low\"\n\n[roles.extra]\nreview = \"r/r\"\n\"deep.name\" = \"c/z\"\n",
            "user",
        )
        .unwrap();
        assert_eq!(c.roles.extra.get("review").map(String::as_str), Some("r/r"));
        assert_eq!(
            c.roles.extra.get("deep.name").map(String::as_str),
            Some("c/z")
        );
        // resolution: @default/@fast read the built-ins, others read extra
        assert_eq!(c.roles.resolve("@default"), Some("a/x"));
        assert_eq!(c.roles.resolve("@fast"), Some("b/y@low"));
        assert_eq!(c.roles.resolve("@review"), Some("r/r"));
        assert_eq!(c.roles.resolve("@deep.name"), Some("c/z"));
        // unknown @names and bare selectors stay literal (None)
        assert_eq!(c.roles.resolve("@nope"), None);
        assert_eq!(c.roles.resolve("a/x"), None);
        assert_eq!(c.roles.resolve("@"), None);
        // overlay merges extra by name, upper layer wins
        let mut base =
            Config::parse_layer("[roles.extra]\nreview = \"b/y\"\nother = \"d/w\"\n", "base")
                .unwrap();
        let over = Config::parse_layer("[roles.extra]\nreview = \"c/z\"\n", "over").unwrap();
        base.overlay(over);
        assert_eq!(
            base.roles.extra.get("review").map(String::as_str),
            Some("c/z")
        );
        assert_eq!(
            base.roles.extra.get("other").map(String::as_str),
            Some("d/w")
        );
        // a typo directly under [roles] is STILL a hard error (the
        // explicit extra table keeps deny_unknown_fields useful)
        let err = Config::parse_layer("[roles]\nrevew = \"b/y\"\n", "user")
            .unwrap_err()
            .to_string();
        assert!(err.contains("revew"), "got: {err}");
        // virtual routers: table + strategy (default round_robin)
        let c = Config::parse_layer(
            "[virtual_models.fleet]\nmodels = [\"a/x\", \"b/y@low\"]\n\n[virtual_models.backup]\nmodels = [\"c/z\"]\nstrategy = \"failover\"\n",
            "user",
        )
        .unwrap();
        assert_eq!(
            c.virtual_models["fleet"].effective_strategy(),
            RouterStrategy::RoundRobin
        );
        assert_eq!(c.virtual_models["fleet"].models, ["a/x", "b/y@low"]);
        assert_eq!(
            c.virtual_models["backup"].effective_strategy(),
            RouterStrategy::Failover
        );
        // routers merge by name across layers
        let mut base =
            Config::parse_layer("[virtual_models.a]\nmodels = [\"a/x\"]\n", "base").unwrap();
        let over = Config::parse_layer(
            "[virtual_models.a]\nmodels = [\"b/y\"]\n\n[virtual_models.b]\nmodels = [\"c/z\"]\n",
            "over",
        )
        .unwrap();
        base.overlay(over);
        assert_eq!(base.virtual_models["a"].models, ["b/y"]);
        assert_eq!(base.virtual_models["b"].models, ["c/z"]);
        // validation: empty router and non-selector members are startup errors
        let err = Config::parse_layer("[virtual_models.a]\nmodels = []\n", "user")
            .unwrap_err()
            .to_string();
        assert!(err.contains("at least one entry"), "got: {err}");
        let err = Config::parse_layer("[virtual_models.a]\nmodels = [\"noslash\"]\n", "user")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a model selector"), "got: {err}");
        // the schema exposes the new surfaces
        let schema = Config::schema_json().unwrap();
        for key in [
            "extra",
            "virtual_models",
            "strategy",
            "round_robin",
            "failover",
        ] {
            assert!(schema.contains(&format!("\"{key}\"")), "schema lost {key}");
        }
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
                "project_doc_fallback_filenames = [\"CONTEXT.md\"]\n\n[guards]\nspend_usd = 2.5\ncontext_pct = 80\n\n[[search]]\nprovider = \"tavily\"\napi_key_env = \"TAVILY_KEY\"\n\n[tools.web]\nallow_private_hosts = true\n\n[tools.mcp]\ndiscovery = \"lazy\"\n\n[tui]\nheader_glyph = \"*\"\nbell = false\nnotify = \"notify-send ka done\"\nherdr = true\n\n[verify]\ntest = \"cargo test\"\n\n[[verify.lints]]\npattern = \"*.rs\"\ncommand = \"rustfmt --check {file}\"\n\n[features]\ndisable = [\"agents\", \"mcp:github\"]\nallow = [\"read\", \"grep\"]\n\n[prompt]\nidentity_file = \"/tmp/IDENTITY.md\"\nidentity_inline = \"You are Ka, personal agent.\"\n\n[lsp]\nwrite_through = true\n\n[debug]\nenable = true\n\n[debug.adapters]\ncodelldb = \"/opt/codelldb/adapter\"\n\n[history]\npersistence = \"none\"\nmax_bytes = 65536\n\n[[model_pins]]\npath = \"projects/ka/*\"\nmodel = \"openai/gpt-5.1@high\"\n",
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
        assert_eq!(
            cfg.lsp.write_through,
            Some(true),
            "[lsp] write_through survives the overlay"
        );
        assert_eq!(
            cfg.debug.enable,
            Some(true),
            "[debug] enable survives the overlay"
        );
        assert_eq!(
            cfg.debug
                .adapters
                .as_ref()
                .and_then(|a| a.get("codelldb"))
                .map(String::as_str),
            Some("/opt/codelldb/adapter"),
            "[debug] adapters survive the overlay"
        );
        assert_eq!(
            cfg.prompt.identity_inline.as_deref(),
            Some("You are Ka, personal agent."),
            "[prompt] identity_inline survives the overlay"
        );
        assert_eq!(
            cfg.effective_history_persistence(),
            HistoryPersistence::None,
            "[history] persistence survives the overlay"
        );
        assert_eq!(
            cfg.effective_history_max_bytes(),
            Some(65536),
            "[history] max_bytes survives the overlay"
        );
        assert_eq!(
            cfg.effective_model_for(std::path::Path::new("/home/me/projects/ka/sub")),
            Some("openai/gpt-5.1@high".to_string()),
            "[[model_pins]] survive the overlay and match"
        );
        assert_eq!(
            cfg.features.allow,
            vec!["read".to_string(), "grep".to_string()],
            "[features] allow survives the overlay"
        );
        assert_eq!(
            cfg.project_doc_fallback_filenames,
            vec!["CONTEXT.md".to_string()],
            "project_doc_fallback_filenames survive the overlay"
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
        // CFG-03: lints accumulate — both rules run, upper layer last
        assert_eq!(base.verify.lints.len(), 2, "lints merge across layers");
        assert_eq!(base.verify.lints[0].pattern, "*.rs");
        assert_eq!(base.verify.lints[1].pattern, "*.ts");
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

    #[test]
    fn model_pins_parse_first_match_suffix_and_overlay() {
        // no pins: no pin model anywhere
        assert_eq!(
            Config::default().effective_model_for(std::path::Path::new("/")),
            None
        );
        let c = Config::parse_layer(
            "[[model_pins]]\npath = \"projects/ka/*\"\nmodel = \"openai/gpt-5.1@high\"\n\n[[model_pins]]\npath = \"/home/me/work/*\"\nmodel = \"anthropic/claude-opus-4\"\n",
            "user",
        )
        .unwrap();
        assert_eq!(c.model_pins.len(), 2);
        // first match wins: `/home/me/work/x` matches the second pin —
        // and would match a suffix view of the first? no: `projects/ka/*`
        // can never suffix-match `work/x`
        assert_eq!(
            c.effective_model_for(std::path::Path::new("/home/me/work/x")),
            Some("anthropic/claude-opus-4".to_string())
        );
        // suffix match: `projects/ka/*` matches a subdirectory path
        assert_eq!(
            c.effective_model_for(std::path::Path::new("/home/me/projects/ka/sub/dir")),
            Some("openai/gpt-5.1@high".to_string())
        );
        // listed order decides when both globs match
        let both = Config::parse_layer(
            "[[model_pins]]\npath = \"*ka*\"\nmodel = \"a/first\"\n\n[[model_pins]]\npath = \"projects/*\"\nmodel = \"b/second\"\n",
            "user",
        )
        .unwrap();
        assert_eq!(
            both.effective_model_for(std::path::Path::new("/home/me/projects/ka")),
            Some("a/first".to_string()),
            "the FIRST matching pin wins"
        );
        // nothing matches → None (the configured model stands)
        assert_eq!(
            c.effective_model_for(std::path::Path::new("/opt/other")),
            None
        );
        // a pin row missing either key is a strict error
        assert!(Config::parse_layer("[[model_pins]]\npath = \"x\"\n", "u").is_err());
        assert!(Config::parse_layer("[[model_pins]]\nmodel = \"a/x\"\n", "u").is_err());
        // unknown keys hard-error
        let err = Config::parse_layer(
            "[[model_pins]]\npath = \"x\"\nmodel = \"a/x\"\nglob = \"y\"\n",
            "u",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("glob"), "{err}");
        // overlay (CFG-03): arrays merge — an empty layer keeps the
        // lower pins; an upper layer's pins sort FIRST (first-match
        // priority) and both survive
        let mut base = c;
        base.overlay(Config::default());
        assert_eq!(base.model_pins.len(), 2, "empty layer keeps pins");
        base.overlay(
            Config::parse_layer(
                "[[model_pins]]\npath = \"sandbox/*\"\nmodel = \"c/third\"\n",
                "o",
            )
            .unwrap(),
        );
        assert_eq!(base.model_pins.len(), 3, "upper pins merge in");
        assert_eq!(
            base.model_pins[0].path, "sandbox/*",
            "upper-layer pins sort first"
        );
        assert_eq!(
            base.effective_model_for(std::path::Path::new("sandbox/deep")),
            Some("c/third".to_string())
        );
        assert_eq!(
            base.effective_model_for(std::path::Path::new("/home/me/work/x")),
            Some("anthropic/claude-opus-4".to_string()),
            "lower-layer pins still match after the merge"
        );
        // the schema documents the table
        let schema = Config::schema_json().unwrap();
        assert!(
            schema.contains("ModelPin") && schema.contains("model_pins"),
            "{schema}"
        );
    }

    #[test]
    fn history_table_parses_strictly_and_overlays() {
        // defaults: persist everything, no byte ceiling
        assert_eq!(
            Config::default().effective_history_persistence(),
            HistoryPersistence::All
        );
        assert_eq!(Config::default().effective_history_max_bytes(), None);
        // both spellings parse
        let c = Config::parse_layer(
            "[history]\npersistence = \"none\"\nmax_bytes = 1048576\n",
            "u",
        )
        .unwrap();
        assert_eq!(c.effective_history_persistence(), HistoryPersistence::None);
        assert_eq!(c.effective_history_max_bytes(), Some(1048576));
        assert_eq!(
            Config::parse_layer("[history]\npersistence = \"all\"\n", "u")
                .unwrap()
                .effective_history_persistence(),
            HistoryPersistence::All
        );
        // a bare [history] header is valid (serde default on the table)
        let empty = Config::parse_layer("[history]\n", "u").unwrap();
        assert_eq!(empty.history, History::default());
        // typo'd value and unknown keys are hard errors (strict TOML)
        assert!(Config::parse_layer("[history]\npersistence = \"off\"\n", "u").is_err());
        let err = Config::parse_layer("[history]\npersist = \"none\"\n", "u")
            .unwrap_err()
            .to_string();
        assert!(err.contains("persist"), "{err}");
        // layering: explicit values survive an unset layer, upper layers win
        let mut base = c;
        base.overlay(Config::default());
        assert_eq!(
            base.effective_history_persistence(),
            HistoryPersistence::None,
            "explicit none survives an unset layer"
        );
        base.overlay(Config::parse_layer("[history]\nmax_bytes = 10\n", "proj").unwrap());
        assert_eq!(
            base.effective_history_persistence(),
            HistoryPersistence::None,
            "unset upper field keeps the lower value"
        );
        assert_eq!(
            base.effective_history_max_bytes(),
            Some(10),
            "upper layer wins"
        );
        // documented in the schema
        let schema = Config::schema_json().unwrap();
        assert!(
            schema.contains("HistoryPersistence") && schema.contains("max_bytes"),
            "{schema}"
        );
    }

    #[test]
    fn fallback_filenames_parse_overlay_and_schema() {
        // default empty
        assert!(Config::default().project_doc_fallback_filenames.is_empty());
        let c = Config::parse_layer(
            "project_doc_fallback_filenames = [\"CONTEXT.md\", \".cursorrules\"]\n",
            "u",
        )
        .unwrap();
        assert_eq!(
            c.project_doc_fallback_filenames,
            vec!["CONTEXT.md".to_string(), ".cursorrules".to_string()]
        );
        // overlay (CFG-03): merge — upper names sort first, lower names
        // survive (dedupe keeps single occurrences)
        let mut base = c;
        base.overlay(Config::default());
        assert_eq!(base.project_doc_fallback_filenames.len(), 2);
        base.overlay(
            Config::parse_layer(
                "project_doc_fallback_filenames = [\"NOTES.md\", \"CONTEXT.md\"]\n",
                "p",
            )
            .unwrap(),
        );
        assert_eq!(
            base.project_doc_fallback_filenames,
            vec![
                "NOTES.md".to_string(),
                "CONTEXT.md".to_string(),
                ".cursorrules".to_string()
            ],
            "upper first, dedupe keeps CONTEXT.md once"
        );
        // wrong type is a hard error
        assert!(
            Config::parse_layer("project_doc_fallback_filenames = \"CONTEXT.md\"\n", "u").is_err()
        );
        // documented in the schema
        let schema = Config::schema_json().unwrap();
        assert!(
            schema.contains("project_doc_fallback_filenames"),
            "got: {schema}"
        );
    }

    #[test]
    fn identity_inline_parses_and_overlays() {
        // set + layered overlay (set fields win)
        let c = Config::parse_layer("[prompt]\nidentity_inline = \"You are Ka.\"\n", "u").unwrap();
        assert_eq!(c.prompt.identity_inline.as_deref(), Some("You are Ka."));
        let mut base = c;
        base.overlay(Config::parse_layer("[prompt]\nidentity_inline = \"Other.\"\n", "o").unwrap());
        assert_eq!(base.prompt.identity_inline.as_deref(), Some("Other."));
        // unset upper layer keeps the lower value
        base.overlay(
            Config::parse_layer("[prompt]\nidentity_file = \"/tmp/ID.md\"\n", "o2").unwrap(),
        );
        assert_eq!(base.prompt.identity_inline.as_deref(), Some("Other."));
        // unknown keys under [prompt] hard-error
        let err = Config::parse_layer("[prompt]\nidentity_text = \"x\"\n", "u")
            .unwrap_err()
            .to_string();
        assert!(err.contains("identity_text"), "{err}");
        // documented in the schema
        assert!(Config::schema_json().unwrap().contains("identity_inline"));
    }

    #[test]
    fn save_settings_writes_schema_sidecar_and_directive() {
        let dir = std::env::temp_dir().join(format!("ka-cfg-schema-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ka.toml");
        save_settings_to(&path, Some("a/x"), None, None, None, None).unwrap();
        // the directive is the first line and the sidecar exists
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("#:schema ka.schema.json\n"),
            "directive prepended as line one: {text:?}"
        );
        let schema_path = dir.join("ka.schema.json");
        let schema = std::fs::read_to_string(&schema_path).unwrap();
        serde_json::from_str::<serde_json::Value>(&schema)
            .expect("the sidecar is valid JSON schema");
        // the directive is a plain TOML comment: strict parse round-trips
        let layer = Config::parse_layer(&text, "roundtrip").unwrap();
        assert_eq!(layer.model.as_deref(), Some("a/x"));
        // a second save rewrites from scratch: still exactly one directive
        save_settings_to(&path, Some("b/y"), None, None, None, None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text.matches("#:schema").count(),
            1,
            "no directive duplication across saves: {text:?}"
        );
        let layer = Config::parse_layer(&text, "roundtrip2").unwrap();
        assert_eq!(layer.model.as_deref(), Some("b/y"));
        // a differently-named toml gets its own stem in the directive
        let other = dir.join("custom.toml");
        save_settings_to(&other, None, None, None, None, None).unwrap();
        let text = std::fs::read_to_string(&other).unwrap();
        assert!(
            text.starts_with("#:schema custom.schema.json\n"),
            "{text:?}"
        );
        assert!(dir.join("custom.schema.json").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------ Lote 9: permissions

    /// PERM-01: `[permissions.profiles.<name>]` parse, `extends`
    /// resolution (own rules first), activation error surface, and the
    /// exact `effective_rules` gate order (profile ahead of [[rules]]).
    #[test]
    fn permission_profiles_parse_extend_activate_and_order() {
        let c = Config::parse_layer(
            r#"
[[rules]]
tool = "bash"
pattern = "cargo *"
verdict = "allow"

[permissions]
profile = "strict"

[permissions.profiles.strict]
extends = "base"
rules = [
  { tool = "bash", pattern = "rm -rf *", verdict = "deny" },
]

[permissions.profiles.base]
rules = [
  { tool = "bash", pattern = "git *", verdict = "ask" },
]
"#,
            "user",
        )
        .unwrap();
        assert_eq!(c.permissions.profile.as_deref(), Some("strict"));
        // gate order: strict's own rules, then base's (extends, one
        // chain), then the global [[rules]]
        let rules = c.effective_rules();
        assert_eq!(rules.len(), 3, "{rules:?}");
        assert_eq!(rules[0].pattern.as_deref(), Some("rm -rf *"));
        assert_eq!(rules[1].pattern.as_deref(), Some("git *"));
        assert_eq!(rules[2].pattern.as_deref(), Some("cargo *"));
        // the same matching the gate uses, driven through the pub fn
        // (shipped semantics: the first rule naming the tool decides)
        assert_eq!(
            evaluate_rules(&rules, "bash", "rm -rf /tmp/x"),
            Some(Verdict::Deny),
            "profile rules outrank the global [[rules]]"
        );
        assert_eq!(evaluate_rules(&rules, "bash", "make all"), None);
        assert_eq!(
            evaluate_rules(&rules[1..], "bash", "git status"),
            Some(Verdict::Ask),
            "the inherited parent rule applies when first"
        );

        // no profile: exactly the global rules
        let plain =
            Config::parse_layer("[[rules]]\ntool = \"write\"\nverdict = \"deny\"\n", "u").unwrap();
        assert_eq!(plain.effective_rules().len(), 1);

        // unknown activation name is a config error naming the profile
        let err = Config::parse_layer("[permissions]\nprofile = \"ghost\"\n", "u")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ghost"), "{err}");

        // unknown extends is a config error naming the parent
        let err = Config::parse_layer(
            "[permissions]\nprofile = \"a\"\n\n[permissions.profiles.a]\nextends = \"ghost\"\n",
            "u",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("ghost"), "{err}");

        // a cycle is a config error
        let err = Config::parse_layer(
            "[permissions]\nprofile = \"a\"\n\n[permissions.profiles.a]\nextends = \"b\"\n\n[permissions.profiles.b]\nextends = \"a\"\n",
            "u",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("cycle"), "{err}");

        // unknown keys under a profile hard-error (strict shape)
        let err = Config::parse_layer("[permissions.profiles.a]\npattern = \"x\"\n", "u")
            .unwrap_err()
            .to_string();
        assert!(err.contains("pattern") || err.contains("unknown"), "{err}");

        // profiles merge across layers (union by name, upper wins)
        let mut merged = c;
        merged.overlay(
            Config::parse_layer(
                "[permissions.profiles.strict]\nrules = [{ tool = \"write\", verdict = \"ask\" }]\n\n[permissions.profiles.extra]\nrules = [{ tool = \"read\", verdict = \"deny\" }]\n",
                "project",
            )
            .unwrap(),
        );
        assert_eq!(merged.permissions.profiles.len(), 3, "union by name");
        assert_eq!(
            merged.permissions.profiles["strict"].rules.len(),
            1,
            "upper layer replaces the profile wholesale"
        );
        assert_eq!(
            merged.effective_rules().first().map(|r| r.tool.clone()),
            Some("write".into()),
            "activated profile picks up the upper layer's rules"
        );
    }

    /// PERM-06: `evaluate_rules` is the gate's matcher — shipped
    /// semantics: the FIRST rule naming the tool is decisive, and when
    /// its pattern doesn't match, no rule applies at all (a
    /// pattern-less rule always matches). Web tools use host semantics.
    #[test]
    fn evaluate_rules_matches_first_and_web_hosts() {
        let rules = vec![
            Rule {
                tool: "web_fetch".into(),
                pattern: Some("evil.example".into()),
                verdict: Verdict::Deny,
            },
            Rule {
                tool: "web_fetch".into(),
                pattern: None,
                verdict: Verdict::Ask,
            },
        ];
        assert_eq!(
            evaluate_rules(&rules, "web_fetch", "https://sub.evil.example/x"),
            Some(Verdict::Deny),
            "first match wins, subdomain covered"
        );
        // first-rule-for-tool semantics: a non-matching pattern on the
        // first web_fetch rule means NO rule applies (the gate ships
        // this; `ka rules check` must pre-flight identically)
        assert_eq!(
            evaluate_rules(&rules, "web_fetch", "https://docs.rs/x"),
            None
        );
        assert_eq!(evaluate_rules(&rules, "bash", "git status"), None);

        // pattern-less rules match every call of their tool
        let catch_all = vec![Rule {
            tool: "bash".into(),
            pattern: None,
            verdict: Verdict::Ask,
        }];
        assert_eq!(
            evaluate_rules(&catch_all, "bash", "cargo test"),
            Some(Verdict::Ask)
        );
    }

    /// PERM-05: [shell] env policy parses strictly, defaults to
    /// inherit-all, overlays across layers, resolves to a policy.
    #[test]
    fn shell_env_policy_parses_overlays_and_resolves() {
        let defaults = Config::default();
        let policy = defaults.effective_shell_env();
        assert_eq!(policy.mode, ShellEnvMode::All);
        assert!(policy.keep.is_empty() && policy.set.is_empty());

        let c = Config::parse_layer(
            "[shell]\nenv = \"none\"\nenv_keep = [\"CI\"]\nenv_set = { KA_A = \"1\" }\n",
            "user",
        )
        .unwrap();
        let policy = c.effective_shell_env();
        assert_eq!(policy.mode, ShellEnvMode::None);
        assert_eq!(policy.keep, vec!["CI".to_string()]);
        assert_eq!(policy.set.get("KA_A").map(String::as_str), Some("1"));

        // core mode + upper layer narrows mode, adds keep + set keys
        let mut merged = c;
        merged.overlay(
            Config::parse_layer(
                "[shell]\nenv = \"core\"\nenv_keep = [\"NO_COLOR\"]\nenv_set = { KA_B = \"2\" }\n",
                "project",
            )
            .unwrap(),
        );
        let policy = merged.effective_shell_env();
        assert_eq!(policy.mode, ShellEnvMode::Core);
        assert_eq!(policy.keep, vec!["CI".to_string(), "NO_COLOR".to_string()]);
        assert_eq!(policy.set.len(), 2, "set keys merge across layers");

        // unknown vocabulary stays a hard error
        assert!(Config::parse_layer("[shell]\nenv = \"some\"\n", "u").is_err());
        assert!(Config::parse_layer("[shell]\nnonsense = 1\n", "u").is_err());
    }

    /// PERM-02/07/08: guard + permission vocabulary parses; unknown
    /// values hard-error through the typed enums.
    #[test]
    fn guard_tier_prompt_policy_and_injection_parse() {
        let c = Config::parse_layer(
            "[guards]\nauto_review = true\nauto_review_tier = \"exec+write\"\ninjection_check = true\n\n[permissions]\nprompt_policy = \"deny\"\n",
            "u",
        )
        .unwrap();
        assert_eq!(c.guards.auto_review_tier, Some(AutoReviewTier::ExecWrite));
        assert_eq!(c.guards.injection_check, Some(true));
        assert_eq!(
            c.effective_prompt_policy(),
            PromptPolicy::Deny,
            "deny covers serve/CI without --dangerously"
        );
        // defaults: exec tier, ask policy, classifier off
        let d = Config::default();
        assert_eq!(
            d.guards.auto_review_tier.unwrap_or_default(),
            AutoReviewTier::Exec
        );
        assert_eq!(d.effective_prompt_policy(), PromptPolicy::Ask);
        assert_eq!(d.guards.injection_check, None);
        // upper layer overrides the tier and the policy
        let mut m = c;
        m.overlay(
            Config::parse_layer(
                "[guards]\nauto_review_tier = \"exec\"\n\n[permissions]\nprompt_policy = \"ask\"\n",
                "p",
            )
            .unwrap(),
        );
        assert_eq!(m.guards.auto_review_tier, Some(AutoReviewTier::Exec));
        assert_eq!(m.effective_prompt_policy(), PromptPolicy::Ask);
        // unknown values are parse errors (strict vocabulary)
        assert!(Config::parse_layer("[guards]\nauto_review_tier = \"everything\"\n", "u").is_err());
        assert!(Config::parse_layer("[permissions]\nprompt_policy = \"silent\"\n", "u").is_err());
    }
}

#[cfg(test)]
mod tui_config_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Batch 5: [tui] theme/status_line/title/notify vocabulary is
    /// strict; [keybindings] merges across layers (upper wins).
    #[test]
    fn tui_keys_parse_validate_and_merge() {
        let c = Config::parse_layer(
            "[tui]\ntheme = \"solarized\"\ntitle = false\nstatus_line = [\"model\", \"context\", \"cost\"]\nnotify_on = [\"ask\"]\nnotify_unfocused = true\n\n[keybindings]\n\"session.new\" = \"ctrl+n\"\n\"theme.next\" = \"f4\"\n",
            "user",
        )
        .unwrap();
        assert_eq!(c.tui.theme.as_deref(), Some("solarized"));
        assert_eq!(c.tui.title, Some(false));
        assert_eq!(
            c.tui.status_line,
            Some(vec!["model".into(), "context".into(), "cost".into()])
        );
        assert_eq!(c.tui.notify_on, Some(vec!["ask".into()]));
        assert_eq!(c.tui.notify_unfocused, Some(true));
        assert_eq!(
            c.keybindings.as_ref().map(|k| k.get("session.new")),
            Some(Some(&"ctrl+n".to_string()))
        );
        // unknown vocabulary is a hard error naming the value
        let err = Config::parse_layer("[tui]\nstatus_line = [\"model\", \"vibes\"]\n", "u")
            .unwrap_err()
            .to_string();
        assert!(err.contains("vibes"), "{err}");
        let err = Config::parse_layer("[tui]\nnotify_on = [\"turn_started\"]\n", "u")
            .unwrap_err()
            .to_string();
        assert!(err.contains("turn_started"), "{err}");
        // merge: an upper [keybindings] adds/remaps, keeps lower keys
        let mut base = c;
        base.overlay(
            Config::parse_layer(
                "[keybindings]\n\"session.new\" = \"ctrl+alt+n\"\n\"composer.abort\" = \"f8\"\n",
                "o",
            )
            .unwrap(),
        );
        let kb = base.keybindings.as_ref().unwrap();
        assert_eq!(
            kb.get("session.new").map(String::as_str),
            Some("ctrl+alt+n"),
            "upper remaps"
        );
        assert_eq!(
            kb.get("composer.abort").map(String::as_str),
            Some("f8"),
            "upper adds"
        );
        assert_eq!(
            kb.get("theme.next").map(String::as_str),
            Some("f4"),
            "lower keys survive"
        );
    }
}
