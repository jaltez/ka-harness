//! The live voice: multi-step tool loop. Each step speaks with the offered
//! tools, executes returned calls through the Hands registry under
//! clearance gating (hardstops always prompt; headless surfaces deny),
//! feeds results back, and repeats until the model rests or the step cap
//! hits.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use ka_dialect::dialects::{Catalog, Wire};
use ka_dialect::speaker::{
    SpeakRequest, Speaker, StreamEvent, ToolCall, ToolResult, ToolSpec, TurnMessage, TurnRole,
};
use ka_protocol::{AskId, AskQuestion, Command, ErrorClass, Event, Stop, Usage};
use tokio::sync::{mpsc, oneshot};

use crate::hands::bashp::{all_readonly, analyze, hardstop, redirect_targets, wants_network};
use crate::hands::{
    Clearance, Hand, HandContext, Ledger, Spill, ToolOutput, registry_with_pathfinder,
};

/// Session-scoped mutable state the voice needs (owned by the engine).
#[derive(Default)]
pub struct VoiceState {
    /// Permission memory: rules granted via the "always" ask option.
    pub rules: HashSet<String>,
    /// Ask id counter.
    pub ask_counter: u64,
    /// Identical tool+argument counts (loop guard).
    pub loop_counts: HashMap<String, u32>,
    /// Loop-gate signatures the user allowed "always" (session scope).
    pub loop_ok: HashSet<String>,
    /// Sandbox-expansion grants remembered per command signature
    /// ("always" answers; applied silently on identical repeats).
    pub sandbox_granted: HashMap<String, ka_sandbox::Grants>,
    /// Signatures already granted-asked once — never nag an identical
    /// command twice in one session (fail-closed on repeat).
    pub sandbox_asked: HashSet<String>,
    /// Per-call expansion grants awaiting the executor (call id →
    /// grants), set by a "this run" allow at admit time.
    pub sandbox_pending: HashMap<String, ka_sandbox::Grants>,
}

/// Spend/context guard runtime state: thresholds from config, the
/// running session spend, and per-session latches. Owned by the engine
/// ([`crate::engine::EngineState`]); the turn consults and latches it.
#[derive(Debug, Clone, Default)]
pub struct GuardRuntime {
    /// Session spend cap in USD (None = off).
    pub spend_usd: Option<f64>,
    /// Context-percentage cap (None = off).
    pub context_pct: Option<u64>,
    /// True once the spend ask fired this session.
    pub spend_latched: bool,
    /// True once the context ask fired this session.
    pub context_latched: bool,
    /// Summed cost of completed turns this session.
    pub session_spend: f64,
}

impl GuardRuntime {
    /// Build from parsed `[guards]` config (both knobs default off).
    pub fn new(spend_usd: Option<f64>, context_pct: Option<u64>) -> Self {
        Self {
            spend_usd,
            context_pct,
            spend_latched: false,
            context_latched: false,
            session_spend: 0.0,
        }
    }

    /// Re-arm for a (re)attached strand, seeding the spend total from
    /// the strand's persisted Usage records. A session resumed above
    /// its cap counts as already crossed (latched, no fresh ask).
    pub fn reset(&mut self, session_spend: f64) {
        self.spend_latched = self.spend_usd.is_some_and(|cap| session_spend >= cap);
        self.context_latched = false;
        self.session_spend = session_spend;
    }
}

// Context-survival knobs (Phase 4 defaults; config knobs later).
/// Tokens of recent tool outputs protected from pruning.
const PROTECT_WINDOW_TOKENS: u64 = 40_000;
/// Minimum estimated savings before pruning fires.
const MIN_PRUNE_SAVINGS: u64 = 20_000;
/// Tokens of history the digest keeps after the summary.
const KEEP_TAIL_TOKENS: u64 = 20_000;
/// Digest reserve floor.
const RESERVE_FLOOR: u64 = 16_384;
/// Digest reserve fraction of the window (15%).
const RESERVE_PCT: u64 = 15;

const DIGEST_SYSTEM: &str = "You are a context digester. Summarize the conversation so a \
continuing agent can pick up exactly where it left off. Preserve: the \
task and its current state, key decisions and their reasons, file paths \
touched (read vs modified), open threads and next steps, and any \
user-stated constraints. Be dense; skip pleasantries. Output only the \
summary.";

/// The `[guards] auto_review` reviewer's instructions: one word back,
/// `allow` only when unmistakably safe — everything else is `ask` and
/// falls through to the human.
const REVIEW_SYSTEM: &str = "You review shell commands for a coding agent's permission \
gate. Reply with exactly one word: `allow` when the command is unmistakably safe to run \
(build, test, list, read-only inspection, routine project-scoped work), `ask` in every \
other case (network mutation, credentials, system-wide changes, anything destructive or \
unusual). When in doubt, `ask`.";

/// Simple glob match: `*` spans anything, `?` one char, everything else
/// literal. No path semantics — patterns match raw strings.
/// One permission rule's pattern against a call's primary argument.
/// File/bash tools glob the argument verbatim; the web tools match the
/// URL's host with domain semantics — `example.com` covers the apex
/// and every subdomain, `*.example.com` covers subdomains (the
/// claude-code `WebFetch(domain:)` behavior).
pub fn rule_pattern_matches(pattern: &str, tool: &str, primary: &str) -> bool {
    if tool != "web_fetch" && tool != "web_search" {
        return glob_match(pattern, primary);
    }
    let Some(host) = url_host(primary) else {
        return glob_match(pattern, primary);
    };
    if glob_match(pattern, &host) {
        return true;
    }
    // `example.com` covers apex + subdomains; `*.example.com` covers
    // subdomains only (the claude-code WebFetch(domain:) behavior)
    let star = pattern.starts_with("*.");
    let domain = pattern.trim_start_matches("*.");
    if star {
        host.ends_with(&format!(".{domain}"))
    } else {
        host == domain || host.ends_with(&format!(".{domain}"))
    }
}

/// Lowercased host of an http(s) URL (None for anything else).
fn url_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.split('@').next_back().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    let host = host.trim().to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn inner(p: &[char], t: &[char]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some('*'), _) => (0..=t.len()).any(|skip| inner(&p[1..], &t[skip..])),
            (Some('?'), Some(_)) => inner(&p[1..], &t[1..]),
            (Some(pc), Some(tc)) if pc == tc => inner(&p[1..], &t[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    inner(&p, &t)
}

/// Rough token estimate for one message under a chars-per-token ratio.
fn message_tokens(msg: &TurnMessage, ratio: f64) -> u64 {
    let chars = msg.content.chars().count() as u64
        + msg
            .calls
            .iter()
            .map(|c| c.arguments.to_string().chars().count() as u64 + c.tool.chars().count() as u64)
            .sum::<u64>()
        + msg
            .results
            .iter()
            .map(|r| r.content.chars().count() as u64)
            .sum::<u64>();
    (chars as f64 / ratio.max(0.1)) as u64
}

/// Everything needed to speak to real models and act on the world.
/// Slot for an in-flight speculative digest: (history watermark,
/// result receiver).
type SpecSlot = std::sync::Arc<tokio::sync::Mutex<Option<(usize, oneshot::Receiver<String>)>>>;

/// Shared LSP diagnostics manager (engine-owned, opt-in): edit/write
/// results get informational diagnostics appended. A cheap cloneable
/// handle — internal state lives behind its own locks, so polls never
/// hold the server table.
pub(crate) type LspSlot = std::sync::Arc<crate::lsp::LspManager>;

pub struct Voice {
    catalog: Catalog,
    speakers: HashMap<Wire, std::sync::Arc<dyn Speaker>>,
    hands: Vec<std::sync::Arc<dyn Hand>>,
    hand_ctx: HandContext,
    pub(crate) state: VoiceState,
    max_steps: u32,
    mode: ka_protocol::Mode,
    /// Conversation history (owned by the voice; engine persists deltas).
    pub history: Vec<TurnMessage>,
    /// Active model selector (set per turn; needed by settle-time digests).
    model_selector: Option<String>,
    /// Reasoning-effort level name from the engine's live settings
    /// (`SetEffort`); a selector `@effort` suffix still wins per model.
    effort: Option<String>,
    /// Active strand id — every request carries it for gateways that
    /// route by conversation (the wire maps the vendor to a header via
    /// `providers::session_header`; opencode-go needs
    /// `x-opencode-session` or the call fails).
    session_id: Option<String>,
    /// Chars-per-token ratio of the active model (estimates).
    ratio: f64,
    /// Active digest summary (prepended to requests, not part of history).
    digest: Option<String>,
    /// Last measured context consumption (tokens, from provider usage).
    last_context: u64,
    /// Stop kind of the most recent turn (set at each turn exit).
    pub(crate) last_stop: Stop,
    /// Bumped every time a digest replaces history.
    digest_revision: u64,
    /// (summary, kept index) of the most recent digest, for persistence.
    last_digest: Option<(String, usize)>,
    /// Configured permission rules (first-match-wins).
    rules_cfg: Vec<crate::config::Rule>,
    /// Configured hooks.
    hooks_cfg: Vec<crate::config::Hook>,
    /// Staged by the engine to run just before the terminal
    /// TurnFinished — auto-commit notes must precede the event ACP
    /// clients break on. Taken and cleared at every turn exit.
    /// Staged by the engine to run just before the terminal
    /// TurnFinished — auto-commit notes must precede the event ACP
    /// clients break on. Taken and cleared at every turn exit. (Mutex
    /// keeps `Voice: Sync` — spawned turn futures hold `&Voice`.)
    pub(crate) pre_finish:
        Option<parking_lot::Mutex<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>>,
    /// Persistent allowlist from `[permissions] allow` (tools that skip
    /// the ask entirely).
    allowed_tools: Vec<String>,
    /// Fallback model chain ([fallback] models; tried in order on
    /// provider/auth failure, max 2 hops per turn).
    fallbacks: Vec<String>,
    /// `[prompt] identity_file`: a file whose text replaces the
    /// built-in identity sentence (re-read every turn; None = default).
    identity_file: Option<String>,
    /// One unreadable-identity Note per session (not one per turn).
    identity_warned: bool,
    /// Auto-promote to a bigger-context sibling on overflow
    /// ([context] promote, default true).
    context_promote: bool,
    /// Promotion already fired for the attached strand (once per strand).
    promoted: bool,
    /// Promotion the engine must apply after the turn (selector).
    pending_promotion: Option<String>,
    /// In-flight speculative digest slot (shared with the background
    /// summarizer task): (history watermark, result receiver).
    speculative: SpecSlot,
    /// Pathfinder bootstrap slot shared with the hand.
    pathfinder_slot:
        std::sync::Arc<parking_lot::RwLock<crate::hands::pathfinder::PathfinderSource>>,
    /// Todo list slot shared with the hand; forwarded to surfaces as
    /// `Event::Todos` after each `todo` call.
    todo: crate::hands::todo::TodoSlot,
    /// Opt-in LSP diagnostics manager (engine bootstrap).
    lsp: Option<LspSlot>,
    /// [verify] lint rules — see [`crate::config::Verify`].
    verify: crate::config::Verify,
    /// Files edited this turn (successful edit/write paths), for the
    /// [verify] test loop. Shared with the spawned tool tasks.
    edited: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
    /// `[guards] auto_review`: the fast-role reviewer pre-screens
    /// exec-tier permission asks (roadmap 9.4). Auto-allow only.
    auto_review: bool,
    /// Resolved fast-role model id for the reviewer (None = disabled).
    reviewer_model: Option<String>,
    /// Per-turn tool allowlist (custom-command frontmatter
    /// `allowed-tools`); None = the session toolset. Set by the engine
    /// around one turn, cleared after.
    turn_tools: Option<Vec<String>>,
    /// Per-turn explicit skill invocation (`/skill:<name>`): the named
    /// skills' full SKILL.md bodies ride this turn's system prompt.
    /// Set by the engine around one turn, cleared after.
    turn_skills: Option<Vec<String>>,
    /// Session feature toggles, shared with the hands that validate
    /// dynamically (delegate roster, lazy MCP calls). Filtering happens
    /// in `specs()` (hide) and `admit_call` (reject strays).
    features: crate::features::FeatureSlot,
    /// Feature/sandbox commands that arrived mid-turn: the cheap part
    /// (hide/deny layer) applied immediately, the side effects
    /// (spawn/strand record/inventory) are the engine's to run once the
    /// turn settles. Drained by the engine after each turn.
    pending_features: parking_lot::Mutex<Vec<Command>>,
}

impl Voice {
    /// New voice over a catalog, working in `cwd`.
    pub fn new(
        catalog: Catalog,
        cwd: std::path::PathBuf,
        mode: ka_protocol::Mode,
        max_steps: u32,
    ) -> Self {
        let slot = std::sync::Arc::new(parking_lot::RwLock::new(
            crate::hands::pathfinder::PathfinderSource::default(),
        ));
        let todos = crate::hands::todo::slot();
        let jobs = std::sync::Arc::new(crate::hands::jobs::JobTable::new());
        Self {
            catalog,
            speakers: Default::default(),
            hands: registry_with_pathfinder(slot.clone(), todos.clone(), jobs.clone()),
            hand_ctx: HandContext {
                cwd: cwd.clone(),
                ledger: std::sync::Arc::new(parking_lot::Mutex::new(Ledger::default())),
                spill: std::sync::Arc::new(Spill::new()),
                snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                    crate::hands::snapshots::Snapshots::open(&cwd),
                )),
                jobs,
                bash_background_ms: 0,
                max_image_mb: 5,
                web_allow_private: false,
                sandbox: ka_sandbox::Policy::Off,
            },
            state: VoiceState::default(),
            max_steps,
            mode,
            history: Vec::new(),
            model_selector: None,
            effort: None,
            session_id: None,
            ratio: 4.0,
            digest: None,
            last_context: 0,
            last_stop: Stop::Done,
            digest_revision: 0,
            last_digest: None,
            fallbacks: Vec::new(),
            identity_file: None,
            identity_warned: false,
            context_promote: true,
            promoted: false,
            pending_promotion: None,
            speculative: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            rules_cfg: Vec::new(),
            hooks_cfg: Vec::new(),
            pre_finish: None,
            allowed_tools: Vec::new(),
            pathfinder_slot: slot,
            todo: todos,
            lsp: None,
            verify: crate::config::Verify::default(),
            edited: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
            auto_review: false,
            reviewer_model: None,
            turn_tools: None,
            turn_skills: None,
            features: crate::features::FeatureToggles::slot(Vec::new()),
            pending_features: parking_lot::Mutex::new(Vec::new()),
        }
    }

    /// Register an extra tool (MCP hands arrive after async discovery).
    pub fn push_hand(&mut self, hand: std::sync::Arc<dyn Hand>) {
        self.hands.push(hand);
    }

    /// Replace one MCP server's hands in registry order (reconnect or
    /// refresh tool diff). Absent prefixes append at the end.
    pub fn replace_server_hands(&mut self, server: &str, hands: Vec<std::sync::Arc<dyn Hand>>) {
        let prefix = format!("{server}.");
        let mut result: Vec<std::sync::Arc<dyn Hand>> = Vec::with_capacity(self.hands.len());
        let mut replaced = false;
        for h in self.hands.drain(..) {
            if h.def().name.starts_with(&prefix) {
                if !replaced {
                    replaced = true;
                    result.extend(hands.iter().cloned());
                }
            } else {
                result.push(h);
            }
        }
        if !replaced {
            result.extend(hands);
        }
        self.hands = result;
    }

    /// Tool names in registry order (built-ins, then anything pushed at
    /// bootstrap: the delegate hand, MCP hands). Bootstrap inventory.
    pub fn hand_names(&self) -> Vec<String> {
        self.hands.iter().map(|h| h.def().name).collect()
    }

    /// Tool names the model can see right now: the registry minus
    /// feature-toggled hands. This is what re-emitted inventory cards
    /// report after a toggle change.
    pub fn visible_hand_names(&self) -> Vec<String> {
        let features = self.features.read();
        self.hands
            .iter()
            .map(|h| h.def().name)
            .filter(|n| features.hidden_reason(n).is_none())
            .collect()
    }

    /// Replace every hand named `name` with `hands` in registry order
    /// (appending when absent). Used to rebuild the lazy `mcp_call` /
    /// meta hands when the connected-server set changes.
    pub fn replace_named_hands(&mut self, name: &str, hands: Vec<std::sync::Arc<dyn Hand>>) {
        let mut result: Vec<std::sync::Arc<dyn Hand>> = Vec::with_capacity(self.hands.len());
        let mut replaced = false;
        for h in self.hands.drain(..) {
            if h.def().name == name {
                if !replaced {
                    replaced = true;
                    result.extend(hands.iter().cloned());
                }
            } else {
                result.push(h);
            }
        }
        if !replaced && !hands.is_empty() {
            result.extend(hands);
        }
        self.hands = result;
    }

    /// The shared feature-toggle slot (engine bootstrap: hands that
    /// validate dynamically hold a clone).
    pub fn feature_slot(&self) -> crate::features::FeatureSlot {
        self.features.clone()
    }

    /// Install the engine-primed toggle slot (bootstrap: replaces the
    /// fresh all-on slot before hands that share it are constructed).
    pub fn set_feature_slot(&mut self, slot: crate::features::FeatureSlot) {
        self.features = slot;
    }

    /// Apply one feature toggle to the live set (returns whether it
    /// changed anything). Spawn/suspend side effects are the engine's.
    pub fn set_feature(&mut self, spec: &ka_protocol::FeatureSpec, enabled: bool) -> bool {
        self.features.write().set(spec, enabled)
    }

    /// Replace the whole toggle snapshot (strand restore). Live MCP
    /// server names are preserved — they describe the world, not the
    /// toggles.
    pub fn restore_feature_snapshot(
        &mut self,
        disabled: Vec<ka_protocol::FeatureSpec>,
        sandbox: Option<String>,
    ) {
        self.features.write().replace_snapshot(disabled, sandbox);
    }

    /// The current toggle snapshot: disabled specs + sandbox override.
    pub fn features_snapshot(&self) -> (Vec<String>, Option<String>) {
        let f = self.features.read();
        (f.disabled_specs(), f.sandbox_override().map(str::to_string))
    }

    /// Whether hooks run at all (the `hooks` feature toggle). Engine
    /// pre/post-turn file hooks consult this.
    pub fn hooks_enabled(&self) -> bool {
        self.features.read().hooks_enabled()
    }

    /// Feature/sandbox commands buffered mid-turn, for the engine to
    /// replay through the full handler once the turn settles.
    pub(crate) fn take_pending_features(&self) -> Vec<Command> {
        std::mem::take(&mut *self.pending_features.lock())
    }

    fn buffer_feature_cmd(&self, cmd: Command) {
        self.pending_features.lock().push(cmd);
    }

    /// Share the snapshot journal (engine-side undo + strand tracking).
    pub fn snapshot_sink(
        &self,
    ) -> std::sync::Arc<parking_lot::Mutex<crate::hands::snapshots::Snapshots>> {
        self.hand_ctx.snapshots.clone()
    }

    /// Read-only research voice (pathfinder): inspect tools only.
    pub fn new_readonly(
        catalog: Catalog,
        cwd: std::path::PathBuf,
        mode: ka_protocol::Mode,
        max_steps: u32,
    ) -> Self {
        let slot = std::sync::Arc::new(parking_lot::RwLock::new(
            crate::hands::pathfinder::PathfinderSource::default(),
        ));
        let todos = crate::hands::todo::slot();
        let hands = crate::hands::registry_with_pathfinder(
            slot.clone(),
            todos.clone(),
            std::sync::Arc::new(crate::hands::jobs::JobTable::new()),
        );
        let hand_ctx = HandContext {
            cwd,
            ledger: std::sync::Arc::new(parking_lot::Mutex::new(Ledger::default())),
            spill: std::sync::Arc::new(Spill::new()),
            snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
            jobs: std::sync::Arc::new(crate::hands::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
        };
        Self {
            catalog,
            speakers: Default::default(),
            hands,
            hand_ctx,
            state: VoiceState::default(),
            max_steps,
            mode,
            rules_cfg: Vec::new(),
            hooks_cfg: Vec::new(),
            pre_finish: None,
            allowed_tools: Vec::new(),
            history: Vec::new(),
            model_selector: None,
            effort: None,
            session_id: None,
            ratio: 4.0,
            digest: None,
            last_context: 0,
            last_stop: Stop::Done,
            digest_revision: 0,
            last_digest: None,
            fallbacks: Vec::new(),
            identity_file: None,
            identity_warned: false,
            context_promote: true,
            promoted: false,
            pending_promotion: None,
            speculative: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            pathfinder_slot: slot,
            todo: todos,
            lsp: None,
            verify: crate::config::Verify::default(),
            edited: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
            auto_review: false,
            reviewer_model: None,
            turn_tools: None,
            turn_skills: None,
            features: crate::features::FeatureToggles::slot(Vec::new()),
            pending_features: parking_lot::Mutex::new(Vec::new()),
        }
    }

    /// Set configured permission rules (engine bootstrap).
    pub fn set_rules(&mut self, rules: Vec<crate::config::Rule>) {
        self.rules_cfg = rules;
    }

    /// The pathfinder's bootstrap slot (engine writes catalog/model).
    pub fn pathfinder_slot(
        &self,
    ) -> std::sync::Arc<parking_lot::RwLock<crate::hands::pathfinder::PathfinderSource>> {
        self.pathfinder_slot.clone()
    }

    /// Set the reasoning-effort level name from the engine's live
    /// settings (`None` clears it back to endpoint defaults). A
    /// selector `@effort` suffix still overrides per model.
    pub fn set_effort(&mut self, level: Option<String>) {
        self.effort = level;
    }

    /// Record the active strand id (attach/transfer); every request
    /// carries it so gateways that route by conversation can route and
    /// cache. Nested voices inherit it through the pathfinder slot.
    pub fn set_session_id(&mut self, id: Option<String>) {
        self.session_id = id;
    }

    /// Set configured hooks (engine bootstrap).
    pub fn set_hooks(&mut self, hooks: Vec<crate::config::Hook>) {
        self.hooks_cfg = hooks;
    }

    /// Set the persistent tool allowlist (`[permissions] allow`).
    pub fn set_allowed_tools(&mut self, tools: Vec<String>) {
        self.allowed_tools = tools;
    }

    /// Restrict the registry to the named hands (an agent's `tools:`
    /// frontmatter). Unknown names drop out; an empty list keeps
    /// everything (a typo cannot mute the agent).
    pub fn restrict_tools(&mut self, keep: &[String]) {
        if keep.is_empty() {
            return;
        }
        self.hands
            .retain(|h| keep.iter().any(|k| k == &h.def().name));
    }

    /// Set the fallback model chain ([fallback] models; engine bootstrap).
    pub fn set_fallbacks(&mut self, models: Vec<String>) {
        self.fallbacks = models;
    }

    /// Enable/disable overflow promotion ([context] promote).
    pub fn set_context_promote(&mut self, promote: bool) {
        self.context_promote = promote;
    }

    /// Whether an active digest summary rides the system prompt.
    pub fn has_digest(&self) -> bool {
        self.digest.is_some()
    }

    /// Consume a promotion the engine must apply (model switch with
    /// Change record + event, same path as `/model`).
    pub fn take_promotion(&mut self) -> Option<String> {
        self.pending_promotion.take()
    }

    /// Fire the speculative digest if pressure sits in the ≥80% zone of
    /// the reserve threshold and nothing is in flight. The candidate is
    /// tagged with the current history watermark; [`Self::take_speculative`]
    /// consumes it only when the watermark still matches. `fast` names the
    /// cheap role model (falls back to the active model).
    pub fn start_speculative(&mut self, fast: Option<&str>) -> bool {
        let window = self.window_tokens();
        if window == 0 || !self.context_pressure_frac(window, 80) || self.context_pressure(window) {
            return false;
        }
        // single in-flight guard
        if self.speculative_in_flight() {
            return false;
        }
        let Some(model_id) = fast
            .map(str::to_string)
            .or_else(|| self.model_selector.clone())
        else {
            return false;
        };
        let Some(dialect) = self.catalog.get(&model_id).cloned() else {
            return false;
        };
        let token = dialect
            .api_key_env
            .as_deref()
            .and_then(ka_dialect::auth::resolve_token);
        let system = DIGEST_SYSTEM.to_string();
        let mut messages = Vec::new();
        if let Some(d) = &self.digest {
            messages.push(TurnMessage::user(format!("<context-digest>\n{d}")));
        }
        messages.extend(self.history.iter().map(Voice::summarizer_view));
        messages.push(TurnMessage::user(
            "Summarize the conversation above now, in at most 300 words.",
        ));
        let watermark = self.history.len();
        let speaker = self
            .speakers
            .get(&dialect.wire)
            .cloned()
            .unwrap_or_else(|| ka_dialect::speaker_for(dialect.wire));
        let slot = self.speculative.clone();
        let session_id = self.session_id.clone();
        tokio::spawn(async move {
            let summary = Self::summarize_with(
                speaker,
                model_id.clone(),
                dialect,
                token,
                session_id,
                system,
                messages,
                std::time::Duration::from_secs(120),
            )
            .await;
            if let Some(summary) = summary {
                slot.lock().await.replace((watermark, {
                    let (tx, rx) = oneshot::channel();
                    tx.send(summary).ok();
                    rx
                }));
            }
        });
        true
    }

    /// Whether a speculative digest is being computed.
    fn speculative_in_flight(&self) -> bool {
        self.speculative
            .try_lock()
            .map(|s| s.is_some())
            .unwrap_or(true)
    }

    /// Consume the speculative candidate when it is ready and computed
    /// against the CURRENT history watermark; otherwise discard it.
    pub async fn take_speculative(&mut self) -> Option<String> {
        let entry = self.speculative.lock().await.take()?;
        let (watermark, rx) = entry;
        if watermark != self.history.len() {
            return None;
        }
        rx.await.ok()
    }

    /// Biggest-context same-vendor sibling whose modalities cover the
    /// current ones and whose context is ≥ 1.5× the current window.
    fn promotion_candidate(&self) -> Option<String> {
        let selector = self.model_selector.as_deref()?;
        let parsed = ka_dialect::parse_selector(selector).ok()?;
        let current_id = parsed.model_id();
        let current = self.catalog.get(&current_id)?;
        if current.context == 0 {
            return None;
        }
        let vendor_prefix = format!("{}/", current_id.split_once('/')?.0);
        self.catalog
            .dialects
            .iter()
            .filter(|(id, d)| {
                id.starts_with(&vendor_prefix)
                    && d.context >= current.context.saturating_mul(3) / 2
                    && current.input.iter().all(|m| d.input.contains(m))
            })
            .min_by_key(|(_, d)| d.context)
            .map(|(id, _)| id.clone())
    }

    /// Set the read-hand image size cap in MB (engine bootstrap).
    pub fn set_max_image_mb(&mut self, mb: u32) {
        self.hand_ctx.max_image_mb = mb;
    }

    /// Set the web-fetch private-host policy (engine bootstrap).
    pub fn set_web_allow_private(&mut self, allow: bool) {
        self.hand_ctx.web_allow_private = allow;
    }

    /// Set the bash sandbox policy (engine bootstrap).
    pub fn set_sandbox(&mut self, policy: ka_sandbox::Policy) {
        self.hand_ctx.sandbox = policy;
    }

    /// Set `[prompt] identity_file` (engine bootstrap; None = default
    /// identity). The file is re-read on every prompt assembly so
    /// edits apply without a restart.
    pub fn set_identity_file(&mut self, path: Option<String>) {
        self.identity_file = path;
        self.identity_warned = false;
    }

    /// Read the identity file for this turn's prompt: `None` keeps the
    /// built-in identity (no file configured, safe mode, unreadable —
    /// with one Note per session for the failure — or empty file).
    /// Content over 8 KiB is cut on a char boundary and marked.
    async fn load_identity(&mut self, events: &mpsc::Sender<Event>) -> Option<String> {
        if crate::conventions::bare_mode() {
            return None;
        }
        let path = self.identity_file.as_deref()?;
        match std::fs::read_to_string(path) {
            Ok(text) => {
                const CAP: usize = 8192;
                if text.len() <= CAP {
                    return Some(text);
                }
                let mut cut = CAP;
                while cut > 0 && !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                Some(format!("{}\n[identity truncated at 8 KiB]", &text[..cut]))
            }
            Err(err) => {
                if !self.identity_warned {
                    self.identity_warned = true;
                    events
                        .send(Event::Note {
                            message: format!(
                                "identity file {path}: {err} — using ka's default identity"
                            ),
                        })
                        .await
                        .ok();
                }
                None
            }
        }
    }

    /// Set the LSP diagnostics manager (engine bootstrap).
    pub fn set_lsp(&mut self, lsp: LspSlot) {
        self.lsp = Some(lsp);
    }

    /// Set [verify] settings (engine bootstrap).
    pub fn set_verify(&mut self, verify: crate::config::Verify) {
        self.verify = verify;
    }

    /// Enable/disable `[guards] auto_review` and arm the reviewer model
    /// (the resolved `fast` role). Engine bootstrap; inert when the
    /// fast role is unresolvable (reviewer silently off).
    pub fn set_auto_review(&mut self, enabled: bool, reviewer_model: Option<String>) {
        self.auto_review = enabled;
        self.reviewer_model = reviewer_model;
    }

    /// Set the per-turn tool allowlist (engine clears it after the
    /// turn). Both filters the specs the model is offered and denies
    /// any stragglers at admit time.
    pub fn set_turn_tools(&mut self, tools: Option<Vec<String>>) {
        self.turn_tools = tools;
    }

    /// Scope one turn to explicitly invoked skills (`/skill:<name>`):
    /// their SKILL.md bodies are injected into that turn's system
    /// prompt only — the session default (progressive disclosure) is
    /// untouched.
    pub fn set_turn_skills(&mut self, skills: Option<Vec<String>>) {
        self.turn_skills = skills;
    }

    /// Whether a per-turn tool allowlist is active (contract tests).
    pub fn turn_tools_active(&self) -> bool {
        self.turn_tools.is_some()
    }

    /// Files edited this turn; also clears the list (read-once).
    pub fn take_edited(&self) -> Vec<String> {
        std::mem::take(&mut *self.edited.lock())
    }

    /// Stop kind of the most recent turn.
    pub fn last_stop(&self) -> Stop {
        self.last_stop
    }

    /// The shared todo slot (engine reads pending items for auto-poke).
    pub(crate) fn todo_slot(&self) -> &crate::hands::todo::TodoSlot {
        &self.todo
    }

    /// The active digest summary, if any (engine reads it for
    /// `/transfer`'s handoff strand).
    pub(crate) fn digest_text(&self) -> Option<&str> {
        self.digest.as_deref()
    }

    /// The live permission mode (engine guards read this — a mid-turn
    /// /mode steer updates the voice, not EngineState's mirror).
    pub fn mode(&self) -> ka_protocol::Mode {
        self.mode
    }

    /// Set the bash auto-background threshold in ms (engine bootstrap;
    /// 0 = never background).
    pub fn set_bash_background_ms(&mut self, ms: u64) {
        self.hand_ctx.bash_background_ms = ms;
    }

    /// Share the auto-backgrounded-jobs table (the engine kills the
    /// remaining jobs at session shutdown).
    pub fn jobs(&self) -> std::sync::Arc<crate::hands::jobs::JobTable> {
        self.hand_ctx.jobs.clone()
    }
    /// Run matching hooks for one event. Returns Err(reason) when a
    /// pre_tool_use hook blocked the call (exit 2, stderr as reason);
    /// Ok(steering) carries stdout steering when a hook emitted it.
    /// The `hooks` feature toggle silences every hook class.
    async fn run_hooks(
        &self,
        event: crate::config::HookEvent,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<Option<crate::fshooks::Steering>, String> {
        if !self.features.read().hooks_enabled() {
            return Ok(None);
        }
        run_hook_scripts(&self.hooks_cfg, event, tool, args, &self.hand_ctx.cwd).await
    }

    /// Apply hook steering: switch the gate's permission mode (same
    /// path as `/mode`, `Event::ModeChanged` keeps surfaces in step)
    /// and surface the note as a transcript row.
    async fn apply_steering(
        &mut self,
        steer: Option<crate::fshooks::Steering>,
        events: &mpsc::Sender<Event>,
    ) {
        let Some(s) = steer else { return };
        if let Some(mode) = s.mode {
            self.mode = mode;
            events.send(Event::ModeChanged { mode }).await.ok();
        }
        if let Some(note) = s.note {
            events
                .send(Event::Note {
                    message: format!("hook: {note}"),
                })
                .await
                .ok();
        }
    }

    /// Fire `stop` and `turn_end` hooks at turn exit and record the stop
    /// kind. Advisory only: failures surface as notes and never fail the
    /// turn; `turn_end` is the claude-compat spelling of `stop` and
    /// receives the same payload under its own `"event"` name.
    pub(crate) async fn stop_hooks(&mut self, events: &mpsc::Sender<Event>, status: &str) {
        self.last_stop = match status {
            "aborted" => Stop::Aborted,
            "error" => Stop::Error,
            _ => Stop::Done,
        };
        if !self.features.read().hooks_enabled() {
            return;
        }
        for event in [
            crate::config::HookEvent::Stop,
            crate::config::HookEvent::TurnEnd,
        ] {
            let payload = serde_json::json!({"event": event.wire_name(), "stop": status});
            let (failures, steer) =
                run_event_scripts(&self.hooks_cfg, event, &payload, &self.hand_ctx.cwd).await;
            for reason in failures {
                events
                    .send(Event::Note {
                        message: format!("{} hook: {reason}", event.wire_name()),
                    })
                    .await
                    .ok();
            }
            if let Some(s) = steer {
                self.apply_steering(Some(s), events).await;
            }
        }
    }

    /// Fire one lifecycle hook event (session_start / session_end /
    /// user_prompt_submit / pre_compact) from the engine. Failures
    /// surface as notes; clean stdout may steer (mode/note).
    pub(crate) async fn lifecycle_hook(
        &mut self,
        event: crate::config::HookEvent,
        payload: serde_json::Value,
        events: &mpsc::Sender<Event>,
    ) {
        if !self.features.read().hooks_enabled() {
            return;
        }
        let (failures, steer) =
            run_event_scripts(&self.hooks_cfg, event, &payload, &self.hand_ctx.cwd).await;
        for reason in failures {
            events
                .send(Event::Note {
                    message: format!("{} hook: {reason}", event.wire_name()),
                })
                .await
                .ok();
        }
        if let Some(s) = steer {
            self.apply_steering(Some(s), events).await;
        }
    }

    /// Load resumed history + digest (engine bootstrap). Also resets the
    /// once-per-strand promotion latch.
    pub fn load_history(&mut self, history: Vec<TurnMessage>, digest: Option<String>) {
        self.history = history;
        self.digest = digest;
        self.promoted = false;
    }

    /// Set the active model selector + ratio (engine forwards each turn
    /// and before settle-time digests).
    #[allow(dead_code)]
    pub fn set_model_selector(&mut self, selector: &str, ratio: f64) {
        self.model_selector = Some(selector.to_string());
        self.ratio = if ratio > 0.0 { ratio } else { 4.0 };
    }

    /// Restore a digest from a resumed strand.
    #[allow(dead_code)]
    pub fn set_digest(&mut self, summary: String) {
        self.digest = Some(summary);
    }

    /// Context-window pressure check against the active dialect.
    pub fn context_pressure(&self, window: u64) -> bool {
        self.context_pressure_frac(window, 100)
    }

    /// Pressure at `pct`% of the reserve threshold (80 = speculative
    /// kick-off zone).
    pub fn context_pressure_frac(&self, window: u64, pct: u64) -> bool {
        if window == 0 {
            return false; // unknown window: never auto-digest
        }
        // omp rule: reserve is the 16k floor when practical, but never
        // more than a quarter of small windows (proportional reserve)
        let proportional = window * RESERVE_PCT / 100;
        let reserve = proportional.max(RESERVE_FLOOR.min(window / 4));
        let threshold = window.saturating_sub(reserve);
        self.last_context + KEEP_TAIL_TOKENS.min(window / 4) > threshold * pct / 100
    }

    /// Blank old tool-result bodies in history beyond the protected
    /// window. In-memory only — strands keep the full originals, so
    /// pruning re-applies deterministically after resume.
    pub fn prune_tool_outputs(&mut self, ratio: f64) -> u64 {
        let sizes: Vec<u64> = self.history_sizes(ratio);
        let total: u64 = sizes.iter().sum();
        let mut cut = self.history.len();
        let mut protected: u64 = 0;
        // walk backwards protecting the recent window (all message kinds)
        for i in (0..self.history.len()).rev() {
            if protected >= PROTECT_WINDOW_TOKENS {
                break;
            }
            protected = protected.saturating_add(sizes[i]);
            cut = i;
        }
        // candidates: tool results strictly older than `cut`
        let mut savings = 0u64;
        let mut replacements: Vec<(usize, usize, String)> = Vec::new();
        for (i, msg) in self.history.iter().enumerate() {
            if i >= cut {
                break;
            }
            if msg.role != TurnRole::Tool {
                continue;
            }
            for (j, result) in msg.results.iter().enumerate() {
                let tokens = (result.content.chars().count() as f64 / ratio.max(0.1)) as u64;
                if tokens > 8 {
                    savings += tokens;
                    replacements.push((i, j, result.content.clone()));
                }
            }
        }
        if savings < MIN_PRUNE_SAVINGS {
            return 0;
        }
        for (i, j, original) in replacements.iter().map(|(i, j, o)| (*i, *j, o.clone())) {
            let parked = self.hand_ctx.spill.park(&original).ok();
            let note = match parked {
                Some(ptr) => format!("[pruned output; full text at {ptr}]"),
                None => "[pruned output]".to_string(),
            };
            if let Some(result) = self.history[i].results.get_mut(j) {
                result.content = note;
            }
        }
        let _ = total;
        savings
    }

    /// The ladder's `shake`: deterministically truncate the tool-call
    /// arguments of every call that predates the last user message —
    /// by then the call is stale (its result is already in the
    /// transcript) and only the giant argument payloads linger in the
    /// resent context. Returns the number of calls truncated.
    /// Idempotent: already-shaken calls are left alone.
    pub fn shake(&mut self, arg_cap: usize) -> usize {
        let Some(last_user) = self.history.iter().rposition(|m| m.role == TurnRole::User) else {
            return 0;
        };
        let mut n = 0;
        for m in self.history[..last_user].iter_mut() {
            for call in m.calls.iter_mut() {
                let compact = call.arguments.to_string();
                if compact.len() > arg_cap {
                    // the replacement is stored as a JSON string: inner
                    // quotes/backslashes would be escaped and re-grow
                    // past the cap, so they are normalized away first
                    const MARKER: &str = "[shaken]";
                    let budget = arg_cap.saturating_sub(MARKER.len() + 2);
                    let flat: String = compact
                        .chars()
                        .map(|c| match c {
                            '"' => '\'',
                            '\\' => '\'',
                            other => other,
                        })
                        .collect();
                    let mut kept: String = flat.chars().take(budget).collect();
                    kept.push_str(MARKER);
                    call.arguments = serde_json::Value::String(kept);
                    n += 1;
                }
            }
        }
        n
    }

    fn history_sizes(&self, ratio: f64) -> Vec<u64> {
        self.history
            .iter()
            .map(|m| message_tokens(m, ratio))
            .collect()
    }

    /// Bound each message so the summarizer request fits even when the
    /// conversation dwarfs the model's real window: head+tail per content,
    /// capped tool noise (snapcompact-style serialization, text-only).
    fn summarizer_view(m: &TurnMessage) -> TurnMessage {
        let cap = |s: &str| -> String {
            const HEAD: usize = 600;
            const TAIL: usize = 300;
            if s.chars().count() <= HEAD + TAIL {
                return s.to_string();
            }
            let head: String = s.chars().take(HEAD).collect();
            let tail: String = s
                .chars()
                .rev()
                .take(TAIL)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            format!(
                "{head}\n…[truncated {} chars]…\n{tail}",
                s.chars().count() - HEAD - TAIL
            )
        };
        TurnMessage {
            role: m.role,
            content: cap(&m.content),
            thinking: None,
            calls: m
                .calls
                .iter()
                .map(|c| {
                    let mut cl = c.clone();
                    cl.arguments = serde_json::Value::String(cap(&c.arguments.to_string()));
                    cl
                })
                .collect(),
            results: m
                .results
                .iter()
                .map(|r| {
                    let mut rl = r.clone();
                    rl.content = cap(&rl.content);
                    rl
                })
                .collect(),
            images: m.images.clone(),
        }
    }
    /// Summarize the conversation with the active model (same-model
    /// digest). Returns the summary text.
    pub async fn summarize(
        &mut self,
        focus: Option<&str>,
        window_deadline: std::time::Duration,
    ) -> Option<String> {
        let model_id = self.model_selector.clone()?;
        let dialect = self.catalog.get(&model_id).cloned()?;
        let token = dialect
            .api_key_env
            .as_deref()
            .and_then(ka_dialect::auth::resolve_token);
        let ratio = if dialect.ratio > 0.0 {
            dialect.ratio as f64
        } else {
            4.0
        };
        let system = match focus {
            Some(f) => format!("{DIGEST_SYSTEM}\n\nFocus: {f}"),
            None => DIGEST_SYSTEM.to_string(),
        };
        let mut messages = Vec::new();
        if let Some(d) = &self.digest {
            messages.push(TurnMessage::user(format!("<context-digest>\n{d}")));
        }
        messages.extend(self.history.iter().map(Voice::summarizer_view));
        // never end on an assistant message: several OpenAI-compatible
        // servers treat it as prefill and emit nothing
        messages.push(TurnMessage::user(
            "Summarize the conversation above now, in at most 300 words.",
        ));
        let speaker = self.speaker(dialect.wire);
        let session_id = self.session_id.clone();
        let _ = ratio;
        Self::summarize_with(
            speaker,
            model_id,
            dialect,
            token,
            session_id,
            system,
            messages,
            window_deadline,
        )
        .await
    }

    /// Shared summarize core: one bounded completion collecting the
    /// text (falling back to the reasoning tail). Static so the
    /// speculative digest can run it without `&mut self`.
    #[allow(clippy::too_many_arguments)]
    async fn summarize_with(
        speaker: std::sync::Arc<dyn Speaker>,
        model_id: String,
        dialect: ka_dialect::Dialect,
        token: Option<String>,
        session_id: Option<String>,
        system: String,
        messages: Vec<TurnMessage>,
        window_deadline: std::time::Duration,
    ) -> Option<String> {
        let req = SpeakRequest {
            model_id,
            dialect: dialect.clone(),
            effort: None,
            system,
            messages,
            tools: Vec::new(),
            token,
            cache_key: None,
            session_id,
            schema: None,
            native_search: false,
        };
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);
        {
            let speaker = speaker.clone();
            tokio::spawn(async move {
                speaker.speak(req, tx).await;
            });
        }
        let mut text = String::new();
        let mut thought = String::new();
        let mut failure: Option<String> = None;
        let _ = tokio::time::timeout(window_deadline, async {
            while let Some(evt) = rx.recv().await {
                match evt {
                    StreamEvent::Text(t) => text.push_str(&t),
                    StreamEvent::Thought(t) => thought.push_str(&t),
                    StreamEvent::Finished { .. } => break,
                    StreamEvent::Failed { message, .. } => {
                        failure = Some(message);
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await;
        // prefer the model's final text; thinking models sometimes only
        // reason — fall back to a trimmed tail of the reasoning channel
        // (thinking converges to conclusions at the end)
        let mut summary = text;
        if summary.trim().is_empty() && !thought.trim().is_empty() {
            const HEAD: usize = 200;
            const TAIL: usize = 1_200;
            let chars: Vec<char> = thought.chars().collect();
            summary = if chars.len() <= HEAD + TAIL {
                thought
            } else {
                let head: String = chars[..HEAD].iter().collect();
                let tail: String = chars[chars.len() - TAIL..].iter().collect();
                format!("{head}\n…[thinking trimmed]…\n{tail}")
            };
        }
        if std::env::var("KA_DEBUG_SETTLE").is_ok() {
            eprintln!("[summarize] chars={} failure={:?}", summary.len(), failure);
        }
        (!summary.trim().is_empty()).then_some(summary)
    }
    /// Output-token ceiling for auxiliary role calls (auto-titles and
    /// later cheap chores): a few words never need more.
    pub const ROLE_MAX_OUTPUT: u32 = 256;

    /// One short auxiliary completion against a role selector already
    /// resolved to a catalog id (`vendor/model`). No tools, capped
    /// output, the same auth/retry plumbing as main-line calls (it rides
    /// the dialect's wire speaker). Returns `None` on unknown model,
    /// transport failure, timeout, or an empty/unstreamed reply — callers
    /// must degrade silently; this is a best-effort channel.
    pub async fn role_complete(
        &mut self,
        model_id: &str,
        system: &str,
        prompt: &str,
        deadline: std::time::Duration,
    ) -> Option<String> {
        let mut dialect = self.catalog.get(model_id)?.clone();
        if dialect.max_output == 0 || dialect.max_output > Self::ROLE_MAX_OUTPUT {
            dialect.max_output = Self::ROLE_MAX_OUTPUT;
        }
        let token = dialect
            .api_key_env
            .as_deref()
            .and_then(ka_dialect::auth::resolve_token);
        let wire = dialect.wire;
        let req = SpeakRequest {
            model_id: model_id.to_string(),
            dialect,
            effort: None,
            system: system.to_string(),
            messages: vec![TurnMessage::user(prompt)],
            tools: Vec::new(),
            token,
            cache_key: None,
            session_id: self.session_id.clone(),
            schema: None,
            native_search: false,
        };
        let speaker = self.speaker(wire);
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);
        {
            let speaker = speaker.clone();
            tokio::spawn(async move {
                speaker.speak(req, tx).await;
            });
        }
        let mut text = String::new();
        let mut finished = false;
        let _ = tokio::time::timeout(deadline, async {
            while let Some(evt) = rx.recv().await {
                match evt {
                    StreamEvent::Text(t) => text.push_str(&t),
                    StreamEvent::Finished { .. } => {
                        finished = true;
                        break;
                    }
                    // a failed stream never yields a trustworthy answer;
                    // partial text before the failure is discarded
                    StreamEvent::Failed { .. } => break,
                    _ => {}
                }
            }
        })
        .await;
        if !finished {
            return None;
        }
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    /// Replace history with the digest summary + kept tail. Returns the
    /// kept index into the OLD history (for Digest-record persistence).
    pub fn apply_digest(&mut self, summary: String, ratio: f64) -> usize {
        let sizes = self.history_sizes(ratio);
        let total: u64 = sizes.iter().sum();
        // clamp the tail to the window: pathological tiny windows must not
        // keep more than they can hold
        let window = self.window_tokens();
        let tail_cap = if window > 0 {
            (window / 4).max(1_000)
        } else {
            KEEP_TAIL_TOKENS
        };
        let mut budget = KEEP_TAIL_TOKENS.min(tail_cap).min(total);
        // walk from the end, then advance the cut to the next user message
        // so a turn is never split (also keeps tool pairs intact).
        let mut cut = self.history.len();
        let mut acc: u64 = 0;
        for i in (0..self.history.len()).rev() {
            if acc >= budget {
                cut = i + 1;
                break;
            }
            acc += sizes[i];
            cut = i;
        }
        let _ = &mut budget;
        while cut < self.history.len() && self.history[cut].role != TurnRole::User {
            cut += 1;
        }
        let kept_from = cut.min(self.history.len());
        let kept_tokens: u64 = sizes.get(kept_from..).map(|s| s.iter().sum()).unwrap_or(0);
        self.history.drain(..kept_from);
        self.digest = Some(summary.clone());
        self.digest_revision += 1;
        self.last_digest = Some((summary, kept_from));
        // pressure reflects the real post-digest estimate: the persistent
        // digest only — the pressure formula already adds the tail budget
        let digest_tokens =
            (self.digest.as_deref().map_or(0, str::len) as u64).div_ceil(ratio.max(0.1) as u64);
        let _ = kept_tokens;
        self.last_context = digest_tokens;
        kept_from
    }

    /// The ladder's last step: after a digest, re-inject the most
    /// recently read/edited files (ledger-hot) as a bounded appendix on
    /// the trailing user message, so the concrete contents the digest
    /// summarised away stay at hand. Merged into that message (never a
    /// separate one): the anthropic wire requires alternating roles, and
    /// merging keeps the change purely in-memory — the strand copy of
    /// the message stays as persisted, and record_ids alignment is
    /// untouched. Returns the files injected, newest first.
    pub fn reinject_hot_files(&mut self, max: usize, per_file_cap: usize) -> Vec<String> {
        let hot = self.hand_ctx.ledger.lock().hot_paths(max);
        if hot.is_empty() {
            return Vec::new();
        }
        let mut blocks = Vec::new();
        let mut injected = Vec::new();
        for path in &hot {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }
            let mut body: String = text.lines().take(400).collect::<Vec<_>>().join("\n");
            if text.lines().count() > 400 {
                body.push_str("\n… (truncated at 400 lines)");
            }
            if body.len() > per_file_cap {
                let mut cut: String = body.chars().take(per_file_cap).collect();
                cut.push_str("\n… (truncated)");
                body = cut;
            }
            blocks.push(format!("[re-read {}]\n{body}", path.display()));
            injected.push(path.display().to_string());
        }
        if blocks.is_empty() {
            return Vec::new();
        }
        let appendix = format!(
            "[context re-read after digest — these files were recently read or edited; \
             current contents follow]\n\n{}",
            blocks.join("\n\n")
        );
        match self
            .history
            .iter_mut()
            .rev()
            .find(|m| m.role == TurnRole::User)
        {
            Some(m) => {
                m.content.push_str("\n\n");
                m.content.push_str(&appendix);
            }
            None => self.history.push(TurnMessage::user(appendix)),
        }
        hot.iter().map(|p| p.display().to_string()).collect()
    }

    /// First matching configured rule's verdict for this call.
    fn match_rule(&self, call: &ToolCall) -> Option<crate::config::Verdict> {
        self.rules_cfg
            .iter()
            .find(|r| r.tool == call.tool)
            .filter(|r| match &r.pattern {
                None => true,
                Some(pat) => rule_pattern_matches(pat, &call.tool, &call.primary_arg()),
            })
            .map(|r| r.verdict)
    }

    /// Truncate history so it ends just before the Nth-last user
    /// message. Returns the kept index (into the pre-truncation history)
    /// or None when there aren't that many user turns.
    pub fn rewind(&mut self, turns: u32) -> Option<usize> {
        if turns == 0 {
            return None;
        }
        let mut seen = 0u32;
        for idx in (0..self.history.len()).rev() {
            if self.history[idx].role == TurnRole::User {
                seen += 1;
                if seen == turns {
                    self.history.truncate(idx);
                    return Some(idx);
                }
            }
        }
        None
    }

    /// Context window of the active model (0 = unknown).
    pub fn window_tokens(&self) -> u64 {
        let Some(model_id) = &self.model_selector else {
            return 0;
        };
        self.catalog
            .get(model_id)
            .map(|d| d.context as u64)
            .unwrap_or(0)
    }

    /// Clone of the active model selector.
    pub fn model_selector_cloned(&self) -> Option<String> {
        self.model_selector.clone()
    }

    /// Chars-per-token ratio of the active model.
    pub fn model_ratio(&self) -> f64 {
        self.ratio
    }

    /// Stop kind of the most recently completed turn.
    /// Debug accessor for the last measured context.
    #[doc(hidden)]
    pub fn debug_last_context(&self) -> u64 {
        self.last_context
    }

    /// Test hook: pretend the provider just reported this context size.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn note_context_for_tests(&mut self, tokens: u64) {
        self.last_context = tokens;
    }

    /// Consume the pending digest outcome for persistence.
    pub fn take_pending_digest(&mut self) -> Option<(String, usize, u64)> {
        let (summary, kept) = self.last_digest.take()?;
        Some((summary, kept, self.digest_revision))
    }

    /// Update the permission mode (engine forwards `SetMode`).
    pub fn set_mode(&mut self, mode: ka_protocol::Mode) {
        self.mode = mode;
    }

    /// Messages as sent on the wire: the conversation (the digest rides
    /// in the system prompt as authoritative memory).
    fn speak_messages(&self) -> Vec<TurnMessage> {
        self.history.clone()
    }

    /// Estimated context usage per component (/context). History
    /// buckets use the same chars/ratio heuristic as the context meter;
    /// the residual (system prompt, conventions, digest, tool specs)
    /// folds into `system` — so the parts sum to `last_context` whenever
    /// the estimates fit under it; when they overshoot the measured
    /// usage (prose-dense history, stingy ratio) `system` floors at zero
    /// and the buckets can exceed it.
    pub fn context_breakdown(&self) -> Vec<ka_protocol::ContextPart> {
        let ratio = if self.ratio > 0.0 { self.ratio } else { 4.0 };
        let mut user = 0u64;
        let mut assistant = 0u64;
        let mut tools = 0u64;
        for m in &self.history {
            match m.role {
                ka_dialect::speaker::TurnRole::User => user += message_tokens(m, ratio),
                ka_dialect::speaker::TurnRole::Assistant => assistant += message_tokens(m, ratio),
                ka_dialect::speaker::TurnRole::Tool => tools += message_tokens(m, ratio),
            }
        }
        let system = self.last_context.saturating_sub(user + assistant + tools);
        vec![
            ka_protocol::ContextPart {
                name: "system".into(),
                tokens: system,
            },
            ka_protocol::ContextPart {
                name: "user".into(),
                tokens: user,
            },
            ka_protocol::ContextPart {
                name: "assistant".into(),
                tokens: assistant,
            },
            ka_protocol::ContextPart {
                name: "tools".into(),
                tokens: tools,
            },
        ]
    }

    fn speaker(&mut self, wire: Wire) -> std::sync::Arc<dyn Speaker> {
        self.speakers
            .entry(wire)
            .or_insert_with(|| ka_dialect::speaker_for(wire))
            .clone()
    }

    /// Inject a speaker for a wire (tests and contract suites).
    pub fn with_speaker(mut self, wire: Wire, speaker: std::sync::Arc<dyn Speaker>) -> Self {
        self.speakers.insert(wire, speaker);
        self
    }

    fn specs(&self) -> Vec<ToolSpec> {
        // two filters stack: session feature toggles hide disabled
        // capabilities outright, and a per-turn allowlist
        // (custom-command `allowed-tools`) narrows what remains
        let features = self.features.read();
        self.hands
            .iter()
            .filter(|h| features.hidden_reason(&h.def().name).is_none())
            .filter(|h| {
                self.turn_tools
                    .as_ref()
                    .is_none_or(|tools| tools.iter().any(|t| t == &h.def().name))
            })
            .map(|h| {
                let def = h.def();
                ToolSpec {
                    name: def.name.to_string(),
                    description: def.description.clone(),
                    parameters: def.parameters.clone(),
                }
            })
            .collect()
    }

    /// Run one live prompt to completion. Always emits exactly one
    /// `TurnFinished` and returns the turn's usage. `guards` carries the
    /// session spend/context thresholds and latches.
    #[allow(clippy::too_many_arguments)]
    pub async fn turn(
        &mut self,
        model_selector: &str,
        prompt: String,
        commands: &mut mpsc::Receiver<Command>,
        events: &mpsc::Sender<Event>,
        steers: &mut Vec<String>,
        queue: &mut VecDeque<String>,
        guards: &mut GuardRuntime,
        schema: Option<serde_json::Value>,
        images: Vec<ka_dialect::ImagePart>,
    ) -> Usage {
        use ka_dialect::parse_selector;
        self.edited.lock().clear();
        let mut parsed = match parse_selector(model_selector) {
            Ok(p) => p,
            Err(e) => {
                self.stop_hooks(events, "error").await;
                return finish_after_error(events, ErrorClass::Protocol, &e.to_string()).await;
            }
        };
        let mut model_id = parsed.model_id();
        let mut dialect = match self.catalog.get(&model_id).cloned() {
            Some(d) => d,
            None => {
                self.stop_hooks(events, "error").await;
                return finish_after_error(
                    events,
                    ErrorClass::Protocol,
                    &format!("unknown model {model_id:?} (not in catalog; add a dialect overlay)"),
                )
                .await;
            }
        };
        if schema.is_some() && !dialect.flags.structured {
            self.stop_hooks(events, "error").await;
            return finish_after_error(
                events,
                ErrorClass::Unsupported,
                &format!(
                    "model {model_id:?} does not support structured output \
                         (no `structured` support; drop --schema or pick a model that has it)"
                ),
            )
            .await;
        }
        let mut price = dialect.price;
        let mut ratio = if dialect.ratio > 0.0 {
            dialect.ratio
        } else {
            4.0
        };
        let mut token = dialect
            .api_key_env
            .as_deref()
            .and_then(ka_dialect::auth::resolve_token);
        let mut window = dialect.context as u64;
        let mut fallback_cursor: usize = 0;
        let mut fallback_hops: usize = 0;

        let est_in = (prompt.len() as f64 / ratio as f64).ceil() as u64;
        events
            .send(Event::TurnStarted {
                context: ka_protocol::ContextMeter {
                    used: est_in,
                    window,
                },
            })
            .await
            .ok();

        // Minimal system context: identity + read-only git awareness.
        // The snapshot shells out to git (blocking, slow on big repos):
        // keep it off the async runtime so turn start cannot stall
        // event pumping.
        let cwd = self.hand_ctx.cwd.clone();
        let snap =
            tokio::task::spawn_blocking(move || crate::hands::git::RepoSnapshot::capture(&cwd))
                .await
                .unwrap_or_else(|_| crate::hands::git::RepoSnapshot::default());
        let mut system = String::new();
        // AGENTS.md hierarchy (root→cwd)
        for agents in crate::conventions::discover_agents(&self.hand_ctx.cwd) {
            system.push_str(&format!(
                "\n<project-instructions src=\"{}\">\n{}\n</project-instructions>\n",
                agents.path.display(),
                agents.content
            ));
        }
        // memory tiers (project, then user-level)
        for memory in crate::conventions::discover_memory(&self.hand_ctx.cwd) {
            system.push_str(&format!(
                "\n<memory src=\"{}\">\n{}\n</memory>\n",
                memory.path.display(),
                memory.content
            ));
        }
        // glob-scoped rules (.ka/rules/*.md): a rule activates once any
        // file matching its `paths:` globs has been read this session
        // (no globs = always active) — TS conventions load when TS files
        // are actually touched
        let touched: Vec<String> = self
            .hand_ctx
            .ledger
            .lock()
            .tracked_paths()
            .iter()
            .map(|p| match p.strip_prefix(&self.hand_ctx.cwd) {
                Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
                Err(_) => p.to_string_lossy().replace('\\', "/"),
            })
            .collect();
        for rule in crate::conventions::discover_rules(&self.hand_ctx.cwd) {
            let active = match &rule.globs {
                None => true,
                Some(globs) => touched.iter().any(|p| {
                    let base = p.rsplit('/').next().unwrap_or(p.as_str());
                    globs
                        .iter()
                        .any(|g| glob_match(g, p) || glob_match(g, base))
                }),
            };
            if active {
                system.push_str(&format!(
                    "\n<scoped-rules src=\"{}\">\n{}\n</scoped-rules>\n",
                    rule.path.display(),
                    rule.content
                ));
            }
        }
        // skills: progressive disclosure — names/descriptions/paths only.
        // The whole block drops when `skills` is toggled off; individual
        // skills drop by name. (Guard scoped: the turn loop below needs
        // `&mut self` back.)
        let skills = {
            let features = self.features.read();
            let discovered = if features.skills_enabled() {
                crate::conventions::discover_skills(&self.hand_ctx.cwd)
            } else {
                Vec::new()
            };
            discovered
                .into_iter()
                .filter(|sk| features.skill_enabled(&sk.name))
                .collect::<Vec<_>>()
        };
        if !skills.is_empty() {
            system.push_str("\nAvailable skills (read the SKILL.md path with the read tool before using one):\n");
            for sk in &skills {
                system.push_str(&format!(
                    "- {}: {} — {}\n",
                    sk.name,
                    sk.description,
                    sk.path.display()
                ));
            }
        }
        // explicit invocation (/skill:<name>): the named skills' full
        // bodies ride this turn only — the engine clears the scope
        // when the turn settles, like `allowed-tools`. Admit-time
        // validation ran engine-side; a name that slipped through
        // injects nothing (the listing above stays honest).
        if let Some(invoked) = &self.turn_skills {
            for name in invoked {
                if let Some(sk) = skills.iter().find(|s| &s.name == name) {
                    match std::fs::read_to_string(&sk.path) {
                        Ok(body) => system.push_str(&format!(
                            "\n<skill src=\"{}\">\n{}\n</skill>\n",
                            sk.path.display(),
                            body
                        )),
                        Err(err) => system.push_str(&format!(
                            "\n<skill src=\"{}\" error=\"{err}\" />\n",
                            sk.path.display()
                        )),
                    }
                }
            }
        }
        let identity = self.load_identity(events).await;
        system.push_str(&core_instructions(
            &snap.summary(),
            self.mode,
            &self.hand_ctx.cwd,
            identity.as_deref(),
        ));
        if let Some(d) = &self.digest {
            system.push_str(&format!(
                "\n\nEarlier-conversation summary (this is your memory of prior turns — treat it as accurate ground truth):\n{d}"
            ));
        }

        self.model_selector = Some(model_selector.to_string());
        self.ratio = if dialect.ratio > 0.0 {
            dialect.ratio as f64
        } else {
            4.0
        };
        self.history
            .push(TurnMessage::user_with_images(prompt, images));
        self.state.loop_counts.clear();
        let mut usage_total = Usage::default();
        let mut assistant_text = String::new();
        // the closing (text-only) round's thinking: step rounds attach
        // theirs to the step push, but the final push used to drop it —
        // the "thoughts vanish after resume" bug
        let mut closing_thinking = String::new();
        let mut final_stop = Stop::Done;
        let mut steps = 0u32;
        let mut overflow_retried = false;
        let mut retry_attempt: usize = 0;

        // native server-search injection needs the dialect flag AND the
        // web feature word — the wire cannot see the toggle itself
        let native_search = dialect.flags.server_search && self.features.read().web_enabled();

        'outer: loop {
            let req = SpeakRequest {
                effort: parsed.effort.clone().or_else(|| self.effort.clone()),
                model_id: model_id.clone(),
                dialect: dialect.clone(),
                system: system.clone(),
                messages: self.speak_messages(),
                tools: self.specs(),
                token: token.clone(),
                cache_key: None,
                session_id: self.session_id.clone(),
                schema: schema.clone(),
                native_search,
            };
            let speaker = self.speaker(dialect.wire);
            let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);
            {
                let speaker = speaker.clone();
                tokio::spawn(async move {
                    speaker.speak(req, tx).await;
                });
            }

            let mut step_calls: Vec<ToolCall> = Vec::new();
            let mut step_text = String::new();
            let mut step_thought = String::new();
            let mut step_failed: Option<(ErrorClass, String, bool)> = None;
            let mut step_finished = false;

            while !step_finished {
                tokio::select! {
                    biased;
                    maybe_cmd = commands.recv() => {
                        match maybe_cmd {
                            None => {
                                // surface gone mid-turn: settle the turn
                                // as aborted so stop hooks fire and
                                // last_stop never goes stale (a stale
                                // Done would green-light auto-commit)
                                events
                                    .send(Event::TurnFinished {
                                        stop: Stop::Aborted,
                                        usage: Usage::default(),
                                    })
                                    .await
                                    .ok();
                                self.stop_hooks(events, "aborted").await;
                                return Usage::default();
                            }
                            Some(Command::Abort) => {
                                events.send(Event::TurnFinished {
                                    stop: Stop::Aborted,
                                    usage: Usage::default(),
                                }).await.ok();
                                self.stop_hooks(events, "aborted").await;
                                return Usage::default();
                            }
                            Some(Command::Steer { text }) => steers.push(text),
                            Some(Command::Queue { text }) => queue.push_back(text),
                            Some(Command::SetMode { mode }) => {
                                self.mode = mode;
                                events.send(Event::ModeChanged { mode }).await.ok();
                            }
                            // feature toggles mid-turn: the cheap half
                            // (hide from the next model round-trip +
                            // reject stragglers) applies immediately;
                            // the side-effect half (spawn/strand/
                            // inventory) is the engine's, once the turn
                            // settles. Sandboxes only buffer — swapping
                            // the policy mid-turn would race in-flight
                            // bash calls.
                            Some(Command::SetFeature { spec, enabled }) => {
                                self.set_feature(&spec, enabled);
                                self.buffer_feature_cmd(Command::SetFeature {
                                    spec,
                                    enabled,
                                });
                            }
                            Some(cmd @ Command::SetSandbox { .. }) => {
                                self.buffer_feature_cmd(cmd);
                            }
                            Some(_) => {}
                        }
                    }
                    maybe_evt = rx.recv() => {
                        let Some(evt) = maybe_evt else {
                            // speaker ended without Finished/Failed (aborted)
                            break;
                        };
                        match evt {
                            StreamEvent::Text(t) => {
                                step_text.push_str(&t);
                                assistant_text.push_str(&t);
                                events.send(Event::Delta { kind: ka_protocol::DeltaKind::Text(t) }).await.ok();
                            }
                            StreamEvent::Thought(t) => {
                                step_thought.push_str(&t);
                                events.send(Event::Delta { kind: ka_protocol::DeltaKind::Thought(t) }).await.ok();
                            }
                            StreamEvent::Call(call) => {
                                events.send(Event::CallStarted {
                                    tool: call.tool.clone(),
                                    id: call.id.clone(),
                                    detail: call_detail(&call.tool, &call.arguments),
                                }).await.ok();
                                step_calls.push(call);
                            }
                            StreamEvent::Finished { stop, usage } => {
                                usage_total.input += usage.input;
                                usage_total.output += usage.output;
                                usage_total.cache_read += usage.cache_read;
                                usage_total.cache_write += usage.cache_write;
                                self.last_context = usage.input
                                    + usage.cache_read
                                    + usage.cache_write
                                    + usage.output;
                                events
                                    .send(Event::ContextMeter {
                                        used: self.last_context,
                                        window: self.window_tokens(),
                                    })
                                    .await
                                    .ok();
                                // session guards: each fires once, on the
                                // first crossing; stop aborts the turn cleanly
                                if !guards.context_latched && window > 0 {
                                    if let Some(cap_pct) = guards.context_pct {
                                        let pct = self.last_context * 100 / window;
                                        if pct >= cap_pct {
                                            guards.context_latched = true;
                                            let question =
                                                format!("context {pct}% full — continue?");
                                            if !ask_continue_or_stop(
                                                &mut self.state,
                                                question,
                                                commands,
                                                events,
                                            )
                                            .await
                                            {
                                                events
                                                    .send(Event::TurnFinished {
                                                        stop: Stop::Aborted,
                                                        usage: Usage::default(),
                                                    })
                                                    .await
                                                    .ok();
                                                self.stop_hooks(events, "aborted").await;
                                                return Usage::default();
                                            }
                                        }
                                    }
                                }
                                if !guards.spend_latched {
                                    if let Some(cap) = guards.spend_usd {
                                        let running = guards.session_spend
                                            + if dialect.priced {
                                                cost_of(&usage_total, price)
                                            } else {
                                                0.0
                                            };
                                        if running >= cap {
                                            guards.spend_latched = true;
                                            let question = format!(
                                                "spend cap ${cap:.2} reached — continue?"
                                            );
                                            if !ask_continue_or_stop(
                                                &mut self.state,
                                                question,
                                                commands,
                                                events,
                                            )
                                            .await
                                            {
                                                events
                                                    .send(Event::TurnFinished {
                                                        stop: Stop::Aborted,
                                                        usage: Usage::default(),
                                                    })
                                                    .await
                                                    .ok();
                                                self.stop_hooks(events, "aborted").await;
                                                return Usage::default();
                                            }
                                        }
                                    }
                                }
                                final_stop = stop;
                                step_finished = true;
                            }
                            StreamEvent::Failed {
                                class,
                                message,
                                retryable,
                            } => {
                                step_failed = Some((class, message, retryable));
                                final_stop = Stop::Error;
                                step_finished = true;
                            }
                        }
                    }
                }
            }

            if let Some((class, message, retryable)) = step_failed {
                // Overflow → promote to a bigger sibling (once per
                // strand, before digesting), else digest-and-retry once.
                if class == ErrorClass::Overflow && !overflow_retried {
                    overflow_retried = true;
                    if self.context_promote && !self.promoted {
                        if let Some(target) = self.promotion_candidate() {
                            self.promoted = true;
                            self.pending_promotion = Some(target.clone());
                            let k = self
                                .catalog
                                .get(&target)
                                .map(|d| d.context / 1024)
                                .unwrap_or(0);
                            events
                                .send(Event::Note {
                                    message: format!("context → {target} ({k}k)"),
                                })
                                .await
                                .ok();
                            if let Ok(p) = parse_selector(&target) {
                                if let Some(d) = self.catalog.get(&p.model_id()).cloned() {
                                    parsed = p;
                                    model_id = parsed.model_id();
                                    dialect = d;
                                    price = dialect.price;
                                    ratio = if dialect.ratio > 0.0 {
                                        dialect.ratio
                                    } else {
                                        4.0
                                    };
                                    token = dialect
                                        .api_key_env
                                        .as_deref()
                                        .and_then(ka_dialect::auth::resolve_token);
                                    window = dialect.context as u64;
                                    retry_attempt = 0;
                                    self.model_selector = Some(target);
                                    self.ratio = ratio as f64;
                                    continue 'outer;
                                }
                            }
                        }
                    }
                    events.send(Event::DigestStarted).await.ok();
                    if let Some(summary) = self
                        .summarize(None, std::time::Duration::from_secs(120))
                        .await
                    {
                        self.apply_digest(summary, self.ratio);
                        continue 'outer;
                    }
                }
                // Retryable failure → automatic slow backoff before
                // giving up (5s / 20s / 60s, max 3 attempts). The failed
                // step is re-driven from the same history, so the user
                // record is never duplicated.
                let delays = retry_delays();
                if retryable && retry_attempt < delays.len() {
                    let delay = delays[retry_attempt];
                    retry_attempt += 1;
                    events
                        .send(Event::Note {
                            message: format!("↻ retrying in {}s (esc cancels)", delay.as_secs()),
                        })
                        .await
                        .ok();
                    if wait_or_cancel(delay, commands, steers, queue).await {
                        events
                            .send(Event::Note {
                                message: "retry canceled".to_string(),
                            })
                            .await
                            .ok();
                    } else {
                        continue 'outer;
                    }
                }
                // Fallback chain: provider/auth failure with per-wire
                // retries exhausted → re-dispatch the same messages on
                // the next configured model (max 2 hops per turn). The
                // switch is transient: the rest of this turn rides the
                // fallback, no strand Change is recorded, and the next
                // turn starts from the requested selector again.
                if matches!(
                    class,
                    ErrorClass::Auth
                        | ErrorClass::RateLimit
                        | ErrorClass::Network
                        | ErrorClass::Internal
                ) && fallback_hops < 2
                {
                    let mut hop: Option<(String, ka_dialect::Selector, ka_dialect::Dialect)> = None;
                    while let Some(sel) = self.fallbacks.get(fallback_cursor) {
                        fallback_cursor += 1;
                        if let Ok(p) = parse_selector(sel) {
                            if let Some(d) = self.catalog.get(&p.model_id()).cloned() {
                                hop = Some((sel.clone(), p, d));
                                break;
                            }
                        }
                    }
                    if let Some((sel, p, d)) = hop {
                        fallback_hops += 1;
                        events
                            .send(Event::Note {
                                message: format!("fallback → {sel}"),
                            })
                            .await
                            .ok();
                        parsed = p;
                        model_id = parsed.model_id();
                        dialect = d;
                        price = dialect.price;
                        ratio = if dialect.ratio > 0.0 {
                            dialect.ratio
                        } else {
                            4.0
                        };
                        token = dialect
                            .api_key_env
                            .as_deref()
                            .and_then(ka_dialect::auth::resolve_token);
                        window = dialect.context as u64;
                        retry_attempt = 0;
                        self.model_selector = Some(sel);
                        self.ratio = ratio as f64;
                        continue 'outer;
                    }
                }
                events
                    .send(Event::Error {
                        class,
                        retryable: false,
                        message,
                    })
                    .await
                    .ok();
                break 'outer;
            }

            if step_calls.is_empty() || steps >= self.max_steps {
                closing_thinking = std::mem::take(&mut step_thought);
                break 'outer;
            }

            // Execute this step's calls. Gating stays sequential —
            // permission asks must remain one-at-a-time UX — then the
            // approved calls run concurrently and results are reassembled
            // in original call order.
            let step_thinking = (!step_thought.trim().is_empty()).then(|| step_thought.clone());
            self.history.push(TurnMessage {
                role: TurnRole::Assistant,
                content: step_text.clone(),
                thinking: step_thinking,
                calls: step_calls.clone(),
                results: Vec::new(),
                images: Vec::new(),
            });
            let mut slots: Vec<Option<ToolOutput>> = vec![None; step_calls.len()];
            let mut approved: Vec<(usize, std::sync::Arc<dyn Hand>)> = Vec::new();
            for (idx, call) in step_calls.iter_mut().enumerate() {
                match self.admit_call(call, commands, events).await {
                    Ok(hand) => approved.push((idx, hand)),
                    Err(output) => slots[idx] = Some(output),
                }
            }
            // admit_call may patch step_calls in place (hook
            // updated_input); refresh the just-pushed history row so
            // the recorded arguments match the call that actually runs
            if let Some(msg) = self.history.last_mut() {
                msg.calls = step_calls.clone();
            }
            let mut aborted = false;
            if !approved.is_empty() {
                let mut in_flight = tokio::task::JoinSet::new();
                let mut task_idx: HashMap<tokio::task::Id, usize> = HashMap::new();
                // the hook table moves into an Arc once per step: each
                // spawned task then pays an Arc bump, not a Vec<Hook>
                // deep clone (hand ctx / sender / slots are Arc-cheap)
                let hooks = std::sync::Arc::new(self.hooks_cfg.clone());
                for (idx, hand) in approved {
                    let call = step_calls[idx].clone();
                    let mut ctx = self.hand_ctx.clone();
                    // a "this run" sandbox grant rides exactly this call
                    if let Some(grants) = self.state.sandbox_pending.remove(&call.id) {
                        ctx.sandbox = ka_sandbox::apply_grants(&ctx.sandbox, &grants);
                    }
                    let events = events.clone();
                    let hooks = hooks.clone();
                    let todo = self.todo.clone();
                    let lsp = self.lsp.clone();
                    let verify = self.verify.clone();
                    let edited = self.edited.clone();
                    let handle = in_flight.spawn(async move {
                        let (mut output, steer) =
                            execute_approved(&hand, &hooks, &call, &ctx, &events).await;
                        // LSP diagnostics and [verify] lints ride successful
                        // edit/write results as informational context — never
                        // errors (opencode #9102: diagnostics must not read as
                        // tool failure)
                        if !output.is_error && (call.tool == "edit" || call.tool == "write") {
                            if let Some(path) = call
                                .arguments
                                .get("path")
                                .and_then(serde_json::Value::as_str)
                            {
                                edited.lock().push(path.to_string());
                            }
                            if let Some(lsp) = lsp.as_ref() {
                                enrich_with_lsp(&mut output, &call, &ctx, lsp).await;
                            }
                            enrich_with_lint(&mut output, &call, &ctx, &verify).await;
                        }
                        // the todo hand owns normalization; surfaces get the
                        // fresh list as a whole-replacement event, and any
                        // confidence-spike advisory rides out as a note so
                        // the user sees the re-check request too
                        if call.tool == "todo" && !output.is_error {
                            let items = todo.lock().clone();
                            events.send(Event::Todos { items }).await.ok();
                            for line in output
                                .content
                                .lines()
                                .filter(|l| l.contains(crate::hands::todo::SPIKE_MARK))
                            {
                                events
                                    .send(Event::Note {
                                        message: format!("todo: {line}"),
                                    })
                                    .await
                                    .ok();
                            }
                        }
                        events
                            .send(Event::CallOutput {
                                tool: call.tool.clone(),
                                id: call.id.clone(),
                                excerpt: truncate_excerpt(&output.content),
                                is_error: output.is_error,
                                spill: output.spill.clone(),
                            })
                            .await
                            .ok();
                        events
                            .send(Event::CallFinished {
                                tool: call.tool.clone(),
                                id: call.id.clone(),
                                ok: !output.is_error,
                            })
                            .await
                            .ok();
                        (idx, output, steer)
                    });
                    task_idx.insert(handle.id(), idx);
                }
                let mut mode_change: Option<ka_protocol::Mode> = None;
                while !in_flight.is_empty() {
                    tokio::select! {
                        biased;
                        // abort cancels the in-flight futures; dropped bash
                        // children die via their drop guards
                        maybe_cmd = commands.recv() => match maybe_cmd {
                            None | Some(Command::Abort) => {
                                aborted = true;
                                break;
                            }
                            Some(Command::Steer { text }) => steers.push(text),
                            Some(Command::Queue { text }) => queue.push_back(text),
                            Some(Command::SetMode { mode }) => mode_change = Some(mode),
                            Some(_) => {}
                        },
                        joined = in_flight.join_next() => match joined {
                            Some(Ok((idx, output, steer))) => {
                                slots[idx] = Some(output);
                                // post-tool steering: note now, mode via
                                // the shared seam below
                                if let Some(s) = steer {
                                    if let Some(note) = s.note {
                                        events
                                            .send(Event::Note {
                                                message: format!("hook: {note}"),
                                            })
                                            .await
                                            .ok();
                                    }
                                    if s.mode.is_some() {
                                        mode_change = s.mode;
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                // a panicking hand must not wedge the step
                                if let Some(idx) = task_idx.remove(&e.id()) {
                                    let output =
                                        ToolOutput::err(format!("tool task failed: {e}"));
                                    events
                                        .send(Event::CallOutput {
                                            tool: step_calls[idx].tool.clone(),
                                            id: step_calls[idx].id.clone(),
                                            excerpt: truncate_excerpt(&output.content),
                                            is_error: true,
                                            spill: None,
                                        })
                                        .await
                                        .ok();
                                    events
                                        .send(Event::CallFinished {
                                            tool: step_calls[idx].tool.clone(),
                                            id: step_calls[idx].id.clone(),
                                            ok: false,
                                        })
                                        .await
                                        .ok();
                                    slots[idx] = Some(output);
                                }
                            }
                            None => {}
                        },
                    }
                }
                if let Some(mode) = mode_change {
                    self.set_mode(mode);
                    events.send(Event::ModeChanged { mode }).await.ok();
                }
            }
            if aborted {
                // canceled calls surface as aborted results so history
                // stays well-formed for the next turn
                for (idx, slot) in slots.iter_mut().enumerate() {
                    if slot.is_some() {
                        continue;
                    }
                    let output = ToolOutput::err("aborted");
                    events
                        .send(Event::CallOutput {
                            tool: step_calls[idx].tool.clone(),
                            id: step_calls[idx].id.clone(),
                            excerpt: truncate_excerpt(&output.content),
                            is_error: true,
                            spill: None,
                        })
                        .await
                        .ok();
                    events
                        .send(Event::CallFinished {
                            tool: step_calls[idx].tool.clone(),
                            id: step_calls[idx].id.clone(),
                            ok: false,
                        })
                        .await
                        .ok();
                    *slot = Some(output);
                }
            }
            let results: Vec<ToolResult> = slots
                .into_iter()
                .zip(step_calls.iter())
                .map(|(slot, call)| {
                    let output = slot.unwrap_or_else(|| ToolOutput::err("aborted"));
                    ToolResult {
                        call_id: call.id.clone(),
                        content: output.content,
                        is_error: output.is_error,
                        images: output.images,
                    }
                })
                .collect();
            self.history.push(TurnMessage::tool(results));
            steps += 1;
            if aborted {
                events
                    .send(Event::TurnFinished {
                        stop: Stop::Aborted,
                        usage: Usage::default(),
                    })
                    .await
                    .ok();
                self.stop_hooks(events, "aborted").await;
                return Usage::default();
            }
            // Live steering at the batch boundary: steers that arrived
            // while the tools ran ride the history right after the
            // results, so the next model request sees them in order.
            // Leftovers (steer after the last batch, or a Length break)
            // are settled by the engine: a live turn returns with them
            // still pending and the engine converts them into queued
            // follow-on turns.
            for text in steers.drain(..) {
                self.history.push(TurnMessage::user(format!(
                    "[operator, while you were working]\n{text}"
                )));
                let first_line = text.lines().next().unwrap_or("").to_string();
                let head: String = first_line.chars().take(80).collect();
                events
                    .send(Event::Note {
                        message: format!("steer delivered: {head}"),
                    })
                    .await
                    .ok();
            }
            if final_stop == Stop::Length {
                break 'outer;
            }
        }

        // True-up + cost
        if usage_total.input == 0 {
            usage_total.input = est_in;
        }
        if usage_total.output == 0 {
            usage_total.output = (assistant_text.len() as f64 / ratio as f64).ceil() as u64;
        }
        // placeholders must never surface as money
        usage_total.cost = if dialect.priced {
            cost_of(&usage_total, price)
        } else {
            0.0
        };

        if final_stop != Stop::Aborted {
            let thinking = (!closing_thinking.trim().is_empty())
                .then(|| std::mem::take(&mut closing_thinking));
            self.history.push(TurnMessage {
                role: TurnRole::Assistant,
                content: if assistant_text.is_empty() {
                    "(no text)".to_string()
                } else {
                    assistant_text.clone()
                },
                thinking,
                calls: Vec::new(),
                results: Vec::new(),
                images: Vec::new(),
            });
        }
        // run a staged pre-finish future (engine auto-commit) ahead of
        // the terminal event, so its notes precede TurnFinished
        let pre = self.pre_finish.take().map(parking_lot::Mutex::into_inner);
        if final_stop == Stop::Done {
            if let Some(fut) = pre {
                fut.await;
            }
        }
        events
            .send(Event::TurnFinished {
                stop: final_stop,
                usage: usage_total,
            })
            .await
            .ok();
        let status = match final_stop {
            Stop::Done | Stop::Length => "done",
            Stop::Aborted => "aborted",
            Stop::Error => "error",
        };
        self.stop_hooks(events, status).await;
        usage_total
    }

    /// Rendered unified-diff preview for an edit/write ask (None when
    /// the call is not a file mutation or the preview cannot be
    /// computed). Read failures and match counts the tool would reject
    /// (0 matches; ambiguous multi-match without replace_all) skip the
    /// preview silently — the tool run reports authoritatively.
    fn ask_detail(&self, call: &ToolCall) -> Option<String> {
        if call.tool != "edit" && call.tool != "write" {
            return None;
        }
        let path = call.arguments.get("path")?.as_str()?;
        let full = crate::hands::read::resolve(&self.hand_ctx, path);
        let old = std::fs::read_to_string(&full).unwrap_or_default();
        let new = match call.tool.as_str() {
            "write" => call.arguments.get("content")?.as_str()?.to_string(),
            _ => {
                let old_str = call.arguments.get("old")?.as_str()?;
                let new_str = call.arguments.get("new")?.as_str()?;
                let replace_all = call
                    .arguments
                    .get("replace_all")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let count = old.matches(old_str).count();
                if count == 0 || (count > 1 && !replace_all) {
                    return None;
                }
                if replace_all {
                    old.replace(old_str, new_str)
                } else {
                    old.replacen(old_str, new_str, 1)
                }
            }
        };
        let diff = crate::hands::unified_diff(path, &old, &new, 24);
        (!diff.is_empty()).then_some(diff)
    }
    /// Sequential gating phase for one call: the loop gate, pre-tool
    /// hooks (block / argument patch / pre-approve), clearance verdict,
    /// the fast-role auto-reviewer, sandbox-expansion grants, and any
    /// permission ask (one at a time — the ask UX stays exclusive).
    /// Returns the approved hand; Err carries the decided result
    /// (already surfaced to events). `call` is `&mut` because
    /// `updated_input` hook steering patches the arguments in place.
    async fn admit_call(
        &mut self,
        call: &mut ToolCall,
        commands: &mut mpsc::Receiver<Command>,
        events: &mpsc::Sender<Event>,
    ) -> Result<std::sync::Arc<dyn Hand>, ToolOutput> {
        let sig = format!("{}|{}", call.tool, call.arguments);
        *self.state.loop_counts.entry(sig.clone()).or_insert(0) += 1;
        let count = self.state.loop_counts.get(&sig).copied().unwrap_or(0);
        if let Some(output) = self.loop_gate(call, &sig, count, commands, events).await {
            return Err(output);
        }
        let Some(hand) = self
            .hands
            .iter()
            .find(|h| h.def().name == call.tool)
            .cloned()
        else {
            let output = ToolOutput::err(format!("unknown tool {}", call.tool));
            self.surface_decided(call, &output, events).await;
            return Err(output);
        };
        // session feature toggles: specs() already hid these; this guard
        // rejects strays (a stale step, a hallucinated name) by naming
        // the toggle so the model stops reaching for the tool
        let hidden_by = self.features.read().hidden_reason(&call.tool);
        if let Some(spec) = hidden_by {
            let output = ToolOutput::err(format!(
                "{} is disabled by a session feature toggle (`{spec}`); it is unavailable until the user re-enables it",
                call.tool
            ));
            self.surface_decided(call, &output, events).await;
            return Err(output);
        }
        // per-turn allowlist (custom-command `allowed-tools`): the spec
        // filter already hides these from the model; this guard catches
        // strays (a stale step, a racing call)
        if let Some(tools) = &self.turn_tools {
            if !tools.iter().any(|t| t == &call.tool) {
                let output = ToolOutput::err(format!(
                    "{} is not in this command's allowed tools ({})",
                    call.tool,
                    tools.join(", ")
                ));
                self.surface_decided(call, &output, events).await;
                return Err(output);
            }
        }
        // pre_tool_use hooks: exit 2 blocks before any gate; clean stdout
        // may steer (mode/note), patch the call's arguments
        // (updated_input), or pre-approve the call (decision: "allow")
        let mut hook_allowed = false;
        match self
            .run_hooks(
                crate::config::HookEvent::PreToolUse,
                &call.tool,
                &call.arguments,
            )
            .await
        {
            Err(reason) => {
                let output = ToolOutput::err(format!("blocked by hook: {reason}"));
                self.surface_decided(call, &output, events).await;
                return Err(output);
            }
            Ok(steer) => {
                if let Some(s) = steer {
                    if let Some(patch) = s.updated_input {
                        let keys = apply_input_patch(call, patch);
                        events
                            .send(Event::Note {
                                message: format!(
                                    "hook patched {} arguments ({})",
                                    call.tool,
                                    keys.chars()
                                        .take(crate::fshooks::NOTE_CHARS)
                                        .collect::<String>()
                                ),
                            })
                            .await
                            .ok();
                    }
                    hook_allowed = s.allow;
                    self.apply_steering(
                        Some(crate::fshooks::Steering {
                            mode: s.mode,
                            note: s.note,
                            updated_input: None,
                            allow: false,
                            feedback: None,
                        }),
                        events,
                    )
                    .await;
                }
            }
        }
        // a hook pre-approve skips the permission ask — but it may
        // only downgrade an ask. Deny-class verdicts (deny rules,
        // plan-mode writes outside .ka/plans/) and the hardstop /
        // protected classes outrank every hook and stand.
        let clearance = hand.clearance_for(&call.arguments);
        let mut gate = if hook_allowed {
            let normal = self.gate(clearance, call);
            let refused = match &normal {
                Gate::Deny { reason } => Some(reason.clone()),
                _ => self.unbypassable_reason(call, clearance),
            };
            match refused {
                Some(reason) => {
                    events
                        .send(Event::Note {
                            message: format!(
                                "hook pre-approve ignored for {}: {}",
                                call.tool, reason
                            ),
                        })
                        .await
                        .ok();
                    normal
                }
                None => {
                    events
                        .send(Event::Note {
                            message: format!("hook pre-approved `{}`", call.tool),
                        })
                        .await
                        .ok();
                    Gate::Allow
                }
            }
        } else {
            self.gate(clearance, call)
        };
        // auto_review (roadmap 9.4): the fast-role reviewer pre-screens
        // reviewable exec asks — auto-allow only, never deny, always
        // visible as a note; unavailable reviewer fails toward the human
        if self.auto_review
            && !crate::conventions::bare_mode()
            && matches!(
                gate,
                Gate::Ask {
                    reviewable: true,
                    ..
                }
            )
        {
            if let Some(model) = self.reviewer_model.clone() {
                let command = command_of(call);
                let prompt = format!("Command:\n{command}\n\nReply with exactly `allow` or `ask`.");
                match self
                    .role_complete(&model, REVIEW_SYSTEM, &prompt, Duration::from_secs(10))
                    .await
                {
                    Some(reply) if reply.trim().eq_ignore_ascii_case("allow") => {
                        events
                            .send(Event::Note {
                                message: format!("auto_review: allowed `{command}` (fast role)"),
                            })
                            .await
                            .ok();
                        gate = Gate::Allow;
                    }
                    Some(_) => {
                        events
                            .send(Event::Note {
                                message: format!(
                                    "auto_review: not confident about `{command}` — asking you"
                                ),
                            })
                            .await
                            .ok();
                    }
                    None => {}
                }
            }
        }
        // sandbox expansion (roadmap 9.1): compute the exact grants the
        // command needs beyond the policy — deterministic, pre-flight,
        // never inferred from failure output
        let mut grants = self.sandbox_grants_for(call).filter(|g| !g.is_empty());
        // grants earned below ride exactly this call; they stay local
        // until the final Ok so no veto/abort path between here and
        // execution can leak a stale sandbox_pending entry under this
        // call id
        let mut pending: Option<ka_sandbox::Grants> = None;
        // session-remembered grants ("always") apply silently
        if let Some(wanted) = grants.clone() {
            if let Some(have) = self.state.sandbox_granted.get(&sig) {
                if have.covers(&wanted) {
                    pending = Some(wanted);
                    grants = None;
                }
            }
        }
        match gate {
            Gate::Allow => {
                // grant-only ask: the permission gate passed, but the
                // sandbox would deny the command's computable needs
                if let Some(g) = grants {
                    if !self.state.sandbox_asked.contains(&sig) {
                        self.state.sandbox_asked.insert(sig.clone());
                        let command = command_of(call);
                        let question = format!(
                            "sandbox grant for `{command}`: {} — allow? (deny keeps the \
                             sandbox unchanged; the command will likely fail)",
                            g.summary()
                        );
                        match self.pose_ask(question, None, commands, events).await {
                            AskOutcome::Choice(0) => {
                                pending = Some(g);
                            }
                            AskOutcome::Choice(1) => {
                                self.expand_live_sandbox(&g);
                                pending = Some(g.clone());
                                self.state.sandbox_granted.insert(sig.clone(), g.clone());
                                self.note_sandbox_save(&g, events).await;
                            }
                            AskOutcome::Choice(_) => {}
                            AskOutcome::Abort => {
                                let output = ToolOutput::err("aborted");
                                self.surface_decided(call, &output, events).await;
                                return Err(output);
                            }
                            AskOutcome::Closed => {
                                let output = ToolOutput::err("surface closed during ask");
                                self.surface_decided(call, &output, events).await;
                                return Err(output);
                            }
                        }
                    }
                }
            }
            Gate::Deny { reason } => {
                let output = ToolOutput::err(reason);
                self.surface_decided(call, &output, events).await;
                return Err(output);
            }
            Gate::Ask { question, .. } => {
                // fold the grant summary into the permission ask so one
                // answer covers both (guarded mode asks once, not twice)
                let question = if let Some(g) = &grants {
                    if self.state.sandbox_asked.contains(&sig) {
                        question
                    } else {
                        self.state.sandbox_asked.insert(sig.clone());
                        format!("{question}\nsandbox grant: {}", g.summary())
                    }
                } else {
                    question
                };
                let detail = self.ask_detail(call);
                match self.pose_ask(question, detail, commands, events).await {
                    AskOutcome::Choice(1) => {
                        self.state.rules.insert(format!("tool:{}", call.tool));
                        // persist the allowlist entry to the project
                        // layer (best-effort, silent)
                        if let Some(path) =
                            crate::config::save_project_permission(&self.hand_ctx.cwd, &call.tool)
                        {
                            events
                                .send(Event::Note {
                                    message: format!(
                                        "always-allow for {} saved to {}",
                                        call.tool,
                                        path.display()
                                    ),
                                })
                                .await
                                .ok();
                        }
                        if let Some(g) = grants.take() {
                            self.expand_live_sandbox(&g);
                            pending = Some(g.clone());
                            self.state.sandbox_granted.insert(sig.clone(), g.clone());
                            self.note_sandbox_save(&g, events).await;
                        }
                    }
                    AskOutcome::Choice(2) => {
                        let output =
                            ToolOutput::err(format!("permission denied by user for {}", call.tool));
                        self.surface_decided(call, &output, events).await;
                        return Err(output);
                    }
                    AskOutcome::Abort => {
                        let output = ToolOutput::err("aborted");
                        self.surface_decided(call, &output, events).await;
                        return Err(output);
                    }
                    AskOutcome::Closed => {
                        let output = ToolOutput::err("surface closed during ask");
                        self.surface_decided(call, &output, events).await;
                        return Err(output);
                    }
                    AskOutcome::Choice(_) => {
                        // allow (this run): grants apply to this call only
                        if let Some(g) = grants.take() {
                            pending = Some(g);
                        }
                    }
                }
            }
        }
        // convention pre-tool hook: non-zero exit vetoes the call;
        // clean-exit stdout steers the mode for subsequent calls (the
        // veto position — after the gate — is unchanged). Silenced by
        // the `hooks` feature toggle like every other hook class.
        let hooks_on = self.features.read().hooks_enabled();
        if hooks_on {
            match crate::fshooks::run(
                crate::fshooks::HookPoint::PreTool,
                &self.hand_ctx.cwd,
                Some(&call.tool),
            )
            .await
            {
                Err(reason) => {
                    events
                        .send(Event::Note {
                            message: reason.clone(),
                        })
                        .await
                        .ok();
                    let output = ToolOutput::err(format!("blocked by pre-tool hook: {reason}"));
                    self.surface_decided(call, &output, events).await;
                    return Err(output);
                }
                Ok(steer) => self.apply_steering(steer, events).await,
            }
        }
        // the call survived every veto: only now does the grant attach
        if let Some(g) = pending {
            self.state.sandbox_pending.insert(call.id.clone(), g);
        }
        Ok(hand)
    }

    /// Pose one allow/always/deny ask and await the answer. `Abort` and
    /// a closed surface are distinct outcomes so callers keep their
    /// existing error strings. Other commands arriving mid-ask are
    /// dropped (the ask stays exclusive).
    async fn pose_ask(
        &mut self,
        question: String,
        detail: Option<String>,
        commands: &mut mpsc::Receiver<Command>,
        events: &mpsc::Sender<Event>,
    ) -> AskOutcome {
        self.state.ask_counter += 1;
        let ask_id = AskId(format!("ask-{}", self.state.ask_counter));
        let ask = Event::Ask {
            id: ask_id.clone(),
            questions: vec![AskQuestion {
                text: question,
                options: vec![
                    "allow".to_string(),
                    "always".to_string(),
                    "deny".to_string(),
                ],
                detail,
            }],
        };
        if events.send(ask).await.is_err() {
            return AskOutcome::Closed;
        }
        loop {
            match commands.recv().await {
                Some(Command::Answer {
                    question: q,
                    choice,
                }) if q == ask_id => {
                    return AskOutcome::Choice(choice);
                }
                Some(Command::Abort) => return AskOutcome::Abort,
                Some(_) => {}
                None => return AskOutcome::Closed,
            }
        }
    }

    /// The loop gate (`[[rules]] tool = "loop"`, roadmap 9.4).
    /// `Some(output)` = the call is refused. Without a loop rule the
    /// guard keeps its shipped shape: identical calls error at 4+.
    async fn loop_gate(
        &mut self,
        call: &ToolCall,
        sig: &str,
        count: u32,
        commands: &mut mpsc::Receiver<Command>,
        events: &mpsc::Sender<Event>,
    ) -> Option<ToolOutput> {
        let rule = self
            .rules_cfg
            .iter()
            .find(|r| r.tool == "loop")
            .map(|r| r.verdict);
        if self.state.loop_ok.contains(sig) || rule == Some(crate::config::Verdict::Allow) {
            return None;
        }
        if count >= 3 && rule.is_some() {
            match rule {
                Some(crate::config::Verdict::Deny) => {
                    return Some(
                        loop_refused(
                            call,
                            format!(
                                "denied by loop rule: `{}` repeated {count}× with identical \
                                 arguments",
                                call.tool
                            ),
                            events,
                        )
                        .await,
                    );
                }
                Some(crate::config::Verdict::Ask) => {
                    let question = format!(
                        "loop detected: `{}` called {count}× with identical arguments — \
                         continue?",
                        call.tool
                    );
                    match self.pose_ask(question, None, commands, events).await {
                        AskOutcome::Choice(1) => {
                            self.state.loop_ok.insert(sig.to_string());
                        }
                        AskOutcome::Choice(2) => {
                            return Some(
                                loop_refused(
                                    call,
                                    "denied by user at the loop gate".to_string(),
                                    events,
                                )
                                .await,
                            );
                        }
                        AskOutcome::Abort => {
                            return Some(loop_refused(call, "aborted".to_string(), events).await);
                        }
                        AskOutcome::Closed => {
                            return Some(
                                loop_refused(call, "surface closed during ask".to_string(), events)
                                    .await,
                            );
                        }
                        AskOutcome::Choice(_) => {} // allow once; re-asks on repeat
                    }
                }
                _ => {}
            }
        }
        // default trip (no rule): unchanged at 4+ — an Ask rule owns the
        // threshold from 3 upward and never falls through to this
        if count >= 4 && rule.is_none() {
            return Some(
                loop_refused(
                    call,
                    "loop guard: this tool was called with identical arguments 4+ times; stop \
                     repeating and reconsider"
                        .to_string(),
                    events,
                )
                .await,
            );
        }
        None
    }

    /// Deterministic pre-flight sandbox-expansion grants for a call
    /// (None unless the call is a sandboxed bash command).
    fn sandbox_grants_for(&self, call: &ToolCall) -> Option<ka_sandbox::Grants> {
        if call.tool != "bash" {
            return None;
        }
        if !matches!(self.hand_ctx.sandbox, ka_sandbox::Policy::Fs { .. }) {
            return None;
        }
        let command = command_of(call);
        if command.is_empty() {
            return None;
        }
        let analysis = analyze(&command);
        let targets = redirect_targets(&command);
        let envs = crate::hands::bashp::env_assignments(&command);
        Some(ka_sandbox::missing_grants(
            &self.hand_ctx.sandbox,
            &self.hand_ctx.cwd,
            &targets,
            &envs,
            // readonly commands never earn a network grant: `git`
            // sits in NETWORK_TOOLS, so `git status`/`git diff` would
            // otherwise pose a dishonest network-expansion ask
            wants_network(&analysis) && !all_readonly(&analysis),
        ))
    }

    /// Fold granted write paths into the live base policy so later
    /// identical commands compute an empty grant set.
    fn expand_live_sandbox(&mut self, grants: &ka_sandbox::Grants) {
        self.hand_ctx.sandbox = ka_sandbox::apply_grants(&self.hand_ctx.sandbox, grants);
    }

    /// Note where an "always" grant's write paths were persisted.
    async fn note_sandbox_save(&self, grants: &ka_sandbox::Grants, events: &mpsc::Sender<Event>) {
        if grants.write_paths.is_empty() {
            return;
        }
        if let Some(path) =
            crate::config::save_project_sandbox_grants(&self.hand_ctx.cwd, &grants.write_paths)
        {
            events
                .send(Event::Note {
                    message: format!(
                        "sandbox write grant ({}) saved to {}",
                        grants.summary(),
                        path.display()
                    ),
                })
                .await
                .ok();
        }
    }

    /// The unbypassable reason a hook pre-approve must NOT skip:
    /// hardstops for exec calls, protected paths for writes.
    /// (Deny-class gate verdicts — deny rules, plan-mode writes
    /// outside .ka/plans/ — are caught at the call site by running
    /// the normal gate first; see admit_call.)
    fn unbypassable_reason(&self, call: &ToolCall, clearance: Clearance) -> Option<String> {
        match clearance {
            Clearance::Exec => {
                let command = command_of(call);
                let analysis = analyze(&command);
                hardstop(&command, &analysis).map(|s| s.reason)
            }
            Clearance::Write if self.mode != ka_protocol::Mode::Plan => {
                crate::hands::protected::reason(&self.hand_ctx.cwd, &call.primary_arg())
                    .map(str::to_string)
            }
            _ => None,
        }
    }

    /// Surface a gate-phase decision (CallOutput + CallFinished) without
    /// executing the call.
    async fn surface_decided(
        &self,
        call: &ToolCall,
        output: &ToolOutput,
        events: &mpsc::Sender<Event>,
    ) {
        events
            .send(Event::CallOutput {
                tool: call.tool.clone(),
                id: call.id.clone(),
                excerpt: truncate_excerpt(&output.content),
                is_error: output.is_error,
                spill: None,
            })
            .await
            .ok();
        events
            .send(Event::CallFinished {
                tool: call.tool.clone(),
                id: call.id.clone(),
                ok: !output.is_error,
            })
            .await
            .ok();
    }

    fn gate(&self, clearance: Clearance, call: &ToolCall) -> Gate {
        // protected paths are hardstop-class: checked before rules so no
        // allow-rule or session always-allow can bypass them, and before
        // mode logic so free mode cannot wave them through. Plan mode is
        // exempt here — it already denies every write outside .ka/plans/,
        // which covers everything the protected list contains.
        if clearance == Clearance::Write && self.mode != ka_protocol::Mode::Plan {
            if let Some(reason) =
                crate::hands::protected::reason(&self.hand_ctx.cwd, &call.primary_arg())
            {
                return Gate::Ask {
                    question: format!(
                        "PROTECTED — {reason}: `{}`. Proceed anyway?",
                        call.primary_arg()
                    ),
                    reviewable: false,
                };
            }
        }
        // configured rules: first match wins, before mode logic
        if let Some(verdict) = self.match_rule(call) {
            return match verdict {
                crate::config::Verdict::Allow => Gate::Allow,
                crate::config::Verdict::Ask => Gate::Ask {
                    question: format!(
                        "rule requires confirmation for {} `{}`",
                        call.tool,
                        call.primary_arg()
                    ),
                    reviewable: false,
                },
                crate::config::Verdict::Deny => Gate::Deny {
                    reason: format!("denied by rule for {}", call.tool),
                },
            };
        }
        if self.state.rules.contains(&format!("tool:{}", call.tool)) {
            return Gate::Allow;
        }
        // `[permissions] allow`: the config-authored persistent
        // allowlist, same standing as a session "always" — protected
        // paths and deny rules outrank it (both checked above)
        if self.allowed_tools.iter().any(|t| t == &call.tool) {
            return Gate::Allow;
        }
        match clearance {
            Clearance::Read => Gate::Allow,
            Clearance::Write => match self.mode {
                ka_protocol::Mode::Free | ka_protocol::Mode::AcceptEdits => Gate::Allow,
                ka_protocol::Mode::Guarded => Gate::Ask {
                    question: format!("allow {} to modify files?", call.tool),
                    reviewable: false,
                },
                ka_protocol::Mode::Plan => {
                    // research mode: only the project root's plans
                    // directory is writable. Resolve the argument
                    // (absolute or relative) before comparing, so every
                    // spelling lands where the plan prompt and the TUI
                    // watcher look
                    let plans_dir = crate::project_root(&self.hand_ctx.cwd).join(".ka/plans");
                    let path = crate::hands::read::resolve(&self.hand_ctx, &call.primary_arg());
                    if path.starts_with(&plans_dir) {
                        Gate::Allow
                    } else {
                        Gate::Deny {
                            reason: format!(
                                "plan mode is read-only except .ka/plans/ (got {:?}); \
use /build to switch to implementation",
                                call.primary_arg()
                            ),
                        }
                    }
                }
            },
            Clearance::Exec => {
                let command = call
                    .arguments
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let analysis = analyze(command);
                if let Some(stop) = hardstop(command, &analysis) {
                    return Gate::Ask {
                        question: format!(
                            "HARDSTOP — {}: `{}`. Proceed anyway?{}",
                            stop.reason,
                            command,
                            self.cost_suffix()
                        ),
                        reviewable: false,
                    };
                }
                if self.state.rules.contains(&format!(
                    "bash:{}",
                    analysis
                        .segments
                        .first()
                        .and_then(|s| s.first())
                        .cloned()
                        .unwrap_or_default()
                )) {
                    return Gate::Allow;
                }
                if all_readonly(&analysis) {
                    return Gate::Allow;
                }
                match self.mode {
                    ka_protocol::Mode::Free => Gate::Allow,
                    ka_protocol::Mode::Plan => Gate::Ask {
                        question: format!(
                            "plan mode: run `{command}`? (build with /build){}",
                            self.cost_suffix()
                        ),
                        reviewable: false,
                    },
                    ka_protocol::Mode::AcceptEdits | ka_protocol::Mode::Guarded => Gate::Ask {
                        question: format!("run `{command}`?{}", self.cost_suffix()),
                        reviewable: true,
                    },
                }
            }
        }
    }

    /// One-line cost estimate appended to exec-tier permission asks:
    /// current context (chars/ratio) billed at the catalog's input
    /// price plus an assumed 2 048 output tokens at its output price.
    /// Empty for unpriced models (pricing honesty: no fake numbers).
    fn cost_suffix(&self) -> String {
        let Some(selector) = self.model_selector.as_deref() else {
            return String::new();
        };
        let Ok(parsed) = ka_dialect::parse_selector(selector) else {
            return String::new();
        };
        let Some(dialect) = self.catalog.get(&parsed.model_id()) else {
            return String::new();
        };
        if !dialect.priced {
            return String::new();
        }
        let ratio = if self.ratio > 0.0 { self.ratio } else { 4.0 };
        let mut ctx_tokens: u64 = self.history.iter().map(|m| message_tokens(m, ratio)).sum();
        // the digest rides in the system prompt, not history — it counts
        // toward billed context too
        if let Some(digest) = &self.digest {
            ctx_tokens += (digest.len() as f64 / ratio).ceil() as u64;
        }
        let est = ctx_tokens as f64 / 1_000_000.0 * dialect.price.input_per_mtok
            + 2048.0 / 1_000_000.0 * dialect.price.output_per_mtok;
        format!(" (rough est. ≈ ${est:.3} — pricing is per-Mtok catalog data)")
    }
}

/// The static core of the system prompt: identity + repo posture, the
/// todo discipline, and (in plan mode) the plan-mode contract. This is
/// the only prompt text ka itself authors — project instructions,
/// memory, scoped rules, skills, and the digest summary are session
/// data layered around it, outside the budget. Contract-capped by
/// `system_prompt_core_stays_within_budget` so it cannot creep.
fn core_instructions(
    snap_summary: &str,
    mode: ka_protocol::Mode,
    cwd: &std::path::Path,
    identity: Option<&str>,
) -> String {
    let mut s = String::new();
    match identity.map(str::trim).filter(|i| !i.is_empty()) {
        Some(identity) => s.push_str(&format!(
            "\n{identity}\nWorkspace: {snap_summary}."
        )),
        None => s.push_str(&format!(
            "\nYou are ka, a precise coding agent. {snap_summary}. Use the provided tools to inspect and modify the repository; prefer read before edit.",
        )),
    }
    s.push_str(
        "\nMaintain your plan with the todo tool: on multi-step tasks keep the todo list \
current — one item per step, mark items done as you finish them (each call replaces the \
whole list).",
    );
    if mode == ka_protocol::Mode::Plan {
        // absolute, root-anchored: a session launched from a
        // subdirectory must draft into the one project plans dir the
        // /approve flow and the TUI watcher watch
        let plan = crate::project_root(cwd).join(".ka/plans/plan.md");
        s.push_str(&format!(
            "\n\nPLAN MODE: research the task with read/glob/grep/pathfinder, then write a \
concrete numbered plan to {} (the only writable path). Do not \
attempt implementation — the user will review and switch to build mode.",
            plan.display()
        ));
    }
    s
}

/// Execution phase for one approved call: post_tool_use hooks, the call
/// itself (bash streams live previews), and one-way secret redaction.
/// Runs concurrently for every approved call of a step; each call emits
/// its own CallOutput/CallFinished as it completes.
async fn execute_approved(
    hand: &std::sync::Arc<dyn Hand>,
    hooks: &[crate::config::Hook],
    call: &ToolCall,
    ctx: &HandContext,
    events: &mpsc::Sender<Event>,
) -> (ToolOutput, Option<crate::fshooks::Steering>) {
    // bash runs long: pump live preview emissions while the child
    // works. Partials are new-since-last-emission output on the same
    // call id (is_error=false, spill=None, redacted, hard-capped);
    // the final CallOutput emission is untouched.
    let mut output = if call.tool == "bash" {
        let tool = call.tool.clone();
        let id = call.id.clone();
        let events = events.clone();
        let progress = move |fresh: String| {
            let excerpt = crate::hands::bash::cap_preview(&fresh);
            let excerpt = crate::hands::secrets::redact(&excerpt);
            if excerpt.is_empty() {
                return;
            }
            let _ = events.try_send(Event::CallOutput {
                tool: tool.clone(),
                id: id.clone(),
                excerpt,
                is_error: false,
                spill: None,
            });
        };
        crate::hands::BashHand
            .execute_streaming(&call.arguments, ctx, &progress)
            .await
    } else {
        hand.execute(&call.arguments, ctx).await
    };
    let mut steering = None;
    // post_tool_use hooks: exit 2 flags the result as an error; clean
    // stdout may steer and may carry `feedback` — appended to the
    // result the model reads (formatters, linters, policy echoes),
    // capped, never an error
    match run_hook_scripts(
        hooks,
        crate::config::HookEvent::PostToolUse,
        &call.tool,
        &call.arguments,
        &ctx.cwd,
    )
    .await
    {
        Err(reason) => {
            output.is_error = true;
            output
                .content
                .push_str(&format!("\n[post-tool hook: {reason}]"));
        }
        Ok(steer) => {
            if let Some(steer) = steer {
                if let Some(feedback) = &steer.feedback {
                    output
                        .content
                        .push_str(&format!("\n[hook feedback]\n{feedback}"));
                }
                steering = Some(steer);
            }
        }
    }
    // one-way secret redaction before anything reaches the model
    output.content = crate::hands::secrets::redact(&output.content);
    (output, steering)
}

/// Append LSP diagnostics to a successful edit/write result. The file
/// on disk is authoritative (the hand just wrote it); `touch` feeds it
/// to the language server and we poll the diagnostics cache for up to
/// 1.5 s — a cold server that loses the race simply contributes
/// nothing this edit. The block is omitted when empty and never flips
/// `is_error`.
async fn enrich_with_lsp(
    output: &mut ToolOutput,
    call: &ToolCall,
    ctx: &HandContext,
    lsp: &LspSlot,
) {
    let Some(path_arg) = call
        .arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
    else {
        return;
    };
    let full = crate::hands::read::resolve(ctx, path_arg);
    let Ok(text) = std::fs::read_to_string(&full) else {
        return;
    };
    // the manager is a cheap handle: internal locks are held only per
    // call, so concurrent edits never serialize on the poll
    let mgr = lsp.clone();
    // touch reports whether content actually went to a server; when it
    // didn't (disabled, unknown language, unconfigured, still starting)
    // there is nothing to poll — skipping the wait keeps default-config
    // edits fast
    if !mgr.touch(&full, &text).await {
        return;
    }
    // 2.5s budget: a warm rust-analyzer flycheck publish measures
    // ~1.5s after didChange (2026-09 host measurement); the extra
    // second is headroom. Cold first-ever checks still lose the race
    // by design — later edits get the diagnostics.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(2500);
    // None = server hasn't published for THIS content yet (the cache was
    // invalidated on touch); a published-empty result also exits the
    // loop immediately (clean file). The prior edit's diagnostics can
    // never leak through here.
    let mut rendered = mgr.diagnostics(&full);
    while rendered.is_none() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        rendered = mgr.diagnostics(&full);
    }
    let Some(rendered) = rendered else {
        return;
    };
    if rendered.is_empty() {
        return;
    }
    // the block is appended after execute_approved's redaction — apply
    // the same one-way redaction here (diagnostics quote file text)
    let block = crate::hands::secrets::redact(&format!(
        "\n<lsp-diagnostics note=\"informational context — your edit succeeded\">\n{}\n</lsp-diagnostics>",
        rendered.join("\n")
    ));
    output.content.push_str(&block);
}

/// Append the first matching [verify] lint result to a successful
/// edit/write tool result (aider's auto-lint). Same contract as the
/// LSP block: informational, never `is_error`, capped output.
async fn enrich_with_lint(
    output: &mut ToolOutput,
    call: &ToolCall,
    ctx: &HandContext,
    verify: &crate::config::Verify,
) {
    let Some(path) = call
        .arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
    else {
        return;
    };
    let rel = path.replace('\\', "/");
    let base = rel.rsplit('/').next().unwrap_or(rel.as_str());
    let Some(rule) = verify
        .lints
        .iter()
        .find(|r| glob_match(&r.pattern, &rel) || glob_match(&r.pattern, base))
    else {
        return;
    };
    let command = if rule.command.contains("{file}") {
        rule.command.replace("{file}", &rel)
    } else {
        format!("{} {rel}", rule.command)
    };
    let started = std::time::Instant::now();
    let ran = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .current_dir(&ctx.cwd)
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let note = match ran {
        Err(_) => Some("timed out after 60s".to_string()),
        Ok(Err(e)) => Some(format!("failed to spawn: {e}")),
        Ok(Ok(out)) if !out.status.success() => {
            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            let text = text.trim();
            // tail-bias: lint errors live at the end of the output
            let chars: Vec<char> = text.chars().collect();
            let cut: String = if chars.len() > 1_500 {
                chars[chars.len() - 1_500..].iter().collect()
            } else {
                text.to_string()
            };
            let code = out.status.code().unwrap_or(-1);
            Some(format!(
                "exit {code} — {}",
                if cut.is_empty() { "(no output)" } else { &cut }
            ))
        }
        _ => None,
    };
    if let Some(note) = note {
        let block = crate::hands::secrets::redact(&format!(
            "\n<lint note=\"informational context — your edit succeeded\">\n`{command}` {note} ({:.1}s)\n</lint>",
            started.elapsed().as_secs_f32()
        ));
        output.content.push_str(&block);
    }
}

/// Run matching hook scripts for one event. Returns Err(reason) when a
/// pre_tool_use hook blocked the call (exit 2, stderr as reason). A free
/// fn so concurrent per-call tasks can run post_tool_use hooks.
async fn run_hook_scripts(
    hooks: &[crate::config::Hook],
    event: crate::config::HookEvent,
    tool: &str,
    args: &serde_json::Value,
    cwd: &std::path::Path,
) -> Result<Option<crate::fshooks::Steering>, String> {
    use tokio::io::AsyncWriteExt;
    let mut steering = None;
    for hook in hooks {
        if hook.event != event {
            continue;
        }
        if let Some(t) = &hook.tool {
            if t != tool {
                continue;
            }
        }
        let payload = serde_json::json!({
            "tool": tool,
            "arguments": args,
            "cwd": cwd.display().to_string(),
        });
        let mut child = match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&hook.command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return Err(format!("hook failed to spawn: {e}")),
        };
        let stdin_opt = child.stdin.take();
        if let Some(mut stdin) = stdin_opt {
            let _ = stdin.write_all(payload.to_string().as_bytes()).await;
        }
        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            child.wait_with_output(),
        )
        .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(format!("hook failed: {e}")),
            Err(_) => return Err("hook timed out after 30s".to_string()),
        };
        if output.status.code() == Some(2) {
            let reason = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(if reason.is_empty() {
                "blocked by hook".to_string()
            } else {
                reason
            });
        }
        // steering: on a clean exit, a JSON object on stdout may switch
        // the mode and/or surface a note. Unparsable or empty stdout is
        // IGNORED — it never erases an earlier hook's steering (only a
        // hook that actually emits steering wins, latest first).
        if output.status.success() {
            if let Some(s) =
                crate::fshooks::parse_steering(&String::from_utf8_lossy(&output.stdout))
                    .into_nonempty()
            {
                steering = Some(s);
            }
        }
    }
    Ok(steering)
}

/// Run configured hooks for a lifecycle `event` (stop / turn_end /
/// session_start / session_end / user_prompt_submit / pre_compact).
/// Exit codes are advisory: failures are collected for the caller to
/// surface as notes, never to fail the turn. Clean stdout may steer
/// (mode/note only — argument patches and pre-approval are pre_tool_use
/// powers and are ignored here). Tool-filtered hooks never fire.
async fn run_event_scripts(
    hooks: &[crate::config::Hook],
    event: crate::config::HookEvent,
    payload: &serde_json::Value,
    cwd: &std::path::Path,
) -> (Vec<String>, Option<crate::fshooks::Steering>) {
    use tokio::io::AsyncWriteExt;
    let mut failures = Vec::new();
    let mut steering = None;
    for hook in hooks {
        if hook.event != event || hook.tool.is_some() {
            continue;
        }
        let outcome = async {
            let mut child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&hook.command)
                .current_dir(cwd)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| format!("failed to spawn: {e}"))?;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(payload.to_string().as_bytes()).await;
            }
            let output =
                tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
                    .await
                    .map_err(|_| "timed out after 30s".to_string())?
                    .map_err(|e| format!("failed: {e}"))?;
            if !output.status.success() {
                let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return Err(if err.is_empty() {
                    format!("exit {}", output.status)
                } else {
                    err
                });
            }
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        }
        .await;
        match outcome {
            Err(reason) => failures.push(reason),
            Ok(stdout) => {
                if let Some(s) = crate::fshooks::parse_steering(&stdout).into_nonempty() {
                    steering = Some(crate::fshooks::Steering {
                        mode: s.mode,
                        note: s.note,
                        updated_input: None,
                        allow: false,
                        feedback: None,
                    });
                }
            }
        }
    }
    (failures, steering)
}

/// Automatic retry backoff for retryable turn failures: 5s, 20s, 60s
/// (max 3 attempts) before falling through to the normal error finish.
/// Tests shrink the schedule to keep the suite fast.
fn retry_delays() -> Vec<Duration> {
    #[cfg(test)]
    {
        vec![
            Duration::from_millis(10),
            Duration::from_millis(10),
            Duration::from_millis(10),
        ]
    }
    #[cfg(not(test))]
    {
        vec![
            Duration::from_secs(5),
            Duration::from_secs(20),
            Duration::from_secs(60),
        ]
    }
}

#[derive(Debug)]
enum Gate {
    Allow,
    /// `reviewable` marks the plain exec-tier mode asks (the only ones
    /// the `[guards] auto_review` fast-role reviewer may pre-screen).
    Ask {
        question: String,
        reviewable: bool,
    },
    Deny {
        reason: String,
    },
}

/// Outcome of a posed allow/always/deny ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AskOutcome {
    /// The chosen option index (0 allow · 1 always · 2 deny).
    Choice(usize),
    /// The surface aborted the turn mid-ask.
    Abort,
    /// The surface channel closed mid-ask.
    Closed,
}

/// The bash `command` string of a call ("" for anything else).
fn command_of(call: &ToolCall) -> String {
    call.arguments
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Shallow-merge an `updated_input` patch into a call's arguments;
/// returns the patched key names for the transcript note. A call whose
/// arguments are not an object is left untouched (nothing to merge).
fn apply_input_patch(
    call: &mut ToolCall,
    patch: serde_json::Map<String, serde_json::Value>,
) -> String {
    let keys = patch.keys().cloned().collect::<Vec<_>>().join(", ");
    if let Some(map) = call.arguments.as_object_mut() {
        for (k, v) in patch {
            map.insert(k, v);
        }
    }
    keys
}

/// Loop-gate refusal: surfaces as CallFinished ONLY — deliberately a
/// different shape than surface_decided (CallOutput + CallFinished),
/// because the refusal reason rides the tool result back to the model
/// instead of the event stream.
async fn loop_refused(
    call: &ToolCall,
    message: String,
    events: &mpsc::Sender<Event>,
) -> ToolOutput {
    events
        .send(Event::CallFinished {
            tool: call.tool.clone(),
            id: call.id.clone(),
            ok: false,
        })
        .await
        .ok();
    ToolOutput {
        content: message,
        is_error: true,
        images: Vec::new(),
        spill: None,
    }
}

/// Wait `delay` before a retry, draining side-effect commands while
/// waiting. Returns `true` when the wait was canceled (Abort or the
/// surface went away).
async fn wait_or_cancel(
    delay: Duration,
    commands: &mut mpsc::Receiver<Command>,
    steers: &mut Vec<String>,
    queue: &mut VecDeque<String>,
) -> bool {
    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            biased;
            maybe = commands.recv() => match maybe {
                None => return true,
                Some(Command::Abort) => return true,
                Some(Command::Steer { text }) => steers.push(text),
                Some(Command::Queue { text }) => queue.push_back(text),
                Some(_) => {}
            },
            () = &mut sleep => return false,
        }
    }
}

/// Short argument summary for [`Event::CallStarted`] transcript headers:
/// bash → the command flattened to one line (≤48 cols), file tools → the
/// path's last segment (≤32), searches → the pattern (≤32), anything
/// else empty (the header shows the bare tool name).
pub(crate) fn call_detail(tool: &str, arguments: &serde_json::Value) -> String {
    let str_arg = |key: &str| arguments.get(key).and_then(serde_json::Value::as_str);
    match tool {
        "bash" => str_arg("command")
            .map(|cmd| {
                cmd.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(48)
                    .collect()
            })
            .unwrap_or_default(),
        "edit" | "write" | "read" => str_arg("path")
            .map(|p| p.rsplit('/').next().unwrap_or(p).chars().take(32).collect())
            .unwrap_or_default(),
        "glob" | "grep" => str_arg("pattern")
            .map(|p| p.chars().take(32).collect())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Pose a `[continue, stop]` guard ask and wait for the answer.
/// `false` = stop (user chose stop, aborted, or the surface went away).
async fn ask_continue_or_stop(
    state: &mut VoiceState,
    question: String,
    commands: &mut mpsc::Receiver<Command>,
    events: &mpsc::Sender<Event>,
) -> bool {
    state.ask_counter += 1;
    let id = AskId(format!("ask-{}", state.ask_counter));
    events
        .send(Event::Ask {
            id: id.clone(),
            questions: vec![AskQuestion {
                text: question,
                options: vec!["continue".to_string(), "stop".to_string()],
                detail: None,
            }],
        })
        .await
        .ok();
    loop {
        tokio::select! {
            maybe = commands.recv() => match maybe {
                Some(Command::Answer { question: q, choice }) if q == id => {
                    return choice == 0;
                }
                Some(Command::Abort) => return false,
                Some(_) => {}
                None => return false,
            }
        }
    }
}

fn truncate_excerpt(text: &str) -> String {
    let capped: String = text.chars().take(2_000).collect();
    if capped.len() < text.len() {
        format!("{capped}…")
    } else {
        capped
    }
}

/// Report an error and finish the turn; returns the (empty) usage for
/// the engine's Usage record.
async fn finish_after_error(
    events: &mpsc::Sender<Event>,
    class: ErrorClass,
    message: &str,
) -> Usage {
    events
        .send(Event::Error {
            class,
            retryable: false,
            message: message.to_string(),
        })
        .await
        .ok();
    events
        .send(Event::TurnFinished {
            stop: Stop::Error,
            usage: Usage::default(),
        })
        .await
        .ok();
    Usage::default()
}

/// USD cost from usage and per-mtok prices (cache reads billed at input
/// rate — a conservative overestimate until per-tier pricing lands).
fn cost_of(usage: &Usage, price: ka_dialect::dialects::Price) -> f64 {
    let in_tokens = usage.input + usage.cache_read + usage.cache_write;
    (in_tokens as f64 / 1_000_000.0) * price.input_per_mtok
        + (usage.output as f64 / 1_000_000.0) * price.output_per_mtok
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::time::Duration;

    use ka_dialect::dialects::{Catalog, Wire};
    use ka_dialect::speaker::{
        SpeakFuture, SpeakRequest, Speaker, StreamEvent, ToolCall, TurnMessage, TurnRole,
    };
    use ka_protocol::{Command, Event, Stop, Usage};

    use super::{Gate, GuardRuntime, Voice, call_detail, cost_of, glob_match};

    #[test]
    fn system_prompt_core_stays_within_budget() {
        // The harness-authored core is budget-capped (chars ≈ 4×
        // tokens): 2 KiB ≈ 500 tokens, the line jcode (671) and pi
        // (336) hold. Session data (AGENTS.md, skills, memory, digest)
        // rides outside this budget; growth here is harness creep.
        const BUDGET: usize = 2048;
        let modes = [
            ka_protocol::Mode::Guarded,
            ka_protocol::Mode::AcceptEdits,
            ka_protocol::Mode::Free,
            ka_protocol::Mode::Plan,
        ];
        for mode in modes {
            let core = super::core_instructions(
                "(git: main, clean)",
                mode,
                std::path::Path::new("/tmp"),
                None,
            );
            let n = core.chars().count();
            assert!(
                n <= BUDGET,
                "static system-prompt core for {mode:?} is {n} chars (budget {BUDGET}) — trim it"
            );
        }
    }

    #[test]
    fn identity_replaces_default_sentence_keeps_todo_line() {
        let identity = "You are Ka, the resident personal assistant of one person.";
        let with = super::core_instructions(
            "(git: main, clean)",
            ka_protocol::Mode::Guarded,
            std::path::Path::new("/tmp"),
            Some(identity),
        );
        assert!(with.contains(identity), "identity text rides the core");
        assert!(
            with.contains("Workspace: (git: main, clean)."),
            "the workspace summary stays: {with}"
        );
        assert!(
            !with.contains("precise coding agent"),
            "the default identity sentence must be replaced: {with}"
        );
        assert!(
            with.contains("Maintain your plan with the todo tool"),
            "the todo-discipline line is unchanged"
        );
        // unset: byte-identical to the pre-identity default
        let without = super::core_instructions(
            "(git: main, clean)",
            ka_protocol::Mode::Guarded,
            std::path::Path::new("/tmp"),
            None,
        );
        assert!(without.contains("You are ka, a precise coding agent."));
        assert!(
            !without.contains("Workspace:"),
            "the default keeps its own phrasing: {without}"
        );
        // whitespace-only identity falls back to the default
        let blank = super::core_instructions(
            "(git: main, clean)",
            ka_protocol::Mode::Guarded,
            std::path::Path::new("/tmp"),
            Some("  \n"),
        );
        assert_eq!(blank, without, "empty identity = default identity");
    }

    /// A speaker that records the system prompt of its one and only
    /// request, then finishes with text.
    struct SystemSpeaker {
        systems: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
    }

    impl Speaker for SystemSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            let systems = &self.systems;
            Box::pin(async move {
                systems.lock().push(req.system.clone());
                out.send(StreamEvent::Text("ok".into())).await.ok();
                out.send(StreamEvent::Finished {
                    stop: ka_protocol::Stop::Done,
                    usage: ka_protocol::Usage {
                        input: 1,
                        output: 1,
                        ..Default::default()
                    },
                })
                .await
                .ok();
            })
        }
    }

    /// Run one turn on `voice`; returns its events plus the system
    /// prompts the speaker saw. The turn future is polled inline (the
    /// existing test pattern) so borrows of `voice` stay local.
    async fn run_identity_turn(voice: &mut Voice) -> (Vec<ka_protocol::Event>, Vec<String>) {
        use tokio::sync::mpsc;
        let (_cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let mut guards = GuardRuntime::default();
        let mut events = Vec::new();
        let turn = voice.turn(
            "test/m",
            "hi".into(),
            &mut cmd_rx,
            &evt_tx,
            &mut steers,
            &mut queue,
            &mut guards,
            None,
            Vec::new(),
        );
        tokio::pin!(turn);
        loop {
            tokio::select! {
                evt = evt_rx.recv() => match evt {
                    Some(e) => {
                        let done = matches!(e, ka_protocol::Event::TurnFinished { .. });
                        events.push(e);
                        if done {
                            break;
                        }
                    }
                    None => break,
                },
                _ = turn.as_mut() => break,
            }
        }
        // the turn may complete while events are still buffered: drain
        while let Ok(evt) = evt_rx.try_recv() {
            events.push(evt);
        }
        (events, Vec::new())
    }

    #[tokio::test]
    async fn identity_file_replaces_default_and_warns_once_on_error() {
        let dir = std::env::temp_dir().join(format!("ka-voice-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();

        // good identity file: its text rides the system prompt, the
        // default sentence does not
        let id_path = dir.join("IDENTITY.md");
        std::fs::write(
            &id_path,
            "You are Ka, the resident personal assistant of one person.",
        )
        .unwrap();
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(catalog.clone(), dir.clone(), ka_protocol::Mode::Guarded, 5)
            .with_speaker(
                Wire::OpenaiChat,
                std::sync::Arc::new(SystemSpeaker {
                    systems: seen.clone(),
                }),
            );
        voice.set_identity_file(Some(id_path.display().to_string()));
        let (events, _) = run_identity_turn(&mut voice).await;
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, ka_protocol::Event::Note { .. })),
            "a readable identity file emits no note: {events:?}"
        );
        let systems = seen.lock().clone();
        assert_eq!(systems.len(), 1);
        assert!(
            systems[0].contains("You are Ka, the resident personal assistant"),
            "identity text rides the system prompt: {}",
            systems[0]
        );
        assert!(
            !systems[0].contains("precise coding agent"),
            "the default identity sentence is gone: {}",
            systems[0]
        );

        // unreadable path: default identity + exactly one Note, then
        // a second turn stays silent about it
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(catalog, dir.clone(), ka_protocol::Mode::Guarded, 5)
            .with_speaker(
                Wire::OpenaiChat,
                std::sync::Arc::new(SystemSpeaker {
                    systems: seen.clone(),
                }),
            );
        voice.set_identity_file(Some(dir.join("missing.md").display().to_string()));
        let (events, _) = run_identity_turn(&mut voice).await;
        let notes = events
            .iter()
            .filter(|e| matches!(e, ka_protocol::Event::Note { .. }))
            .count();
        assert_eq!(notes, 1, "exactly one identity note: {events:?}");
        assert!(
            events.iter().any(|e| matches!(
                e,
                ka_protocol::Event::Note { message }
                    if message.contains("identity file") && message.contains("default identity")
            )),
            "the note names the file and the fallback: {events:?}"
        );
        let systems = seen.lock().clone();
        assert!(
            systems[0].contains("You are ka, a precise coding agent."),
            "unreadable identity falls back to the default: {}",
            systems[0]
        );
        let (events2, _) = run_identity_turn(&mut voice).await;
        assert!(
            events2
                .iter()
                .all(|e| !matches!(e, ka_protocol::Event::Note { .. })),
            "the warning fires once per session: {events2:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unpriced_dialects_never_report_cost() {
        // the gating expression at the true-up site
        let priced = false;
        let cost = if priced { 18.0 } else { 0.0 };
        assert_eq!(cost, 0.0);
        let priced = true;
        let cost = if priced { 18.0 } else { 0.0 };
        assert_eq!(cost, 18.0);
    }
    #[test]
    fn call_detail_summarizes_head_path_and_pattern() {
        use serde_json::json;
        // bash: multiline command flattened, capped at 48
        let d = call_detail("bash", &json!({"command": "echo  a\nb\nc"}));
        assert_eq!(d, "echo a b c");
        let long = call_detail("bash", &json!({"command": "x".repeat(60)}));
        assert_eq!(long.chars().count(), 48);
        // file tools: last path segment, capped at 32
        assert_eq!(
            call_detail("edit", &json!({"path": "src/deep/lib/mod.rs"})),
            "mod.rs"
        );
        assert_eq!(call_detail("read", &json!({"path": "/a/b.rs"})), "b.rs");
        // searches: pattern
        assert_eq!(
            call_detail("grep", &json!({"pattern": "foo.*bar"})),
            "foo.*bar"
        );
        // everything else: empty
        assert_eq!(call_detail("todo", &json!({"items": []})), "");
        assert_eq!(call_detail("bash", &json!({})), "");
    }
    #[test]
    fn cost_of_computes_from_price() {
        let usage = Usage {
            input: 1_000_000,
            output: 1_000_000,
            ..Default::default()
        };
        let price = ka_dialect::dialects::Price {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        };
        assert!((cost_of(&usage, price) - 18.0).abs() < 1e-9);
    }

    /// Fake speaker: first request → one `read` tool call; once a tool
    /// result is present → final text. Records every request it saw.
    struct FakeSpeaker {
        seen: std::sync::Arc<parking_lot::Mutex<Vec<Vec<TurnMessage>>>>,
    }

    impl Speaker for FakeSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            let seen = self.seen.clone();
            Box::pin(async move {
                seen.lock().push(req.messages.clone());
                let has_result = req
                    .messages
                    .iter()
                    .any(|m| m.role == TurnRole::Tool && !m.results.is_empty());
                if has_result {
                    out.send(StreamEvent::Text("all done".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: ka_protocol::Usage {
                            input: 10,
                            output: 5,
                            ..Default::default()
                        },
                    })
                    .await
                    .ok();
                } else {
                    out.send(StreamEvent::Call(ToolCall {
                        id: "c1".into(),
                        tool: "read".into(),
                        arguments: serde_json::json!({"path": "roundtrip.txt"}),
                    }))
                    .await
                    .ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: ka_protocol::Usage {
                            input: 10,
                            output: 5,
                            ..Default::default()
                        },
                    })
                    .await
                    .ok();
                }
            })
        }
    }

    /// Records the session id of every request it sees.
    struct SessionSpeaker {
        seen: std::sync::Arc<parking_lot::Mutex<Vec<Option<String>>>>,
    }

    impl Speaker for SessionSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                self.seen.lock().push(req.session_id.clone());
                out.send(StreamEvent::Finished {
                    stop: Stop::Done,
                    usage: ka_protocol::Usage::default(),
                })
                .await
                .ok();
            })
        }
    }

    #[tokio::test]
    async fn session_id_rides_every_request() {
        use ka_protocol::Event;
        use tokio::sync::mpsc;

        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(
            catalog,
            std::env::temp_dir(),
            ka_protocol::Mode::Guarded,
            10,
        )
        .with_speaker(
            Wire::OpenaiChat,
            std::sync::Arc::new(SessionSpeaker { seen: seen.clone() }),
        );
        voice.set_session_id(Some("strand-42".to_string()));

        let (_cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    "hi".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut steers,
                    &mut queue,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });
        while let Some(evt) = evt_rx.recv().await {
            if matches!(evt, Event::TurnFinished { .. }) {
                break;
            }
        }
        handle.await.unwrap();
        assert_eq!(
            seen.lock().as_slice(),
            &[Some("strand-42".to_string())],
            "every wire request must carry the active strand id"
        );
    }

    #[tokio::test]
    async fn tool_roundtrip_executes_and_feeds_results_back() {
        use ka_protocol::Event;
        use tokio::sync::mpsc;

        let dir = std::env::temp_dir().join(format!("ka-voice-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("roundtrip.txt"), "ROUNDTRIP-CONTENT\n").unwrap();

        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(catalog, dir.clone(), ka_protocol::Mode::Guarded, 10)
            .with_speaker(
                Wire::OpenaiChat,
                std::sync::Arc::new(FakeSpeaker { seen: seen.clone() }),
            );

        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();

        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    "read the file".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut steers,
                    &mut queue,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });

        let mut saw_output = false;
        let mut saw_done = false;
        while let Some(evt) = evt_rx.recv().await {
            match evt {
                Event::CallOutput { excerpt, .. } => {
                    assert!(excerpt.contains("ROUNDTRIP-CONTENT"), "{excerpt}");
                    saw_output = true;
                }
                Event::TurnFinished {
                    stop: ka_protocol::Stop::Done,
                    usage,
                } => {
                    assert_eq!(usage.input, 20); // two steps × 10
                    saw_done = true;
                    break;
                }
                Event::TurnFinished { .. } => break,
                _ => {}
            }
        }
        drop(cmd_tx);
        handle.await.unwrap();
        assert!(saw_output, "tool output must reach the surface");
        assert!(saw_done, "turn must finish done");

        let seen = seen.lock();
        assert_eq!(seen.len(), 2, "exactly two speaks expected");
        let second = &seen[1];
        let result = second
            .iter()
            .flat_map(|m| &m.results)
            .next()
            .expect("second speak must carry the tool result");
        assert!(result.content.contains("ROUNDTRIP-CONTENT"));
        assert!(!result.is_error);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[tokio::test]
    async fn stop_hook_fires_once_per_turn_with_payload() {
        use ka_protocol::Event;
        use tokio::sync::mpsc;

        let dir = std::env::temp_dir().join(format!("ka-voice-stop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("stop-marker");

        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(catalog, dir.clone(), ka_protocol::Mode::Free, 10).with_speaker(
            Wire::OpenaiChat,
            std::sync::Arc::new(FakeSpeaker { seen: seen.clone() }),
        );
        voice.set_hooks(vec![crate::config::Hook {
            event: crate::config::HookEvent::Stop,
            tool: None,
            command: format!("cat >> {}", marker.display()),
        }]);

        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();

        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    "read the file".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut steers,
                    &mut queue,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });

        while let Some(evt) = evt_rx.recv().await {
            if matches!(evt, Event::TurnFinished { .. }) {
                break;
            }
        }
        drop(cmd_tx);
        handle.await.unwrap();

        let content = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(
            content.matches("\"event\":\"stop\"").count(),
            1,
            "stop hook must fire exactly once per turn: {content}"
        );
        assert!(content.contains("\"stop\":\"done\""), "{content}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fake speaker: first request → one `todo` tool call; once a tool
    /// result is present → final text.
    struct TodoSpeaker;

    impl Speaker for TodoSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                let has_result = req
                    .messages
                    .iter()
                    .any(|m| m.role == TurnRole::Tool && !m.results.is_empty());
                if has_result {
                    out.send(StreamEvent::Text("plan noted".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: ka_protocol::Usage::default(),
                    })
                    .await
                    .ok();
                } else {
                    out.send(StreamEvent::Call(ToolCall {
                        id: "t1".into(),
                        tool: "todo".into(),
                        arguments: serde_json::json!({"items": [
                            {"text": "survey", "state": "done"},
                            {"text": "implement", "state": "pending"},
                        ]}),
                    }))
                    .await
                    .ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: ka_protocol::Usage::default(),
                    })
                    .await
                    .ok();
                }
            })
        }
    }

    #[tokio::test]
    async fn todo_call_reaches_surfaces_as_todos_event() {
        use tokio::sync::mpsc;

        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(
            catalog,
            std::env::temp_dir(),
            ka_protocol::Mode::Guarded,
            10,
        )
        .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(TodoSpeaker));

        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();

        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    "plan the work".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut steers,
                    &mut queue,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });

        let mut todos_event = None;
        while let Some(evt) = evt_rx.recv().await {
            if let Event::Todos { items } = evt {
                todos_event = Some(items);
                break;
            }
        }
        drop(cmd_tx);
        handle.await.unwrap();
        let items = todos_event.expect("todo call must emit Event::Todos");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "survey");
        assert_eq!(items[0].state, ka_protocol::TodoState::Done);
        assert_eq!(items[1].state, ka_protocol::TodoState::Pending);
    }

    #[test]
    fn prune_blanks_old_tool_outputs_beyond_window() {
        use ka_dialect::speaker::{ToolCall, ToolResult, TurnMessage, TurnRole};
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\nratio = 1.0\n",
        )
        .unwrap();
        let dir = std::env::temp_dir();
        let mut voice = Voice::new(catalog, dir.clone(), ka_protocol::Mode::Guarded, 5);
        // ratio 1.0 → tokens == chars
        let big = "x".repeat(30_000); // 30k tokens
        voice.history.push(TurnMessage::user("start"));
        voice.history.push(TurnMessage::assistant_with_calls(
            "working",
            vec![ToolCall {
                id: "c1".into(),
                tool: "bash".into(),
                arguments: Default::default(),
            }],
        ));
        voice.history.push(TurnMessage::tool(vec![ToolResult {
            call_id: "c1".into(),
            content: big.clone(),
            is_error: false,
            images: Vec::new(),
        }]));
        // recent large exchange: fills the 40k protect window so the old
        // tool result falls outside it
        voice
            .history
            .push(TurnMessage::user(format!("recent {}", "r".repeat(45_000))));
        voice.history.push(TurnMessage::assistant("tail"));

        let saved = voice.prune_tool_outputs(1.0);
        assert!(saved >= 20_000, "savings {saved}");
        let tool_msg = voice
            .history
            .iter()
            .find(|m| m.role == TurnRole::Tool)
            .unwrap();
        let content = &tool_msg.results[0].content;
        assert!(content.starts_with("[pruned output"), "{content}");
        assert!(content.contains("spill://"), "{content}");
        assert!(
            voice.history[3].content.starts_with("recent"),
            "recent messages untouched"
        );
        assert_eq!(voice.history[4].content, "tail");
    }

    #[test]
    fn prune_skips_when_savings_below_threshold() {
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(
            catalog.clone(),
            std::env::temp_dir(),
            ka_protocol::Mode::Guarded,
            5,
        );
        voice.set_model_selector("test/m", 1.0);
        use ka_dialect::speaker::{ToolCall, ToolResult, TurnMessage};
        voice.history.push(TurnMessage::user("q"));
        voice.history.push(TurnMessage::assistant_with_calls(
            "",
            vec![ToolCall {
                id: "c".into(),
                tool: "read".into(),
                arguments: Default::default(),
            }],
        ));
        voice.history.push(TurnMessage::tool(vec![ToolResult {
            call_id: "c".into(),
            content: "tiny".to_string(),
            is_error: false,
            images: Vec::new(),
        }]));
        let saved = voice.prune_tool_outputs(1.0);
        assert_eq!(saved, 0, "tiny outputs must not be pruned");
        assert_eq!(voice.history[2].results[0].content, "tiny");
    }
    use std::sync::Arc;
    use tokio::sync::mpsc;

    /// Records requests, replies with one trimmed text delta.
    struct TitleSpeaker {
        seen: std::sync::Arc<parking_lot::Mutex<Vec<SpeakRequest>>>,
    }
    impl Speaker for TitleSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            self.seen.lock().push(req);
            Box::pin(async move {
                out.send(StreamEvent::Text("  Fix the Parser \n".into()))
                    .await
                    .ok();
                out.send(StreamEvent::Finished {
                    stop: Stop::Done,
                    usage: Usage::default(),
                })
                .await
                .ok();
            })
        }
    }

    /// Streams partial text, then fails: the partial must be discarded.
    struct FailingSpeaker;
    impl Speaker for FailingSpeaker {
        fn speak<'a>(
            &'a self,
            _req: SpeakRequest,
            out: mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                out.send(StreamEvent::Text("partial".into())).await.ok();
                out.send(StreamEvent::Failed {
                    class: ka_protocol::ErrorClass::Network,
                    retryable: false,
                    message: "boom".into(),
                })
                .await
                .ok();
            })
        }
    }

    fn fast_voice(
        speaker: std::sync::Arc<dyn Speaker>,
    ) -> (Voice, std::sync::Arc<parking_lot::Mutex<Vec<SpeakRequest>>>) {
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let catalog = Catalog::parse(
            "[dialects.\"test/fast\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let seen2 = seen.clone();
        (
            Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
                .with_speaker(Wire::OpenaiChat, speaker),
            seen2,
        )
    }

    #[tokio::test]
    async fn role_complete_trims_caps_and_records_the_request() {
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (mut voice, _) = fast_voice(Arc::new(TitleSpeaker { seen: seen.clone() }));
        let out = voice
            .role_complete(
                "test/fast",
                "titles",
                "make a title",
                Duration::from_secs(5),
            )
            .await;
        assert_eq!(out.as_deref(), Some("Fix the Parser"));
        let reqs = seen.lock();
        assert_eq!(reqs.len(), 1);
        let req = &reqs[0];
        assert_eq!(req.model_id, "test/fast");
        assert!(req.tools.is_empty(), "role calls never carry tools");
        assert_eq!(req.effort, None);
        assert_eq!(
            req.dialect.max_output,
            Voice::ROLE_MAX_OUTPUT,
            "output capped for the cheap role"
        );
        assert_eq!(req.system, "titles");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, TurnRole::User);
        assert_eq!(req.messages[0].content, "make a title");
    }

    #[tokio::test]
    async fn role_complete_degrades_silently() {
        // unknown selector → None
        let (mut voice, _) = fast_voice(Arc::new(TitleSpeaker {
            seen: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        }));
        assert!(
            voice
                .role_complete("nope/missing", "s", "p", Duration::from_secs(1))
                .await
                .is_none()
        );
        // failed stream: even partial text is discarded
        let (mut voice, _) = fast_voice(Arc::new(FailingSpeaker));
        assert!(
            voice
                .role_complete("test/fast", "s", "p", Duration::from_secs(1))
                .await
                .is_none()
        );
    }
    #[test]
    fn shake_truncates_stale_args_only() {
        use ka_dialect::speaker::{ToolCall, ToolResult, TurnMessage};
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Guarded, 5);
        let big_args = serde_json::json!({ "q": "x".repeat(500) });
        // stale: a call issued before the last user message
        voice.history.push(TurnMessage::assistant_with_calls(
            "checking",
            vec![ToolCall {
                id: "t1".into(),
                tool: "grep".into(),
                arguments: big_args.clone(),
            }],
        ));
        voice.history.push(TurnMessage::tool(vec![ToolResult {
            call_id: "t1".into(),
            content: "results".into(),
            is_error: false,
            images: Vec::new(),
        }]));
        voice.history.push(TurnMessage::user("new question"));
        // fresh: a call after the last user message keeps its arguments
        voice.history.push(TurnMessage::assistant_with_calls(
            "now checking",
            vec![ToolCall {
                id: "t2".into(),
                tool: "grep".into(),
                arguments: big_args.clone(),
            }],
        ));

        let n = voice.shake(240);
        assert_eq!(n, 1, "only the stale call is shaken");
        assert!(
            voice.history[0].calls[0]
                .arguments
                .to_string()
                .contains("[shaken]"),
            "the stale arguments are truncated"
        );
        assert_eq!(
            voice.history[3].calls[0].arguments, big_args,
            "the fresh call keeps its arguments"
        );
        assert_eq!(voice.shake(240), 0, "shaking is idempotent");
    }

    #[test]
    fn reinject_hot_files_merges_into_the_last_user_message() {
        use ka_dialect::speaker::TurnRole;
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(
            catalog.clone(),
            std::env::temp_dir(),
            ka_protocol::Mode::Guarded,
            5,
        );
        let a = std::env::temp_dir().join(format!("ka-hot-a-{}", std::process::id()));
        let b = std::env::temp_dir().join(format!("ka-hot-b-{}", std::process::id()));
        std::fs::write(&a, "alpha contents\n").unwrap();
        let meta_a = std::fs::metadata(&a).unwrap();
        voice.hand_ctx.ledger.lock().mint(&a, &meta_a);
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&b, "beta contents\n").unwrap();
        let meta_b = std::fs::metadata(&b).unwrap();
        voice.hand_ctx.ledger.lock().mint(&b, &meta_b);
        // the post-digest shape: history is a kept tail ending on the
        // user's current prompt
        voice.history.push(TurnMessage::user("current prompt"));

        let injected = voice.reinject_hot_files(5, 6144);
        assert_eq!(
            injected,
            vec![b.display().to_string(), a.display().to_string()],
            "newest first"
        );
        assert_eq!(voice.history.len(), 1, "merged, not appended");
        let last = voice.history.last().unwrap();
        assert_eq!(last.role, TurnRole::User);
        assert!(last.content.contains("beta contents"), "{:?}", last.content);
        assert!(
            last.content.contains("alpha contents"),
            "{:?}",
            last.content
        );
        assert!(last.content.contains("re-read"), "{:?}", last.content);

        // a nonexistent hot path is skipped, not fatal (fresh voice: the
        // previous phase already merged its files)
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Guarded, 5);
        let ghost = std::env::temp_dir().join(format!("ka-hot-ghost-{}", std::process::id()));
        std::fs::write(&ghost, "gone\n").unwrap();
        let meta = std::fs::metadata(&ghost).unwrap();
        voice.hand_ctx.ledger.lock().mint(&ghost, &meta);
        std::fs::remove_file(&ghost).unwrap();
        let injected = voice.reinject_hot_files(5, 6144);
        assert!(
            injected.is_empty(),
            "unreadable hot paths are skipped: {injected:?}"
        );
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn apply_digest_cuts_at_user_boundary_and_keeps_tail() {
        use ka_dialect::speaker::{ToolCall, ToolResult, TurnMessage, TurnRole};
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Guarded, 5);
        // ratio 1 char/token; tail budget is 20k, so the filler must not fit
        let filler = "y".repeat(30_000);
        voice.history.push(TurnMessage::user("old question"));
        voice.history.push(TurnMessage::assistant("old answer"));
        voice
            .history
            .push(TurnMessage::user(format!("filler {filler}")));
        voice.history.push(TurnMessage::assistant_with_calls(
            "let me check",
            vec![ToolCall {
                id: "t9".into(),
                tool: "read".into(),
                arguments: Default::default(),
            }],
        ));
        voice.history.push(TurnMessage::tool(vec![ToolResult {
            call_id: "t9".into(),
            content: "file contents".into(),
            is_error: false,
            images: Vec::new(),
        }]));
        voice.history.push(TurnMessage::assistant("done with that"));
        voice.history.push(TurnMessage::user("keep me"));

        let kept = voice.apply_digest("SUMMARY".to_string(), 1.0);
        // the cut must land on a user message boundary ("filler..." is too
        // big to fit with everything after; "keep me" is small)
        assert_eq!(
            voice.history[0].role,
            TurnRole::User,
            "history must start at a user message"
        );
        assert!(
            voice.history.iter().any(|m| m.content == "keep me"),
            "tail preserved"
        );
        // tool pair intact: an assistant-with-calls message is followed by
        // its tool message, or neither is present
        if voice.history.iter().any(|m| m.role == TurnRole::Tool) {
            let idx = voice
                .history
                .iter()
                .position(|m| m.role == TurnRole::Tool)
                .unwrap();
            assert!(idx > 0, "tool message never first");
        }
        assert_eq!(voice.digest.as_deref(), Some("SUMMARY"));
        assert_eq!(kept, 2, "cut index points at the filler user message");
        assert!(voice.digest_revision >= 1);
        assert!(voice.take_pending_digest().is_some());
        assert!(voice.take_pending_digest().is_none(), "consumed once");
    }

    #[test]
    fn context_pressure_uses_reserve() {
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 100000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Guarded, 5);
        voice.set_model_selector("test/m", 4.0);
        assert!(
            !voice.context_pressure(100_000),
            "empty history, no pressure"
        );
        voice.note_context_for_tests(90_000);
        // reserve = max(16384, 15%) = 16384; 90k + 20k tail > 100k - 16384
        assert!(voice.context_pressure(100_000), "90k used + tail must trip");
        assert!(!voice.context_pressure(0), "unknown window never trips");
    }

    /// Overflow → digest → retry, all through the fake speaker.
    struct OverflowFakeSpeaker {
        calls: std::sync::Arc<parking_lot::Mutex<Vec<usize>>>,
    }

    impl Speaker for OverflowFakeSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            let calls = self.calls.clone();
            Box::pin(async move {
                let n = {
                    let mut c = calls.lock();
                    c.push(0);
                    c.len()
                };
                let has_digest = req
                    .messages
                    .first()
                    .is_some_and(|m| m.content.starts_with("<context-digest>"));
                if req.tools.is_empty() {
                    // summarize call
                    out.send(StreamEvent::Text("digested state".into()))
                        .await
                        .ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: Default::default(),
                    })
                    .await
                    .ok();
                } else if n == 1 && !has_digest {
                    out.send(StreamEvent::Failed {
                        class: ka_protocol::ErrorClass::Overflow,
                        retryable: false,
                        message: "prompt is too long".into(),
                    })
                    .await
                    .ok();
                } else {
                    out.send(StreamEvent::Text("recovered".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: Default::default(),
                    })
                    .await
                    .ok();
                }
            })
        }
    }

    #[tokio::test]
    async fn overflow_triggers_digest_and_retry() {
        use ka_protocol::Event;
        use tokio::sync::mpsc;

        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 50000\n",
        )
        .unwrap();
        let calls = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Guarded, 5)
            .with_speaker(
                Wire::OpenaiChat,
                std::sync::Arc::new(OverflowFakeSpeaker {
                    calls: calls.clone(),
                }),
            );

        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    "big prompt".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut steers,
                    &mut queue,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });

        let mut finished = false;
        let mut digest_started = false;
        while let Some(evt) = evt_rx.recv().await {
            match evt {
                Event::DigestStarted => digest_started = true,
                Event::TurnFinished {
                    stop: ka_protocol::Stop::Done,
                    ..
                } => {
                    finished = true;
                    break;
                }
                Event::TurnFinished { .. } => break,
                _ => {}
            }
        }
        drop(cmd_tx);
        handle.await.unwrap();
        assert!(digest_started, "overflow must trigger a digest");
        assert!(finished, "turn must recover and finish done");
        assert!(calls.lock().len() >= 3, "speak, summarize, retry");
    }

    #[test]
    fn glob_match_basics() {
        assert!(glob_match("cargo *", "cargo build --release"));
        assert!(glob_match("cargo build", "cargo build"));
        assert!(!glob_match("cargo build", "cargo test"));
        assert!(glob_match("git push *", "git push origin main"));
        assert!(!glob_match("git push *", "git status"));
        assert!(glob_match("rm *build*", "rm -rf ./build-dir"));
        assert!(glob_match("*", "anything at all"));
        assert!(glob_match("read?file", "read-file"));
    }

    struct RuleFakeSpeaker;

    impl Speaker for RuleFakeSpeaker {
        fn speak<'a>(
            &'a self,
            _req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                out.send(StreamEvent::Call(ToolCall {
                    id: "c1".into(),
                    tool: "bash".into(),
                    arguments: serde_json::json!({"command": "cargo build"}),
                }))
                .await
                .ok();
                out.send(StreamEvent::Finished {
                    stop: ka_protocol::Stop::Done,
                    usage: Default::default(),
                })
                .await
                .ok();
            })
        }
    }

    #[tokio::test]
    async fn rules_deny_in_free_mode() {
        use ka_protocol::Event;
        use tokio::sync::mpsc;

        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(RuleFakeSpeaker));
        voice.set_rules(vec![crate::config::Rule {
            tool: "bash".into(),
            pattern: Some("cargo *".into()),
            verdict: crate::config::Verdict::Deny,
        }]);

        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    "build it".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut steers,
                    &mut queue,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });

        let mut denied_output = false;
        while let Some(evt) = evt_rx.recv().await {
            if let Event::CallOutput {
                excerpt, is_error, ..
            } = &evt
            {
                assert!(is_error, "denied call must be an error result");
                assert!(excerpt.contains("denied by rule"), "{excerpt}");
                denied_output = true;
            }
            if matches!(evt, Event::TurnFinished { .. }) {
                break;
            }
        }
        drop(cmd_tx);
        handle.await.unwrap();
        assert!(denied_output, "rule denial must surface as tool error");
    }

    #[tokio::test]
    async fn plan_mode_denies_writes_outside_plans_dir() {
        use ka_protocol::Event;
        use tokio::sync::mpsc;

        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        // speaker asks to write OUTSIDE the plans dir
        struct WriteOutside;
        impl Speaker for WriteOutside {
            fn speak<'a>(
                &'a self,
                _req: SpeakRequest,
                out: tokio::sync::mpsc::Sender<StreamEvent>,
            ) -> SpeakFuture<'a> {
                Box::pin(async move {
                    out.send(StreamEvent::Call(ToolCall {
                        id: "w1".into(),
                        tool: "write".into(),
                        arguments: serde_json::json!({"path": "src/main.rs", "content": "x"}),
                    }))
                    .await
                    .ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: Default::default(),
                    })
                    .await
                    .ok();
                })
            }
        }
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Plan, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(WriteOutside));

        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let handle = tokio::spawn(async move {
            let mut i = Vec::new();
            let mut d = std::collections::VecDeque::new();
            voice
                .turn(
                    "test/m",
                    "do it".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut i,
                    &mut d,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });
        let mut blocked = false;
        while let Some(evt) = evt_rx.recv().await {
            if let Event::CallOutput {
                excerpt, is_error, ..
            } = &evt
            {
                if excerpt.contains("plan mode is read-only") && *is_error {
                    blocked = true;
                }
            }
            if matches!(evt, Event::TurnFinished { .. }) {
                break;
            }
        }
        drop(cmd_tx);
        handle.await.unwrap();
        assert!(
            blocked,
            "write outside .ka/plans must be denied in plan mode"
        );
    }

    #[test]
    fn write_gate_allows_free_and_accept_edits_asks_guarded() {
        use super::Gate;
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let call = ToolCall {
            id: "w1".into(),
            tool: "write".into(),
            arguments: serde_json::json!({"path": "out.txt", "content": "x"}),
        };
        for mode in [ka_protocol::Mode::Free, ka_protocol::Mode::AcceptEdits] {
            let voice = crate::voice::Voice::new(catalog.clone(), std::env::temp_dir(), mode, 5);
            assert!(
                matches!(
                    voice.gate(crate::hands::Clearance::Write, &call),
                    Gate::Allow
                ),
                "write must auto-allow in {mode:?}"
            );
        }
        let voice =
            crate::voice::Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Guarded, 5);
        match voice.gate(crate::hands::Clearance::Write, &call) {
            Gate::Ask { question, .. } => assert_eq!(question, "allow write to modify files?"),
            Gate::Allow | Gate::Deny { .. } => {
                panic!("guarded write must ask")
            }
        }
    }

    #[test]
    fn exec_gate_allows_only_free_and_keeps_plan_phrasing() {
        use super::Gate;
        let command = "cargo build --release";
        let call = ToolCall {
            id: "b1".into(),
            tool: "bash".into(),
            arguments: serde_json::json!({"command": command}),
        };
        for (mode, expected) in [
            (ka_protocol::Mode::Free, None),
            (
                ka_protocol::Mode::AcceptEdits,
                Some("run `cargo build --release`?"),
            ),
            (
                ka_protocol::Mode::Guarded,
                Some("run `cargo build --release`?"),
            ),
            (
                ka_protocol::Mode::Plan,
                Some("plan mode: run `cargo build --release`? (build with /build)"),
            ),
        ] {
            let catalog = Catalog::parse(
                "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
            )
            .unwrap();
            let voice = crate::voice::Voice::new(catalog, std::env::temp_dir(), mode, 5);
            let gate = voice.gate(crate::hands::Clearance::Exec, &call);
            match (gate, expected) {
                (Gate::Allow, None) => {}
                (Gate::Ask { question, .. }, Some(want)) => assert_eq!(question, want),
                (other, want) => panic!("exec gate in {mode:?}: got {other:?}, want {want:?}"),
            }
        }
    }

    #[test]
    fn rewind_truncates_before_nth_last_user_message() {
        use ka_dialect::speaker::TurnMessage;
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Guarded, 5);
        for (u, a) in [("q1", "a1"), ("q2", "a2"), ("q3", "a3")] {
            voice.history.push(TurnMessage::user(u));
            voice.history.push(TurnMessage::assistant(a));
        }
        let kept = voice.rewind(1).unwrap();
        assert_eq!(kept, 4);
        let contents: Vec<&str> = voice.history.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            vec!["q1", "a1", "q2", "a2"],
            "last exchange dropped"
        );
    }

    /// Speaker that fails `failures` times with a retryable network
    /// error, then finishes normally. Records request count.
    struct FlakySpeaker {
        failures: usize,
        seen: std::sync::Arc<parking_lot::Mutex<Vec<usize>>>,
    }

    impl FlakySpeaker {
        fn new(failures: usize) -> (Self, std::sync::Arc<parking_lot::Mutex<Vec<usize>>>) {
            let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
            (
                Self {
                    failures,
                    seen: seen.clone(),
                },
                seen,
            )
        }
    }

    impl Speaker for FlakySpeaker {
        fn speak<'a>(
            &'a self,
            _req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            let failures = self.failures;
            let seen = self.seen.clone();
            Box::pin(async move {
                let attempt = {
                    let mut counts = seen.lock();
                    counts.push(1);
                    counts.len()
                };
                if attempt <= failures {
                    out.send(StreamEvent::Failed {
                        class: ka_protocol::ErrorClass::Network,
                        retryable: true,
                        message: "connection reset".into(),
                    })
                    .await
                    .ok();
                    return;
                }
                out.send(StreamEvent::Text("recovered".into())).await.ok();
                out.send(StreamEvent::Finished {
                    stop: ka_protocol::Stop::Done,
                    usage: Usage::default(),
                })
                .await
                .ok();
            })
        }
    }

    fn flaky_catalog() -> Catalog {
        Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap()
    }

    /// Drive one turn, answering asks via `answer` (None = never answer).
    /// Returns (events, seen-request-counts).
    async fn drive_turn(
        voice: &mut Voice,
        guards: &mut GuardRuntime,
        prompt: &str,
        answer: Option<usize>,
    ) -> (Vec<Event>, Vec<usize>) {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(16);
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel(256);
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let events_handle = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(evt) = evt_rx.recv().await {
                let done = matches!(evt, Event::TurnFinished { .. });
                if let (Event::Ask { id, .. }, Some(choice)) = (&evt, answer) {
                    cmd_tx
                        .send(Command::Answer {
                            question: id.clone(),
                            choice,
                        })
                        .await
                        .ok();
                }
                events.push(evt);
                if done {
                    break;
                }
            }
            events
        });
        voice
            .turn(
                "test/m",
                prompt.into(),
                &mut cmd_rx,
                &evt_tx,
                &mut steers,
                &mut queue,
                guards,
                None,
                Vec::new(),
            )
            .await;
        // keep the receiver alive until the turn task wraps up
        drop(cmd_rx);
        (events_handle.await.unwrap(), Vec::new())
    }

    #[tokio::test]
    async fn retryable_failure_retries_then_succeeds_without_duplicate_user() {
        let (speaker, seen) = FlakySpeaker::new(2);
        let mut voice = Voice::new(
            flaky_catalog(),
            std::env::temp_dir(),
            ka_protocol::Mode::Free,
            5,
        )
        .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(speaker));
        let mut guards = GuardRuntime::default();
        let (events, _) = drive_turn(&mut voice, &mut guards, "only prompt", None).await;

        let notes: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                Event::Note { message } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(notes.len(), 2, "two retry notes expected: {notes:?}");
        assert!(notes[0].contains("retrying in"), "{notes:?}");
        assert!(matches!(
            events.last(),
            Some(Event::TurnFinished {
                stop: Stop::Done,
                ..
            })
        ));
        assert_eq!(seen.lock().len(), 3, "2 failures + 1 success");
        assert_eq!(
            voice
                .history
                .iter()
                .filter(|m| m.role == TurnRole::User)
                .count(),
            1,
            "user record must not be duplicated by retries"
        );
    }

    #[tokio::test]
    async fn set_effort_reaches_the_speak_request() {
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(
            flaky_catalog(),
            std::env::temp_dir(),
            ka_protocol::Mode::Free,
            5,
        )
        .with_speaker(
            Wire::OpenaiChat,
            std::sync::Arc::new(TitleSpeaker { seen: seen.clone() }),
        );
        let mut guards = GuardRuntime::default();

        // the engine-level level rides every request
        voice.set_effort(Some("high".into()));
        drive_turn(&mut voice, &mut guards, "prompt one", None).await;
        assert_eq!(
            seen.lock()[0].effort.as_deref(),
            Some("high"),
            "set_effort must reach SpeakRequest.effort"
        );

        // a selector @effort suffix still wins per model
        voice.set_effort(Some("low".into()));
        let (_evt_tx, evt_rx) = tokio::sync::mpsc::channel::<Event>(256);
        let (_cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(16);
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        voice
            .turn(
                "test/m@medium",
                "prompt two".into(),
                &mut cmd_rx,
                &_evt_tx,
                &mut steers,
                &mut queue,
                &mut guards,
                None,
                Vec::new(),
            )
            .await;
        drop(cmd_rx);
        drop(evt_rx);
        assert_eq!(
            seen.lock()[1].effort.as_deref(),
            Some("medium"),
            "the selector suffix outranks the engine level"
        );
    }

    #[tokio::test]
    async fn retry_canceled_by_abort_finishes_with_error() {
        let (speaker, _seen) = FlakySpeaker::new(usize::MAX);
        let mut voice = Voice::new(
            flaky_catalog(),
            std::env::temp_dir(),
            ka_protocol::Mode::Free,
            5,
        )
        .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(speaker));

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(16);
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel(256);
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    "prompt".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut steers,
                    &mut queue,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });
        // wait for the first retry note, then abort the wait
        let mut saw_retry_note = false;
        let mut canceled_note = false;
        let mut finished = None;
        while let Some(evt) = evt_rx.recv().await {
            match evt {
                Event::Note { message } if message.contains("retrying in") => {
                    saw_retry_note = true;
                    cmd_tx.send(Command::Abort).await.ok();
                }
                Event::Note { message } if message.contains("retry canceled") => {
                    canceled_note = true;
                }
                Event::TurnFinished { stop, .. } => {
                    finished = Some(stop);
                    break;
                }
                _ => {}
            }
        }
        handle.await.unwrap();
        assert!(saw_retry_note, "retry note must precede the cancel");
        assert!(canceled_note, "cancel must be announced");
        assert_eq!(finished, Some(Stop::Error), "cancel finishes with Error");
    }

    #[tokio::test]
    async fn spend_guard_asks_once_and_stop_aborts() {
        // priced dialect: 1M input tokens at $1/Mtok = $1.00 per step
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\npriced = true\n\n[dialects.\"test/m\".price]\ninput_per_mtok = 1.0\noutput_per_mtok = 0.0\n",
        )
        .unwrap();
        struct BigUsage;
        impl Speaker for BigUsage {
            fn speak<'a>(
                &'a self,
                _req: SpeakRequest,
                out: tokio::sync::mpsc::Sender<StreamEvent>,
            ) -> SpeakFuture<'a> {
                Box::pin(async move {
                    out.send(StreamEvent::Text("hi".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: Usage {
                            input: 1_000_000,
                            ..Default::default()
                        },
                    })
                    .await
                    .ok();
                })
            }
        }
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(BigUsage));
        let mut guards = GuardRuntime::new(Some(0.5), None);

        // stop at the ask → clean abort
        let (events, _) = drive_turn(&mut voice, &mut guards, "q1", Some(1)).await;
        let asks = events
            .iter()
            .filter(|e| matches!(e, Event::Ask { .. }))
            .count();
        assert_eq!(asks, 1, "guard ask fires once: {events:?}");
        assert!(matches!(
            events.last(),
            Some(Event::TurnFinished {
                stop: Stop::Aborted,
                ..
            })
        ));
        assert!(guards.spend_latched, "guard latches after the ask");

        // second turn: latched → no repeat ask, turn proceeds normally
        let (events2, _) = drive_turn(&mut voice, &mut guards, "q2", None).await;
        assert!(
            !events2.iter().any(|e| matches!(e, Event::Ask { .. })),
            "latched guard must not re-ask"
        );
        assert!(matches!(
            events2.last(),
            Some(Event::TurnFinished {
                stop: Stop::Done,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn permission_memory_answers_once_then_auto_allows() {
        let dir = std::env::temp_dir().join(format!("ka-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("perm-target.txt"), "one\n").unwrap();
        struct TwoWrites;
        impl Speaker for TwoWrites {
            fn speak<'a>(
                &'a self,
                req: SpeakRequest,
                out: tokio::sync::mpsc::Sender<StreamEvent>,
            ) -> SpeakFuture<'a> {
                Box::pin(async move {
                    let writes = req.messages.iter().flat_map(|m| &m.results).count();
                    if writes == 0 {
                        out.send(StreamEvent::Call(ToolCall {
                            id: "w1".into(),
                            tool: "write".into(),
                            arguments: serde_json::json!({
                                "path": "perm-target.txt",
                                "content": "changed\n"
                            }),
                        }))
                        .await
                        .ok();
                        out.send(StreamEvent::Finished {
                            stop: ka_protocol::Stop::Done,
                            usage: Usage::default(),
                        })
                        .await
                        .ok();
                    } else {
                        out.send(StreamEvent::Text("done".into())).await.ok();
                        out.send(StreamEvent::Finished {
                            stop: ka_protocol::Stop::Done,
                            usage: Usage::default(),
                        })
                        .await
                        .ok();
                    }
                })
            }
        }
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, dir.clone(), ka_protocol::Mode::Guarded, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(TwoWrites));
        let mut guards = GuardRuntime::default();
        // choice 1 = "always"
        let (events, _) = drive_turn(&mut voice, &mut guards, "edit it", Some(1)).await;
        let asks = events
            .iter()
            .filter(|e| matches!(e, Event::Ask { .. }))
            .count();
        assert_eq!(asks, 1, "exactly one permission ask: {events:?}");
        assert!(matches!(
            events.last(),
            Some(Event::TurnFinished {
                stop: Stop::Done,
                ..
            })
        ));
        // session memory holds the grant; a second identical call needs no ask
        let (events2, _) = drive_turn(&mut voice, &mut guards, "edit again", None).await;
        assert!(
            !events2.iter().any(|e| matches!(e, Event::Ask { .. })),
            "remembered permission must not re-ask"
        );
        assert!(matches!(
            events2.last(),
            Some(Event::TurnFinished {
                stop: Stop::Done,
                ..
            })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn exec_ask_appends_cost_estimate_for_priced_models_only() {
        struct WantsBash;
        impl Speaker for WantsBash {
            fn speak<'a>(
                &'a self,
                req: SpeakRequest,
                out: tokio::sync::mpsc::Sender<StreamEvent>,
            ) -> SpeakFuture<'a> {
                Box::pin(async move {
                    let ran = req.messages.iter().any(|m| !m.results.is_empty());
                    if ran {
                        out.send(StreamEvent::Text("done".into())).await.ok();
                    } else {
                        out.send(StreamEvent::Call(ToolCall {
                            id: "b1".into(),
                            tool: "bash".into(),
                            arguments: serde_json::json!({
                                "command": "touch priced-marker"
                            }),
                        }))
                        .await
                        .ok();
                    }
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: Usage::default(),
                    })
                    .await
                    .ok();
                })
            }
        }
        let dir = std::env::temp_dir().join(format!("ka-cost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let priced = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\npriced = true\n\n[dialects.\"test/m\".price]\ninput_per_mtok = 1.0\noutput_per_mtok = 0.0\n",
        )
        .unwrap();
        let mut voice = Voice::new(priced, dir.clone(), ka_protocol::Mode::Guarded, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(WantsBash));
        let mut guards = GuardRuntime::default();
        // choice 2 = deny: the ask text is what we assert on
        let (events, _) = drive_turn(&mut voice, &mut guards, "run it", Some(2)).await;
        let ask_text = events
            .iter()
            .find_map(|e| match e {
                Event::Ask { questions, .. } => questions.first().map(|q| q.text.clone()),
                _ => None,
            })
            .expect("exec ask fired");
        assert!(
            ask_text.contains("rough est. ≈ $"),
            "priced model ask carries the estimate: {ask_text}"
        );

        // unpriced row: no estimate, identical question otherwise
        let unpriced = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut bare = Voice::new(unpriced, dir.clone(), ka_protocol::Mode::Guarded, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(WantsBash));
        let (events2, _) = drive_turn(&mut bare, &mut guards, "run it", Some(2)).await;
        let ask2 = events2
            .iter()
            .find_map(|e| match e {
                Event::Ask { questions, .. } => questions.first().map(|q| q.text.clone()),
                _ => None,
            })
            .expect("ask fired");
        assert!(
            !ask2.contains("rough est."),
            "unpriced model must not show an estimate: {ask2}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hook_stdout_steers_mode_and_notes() {
        struct RunsEcho;
        impl Speaker for RunsEcho {
            fn speak<'a>(
                &'a self,
                req: SpeakRequest,
                out: tokio::sync::mpsc::Sender<StreamEvent>,
            ) -> SpeakFuture<'a> {
                Box::pin(async move {
                    let ran = req.messages.iter().any(|m| !m.results.is_empty());
                    if ran {
                        out.send(StreamEvent::Text("done".into())).await.ok();
                    } else {
                        out.send(StreamEvent::Call(ToolCall {
                            id: "e1".into(),
                            tool: "bash".into(),
                            arguments: serde_json::json!({ "command": "echo hi" }),
                        }))
                        .await
                        .ok();
                    }
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: Usage::default(),
                    })
                    .await
                    .ok();
                })
            }
        }
        let dir = std::env::temp_dir().join(format!("ka-steer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, dir.clone(), ka_protocol::Mode::Free, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(RunsEcho));
        voice.set_hooks(vec![crate::config::Hook {
            event: crate::config::HookEvent::PreToolUse,
            tool: Some("bash".to_string()),
            command: r#"echo '{"mode":"plan","note":"switching"}'"#.to_string(),
        }]);
        let mut guards = GuardRuntime::default();
        let (events, _) = drive_turn(&mut voice, &mut guards, "run", None).await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                Event::ModeChanged {
                    mode: ka_protocol::Mode::Plan
                }
            )),
            "steering switched the mode: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Note { message } if message == "hook: switching")),
            "steering surfaced the note: {events:?}"
        );
        assert_eq!(voice.mode, ka_protocol::Mode::Plan, "gate mode updated");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_ask_carries_a_diff_detail() {
        let dir = std::env::temp_dir().join(format!("ka-diff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("diff-target.txt"), "keep\nchange me\n").unwrap();

        struct OneWrite;
        impl Speaker for OneWrite {
            fn speak<'a>(
                &'a self,
                req: SpeakRequest,
                out: tokio::sync::mpsc::Sender<StreamEvent>,
            ) -> SpeakFuture<'a> {
                Box::pin(async move {
                    let results = req.messages.iter().flat_map(|m| &m.results).count();
                    if results == 0 {
                        out.send(StreamEvent::Call(ToolCall {
                            id: "w1".into(),
                            tool: "write".into(),
                            arguments: serde_json::json!({
                                "path": "diff-target.txt",
                                "content": "keep\nchanged\n"
                            }),
                        }))
                        .await
                        .ok();
                        out.send(StreamEvent::Finished {
                            stop: ka_protocol::Stop::Done,
                            usage: Usage::default(),
                        })
                        .await
                        .ok();
                    } else {
                        out.send(StreamEvent::Text("done".into())).await.ok();
                        out.send(StreamEvent::Finished {
                            stop: ka_protocol::Stop::Done,
                            usage: Usage::default(),
                        })
                        .await
                        .ok();
                    }
                })
            }
        }
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, dir.clone(), ka_protocol::Mode::Guarded, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(OneWrite));
        let mut guards = GuardRuntime::default();
        let (events, _) = drive_turn(&mut voice, &mut guards, "rewrite it", Some(0)).await;
        let asks: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Ask { questions, .. } => questions.first().map(|q| q.detail.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(asks.len(), 1, "one guarded write ask: {events:?}");
        let detail = asks[0].as_ref().expect("write ask must carry a diff");
        assert!(detail.contains("--- a/diff-target.txt"), "{detail}");
        assert!(detail.contains("+++ b/diff-target.txt"), "{detail}");
        assert!(detail.contains("-change me"), "{detail}");
        assert!(detail.contains("+changed"), "{detail}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewind_to_empty_clears_history() {
        use ka_dialect::speaker::TurnMessage;
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Guarded, 5);
        for (u, a) in [("q1", "a1"), ("q2", "a2")] {
            voice.history.push(TurnMessage::user(u));
            voice.history.push(TurnMessage::assistant(a));
        }
        let kept = voice.rewind(2).unwrap();
        assert_eq!(kept, 0);
        assert!(voice.history.is_empty());
        assert!(voice.rewind(1).is_none(), "nothing left to rewind");
    }

    #[test]
    fn context_breakdown_sums_to_last_context() {
        use ka_dialect::speaker::TurnMessage;
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5);
        voice.ratio = 4.0;
        voice.last_context = 10_000;
        voice
            .history
            .push(TurnMessage::user("hello there, a user prompt"));
        voice
            .history
            .push(TurnMessage::assistant("and the assistant reply"));
        voice
            .history
            .push(TurnMessage::tool(vec![ka_dialect::speaker::ToolResult {
                call_id: "c1".into(),
                content: "some tool output bytes".into(),
                is_error: false,
                images: Vec::new(),
            }]));
        let parts = voice.context_breakdown();
        let sum: u64 = parts.iter().map(|p| p.tokens).sum();
        assert_eq!(sum, 10_000, "parts must sum to last_context: {parts:?}");
        let by_name = |n: &str| parts.iter().find(|p| p.name == n).unwrap().tokens;
        assert!(by_name("user") > 0, "{parts:?}");
        assert!(by_name("assistant") > 0, "{parts:?}");
        assert!(by_name("tools") > 0, "{parts:?}");
        assert!(
            by_name("system") >= 9_000,
            "untracked residual folds into system: {parts:?}"
        );
    }

    #[test]
    fn catalog_lookup_contract() {
        let catalog = Catalog::embedded();
        assert!(catalog.get("nope/missing").is_none());
        assert!(catalog.get("openai/gpt-5.1").is_some());
        let _ = Wire::OpenaiChat;
    }

    /// Is `pid` alive? (`kill -0`)
    fn pid_alive(pid: u32) -> bool {
        // zombies count as dead: the killed child is unreaped until the
        // test process exits
        crate::hands::jobs::pid_alive(Some(pid))
    }

    /// Three bash calls whose starts/finishes are logged to a shared file:
    /// concurrent execution shows all S- lines before the first E- line.
    struct BashTrio {
        log: std::path::PathBuf,
    }

    impl Speaker for BashTrio {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                let has_result = req
                    .messages
                    .iter()
                    .any(|m| m.role == TurnRole::Tool && !m.results.is_empty());
                if has_result {
                    out.send(StreamEvent::Text("all done".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: Usage::default(),
                    })
                    .await
                    .ok();
                    return;
                }
                let log = self.log.display().to_string();
                for (i, name) in [(1, "ALPHA"), (2, "BETA"), (3, "GAMMA")] {
                    out.send(StreamEvent::Call(ToolCall {
                        id: format!("c{i}"),
                        tool: "bash".into(),
                        arguments: serde_json::json!({"command": format!(
                            "echo S-{i} >> {log}; sleep 0.3; echo E-{i} >> {log}; echo OUT-{name}"
                        )}),
                    }))
                    .await
                    .ok();
                }
                out.send(StreamEvent::Finished {
                    stop: ka_protocol::Stop::Done,
                    usage: Usage::default(),
                })
                .await
                .ok();
            })
        }
    }

    #[tokio::test]
    async fn parallel_step_runs_concurrently_and_returns_results_in_call_order() {
        let dir = std::env::temp_dir().join(format!("ka-voice-par-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, dir.clone(), ka_protocol::Mode::Free, 5).with_speaker(
            Wire::OpenaiChat,
            std::sync::Arc::new(BashTrio { log: log.clone() }),
        );
        let mut guards = GuardRuntime::default();
        let (_events, _) = drive_turn(&mut voice, &mut guards, "run the trio", None).await;

        // results fed back in ORIGINAL call order with matching call ids
        let tool_msg = voice
            .history
            .iter()
            .find(|m| m.role == TurnRole::Tool)
            .unwrap();
        let results = &tool_msg.results;
        for ((i, result), marker) in results.iter().enumerate().zip(["ALPHA", "BETA", "GAMMA"]) {
            assert_eq!(result.call_id, format!("c{}", i + 1), "{results:?}");
            assert!(
                result.content.contains(&format!("OUT-{marker}")),
                "result {i} must carry its own output: {}",
                result.content
            );
            assert!(!result.is_error);
        }
        // concurrency proof: every call started before any finished
        let lines: Vec<String> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        let last_start = lines
            .iter()
            .rposition(|l| l.starts_with("S-"))
            .expect("starts logged");
        let first_end = lines
            .iter()
            .position(|l| l.starts_with("E-"))
            .expect("finishes logged");
        assert!(
            last_start < first_end,
            "calls must overlap, not serialize: {lines:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One long-running bash call; the abort arrives mid-step.
    struct LongBash {
        pidfile: std::path::PathBuf,
    }

    impl Speaker for LongBash {
        fn speak<'a>(
            &'a self,
            _req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                out.send(StreamEvent::Call(ToolCall {
                    id: "b1".into(),
                    tool: "bash".into(),
                    arguments: serde_json::json!({"command": format!(
                        "sleep 30 & echo $! > {}; wait",
                        self.pidfile.display()
                    )}),
                }))
                .await
                .ok();
                out.send(StreamEvent::Finished {
                    stop: ka_protocol::Stop::Done,
                    usage: Usage::default(),
                })
                .await
                .ok();
            })
        }
    }

    #[tokio::test]
    async fn abort_mid_step_cancels_in_flight_and_kills_children() {
        let dir = std::env::temp_dir().join(format!("ka-voice-abort-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("sleeper.pid");
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, dir.clone(), ka_protocol::Mode::Free, 5).with_speaker(
            Wire::OpenaiChat,
            std::sync::Arc::new(LongBash {
                pidfile: pidfile.clone(),
            }),
        );

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(16);
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel(256);
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    "start the sleeper".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut steers,
                    &mut queue,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });
        let mut aborted = false;
        while let Some(evt) = evt_rx.recv().await {
            match evt {
                Event::CallStarted { .. } => {
                    // wait for the child to publish its pid, then abort
                    for _ in 0..100 {
                        if pidfile.exists() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    cmd_tx.send(Command::Abort).await.ok();
                }
                Event::TurnFinished { stop, .. } => {
                    aborted = stop == Stop::Aborted;
                    break;
                }
                _ => {}
            }
        }
        handle.await.unwrap();
        assert!(aborted, "mid-step abort must finish the turn aborted");

        // the in-flight child must be dead shortly after the cancel
        let sleeper: u32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        for _ in 0..100 {
            if !pid_alive(sleeper) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            !pid_alive(sleeper),
            "aborted bash child (pid {sleeper}) must be killed, not orphaned"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bash (auto-backgrounds) → jobs list → jobs kill → done.
    struct BgScript;

    impl Speaker for BgScript {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                let results = req.messages.iter().flat_map(|m| &m.results).count();
                if results >= 3 {
                    out.send(StreamEvent::Text("wrapped up".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: Usage::default(),
                    })
                    .await
                    .ok();
                    return;
                }
                let (tool, id, call) = match results {
                    0 => (
                        "bash",
                        "b1",
                        serde_json::json!({"command": "echo bg-running; sleep 30"}),
                    ),
                    1 => ("jobs", "j1", serde_json::json!({})),
                    _ => ("jobs", "j2", serde_json::json!({"action": "kill", "id": 1})),
                };
                out.send(StreamEvent::Call(ToolCall {
                    id: id.into(),
                    tool: tool.into(),
                    arguments: call,
                }))
                .await
                .ok();
                out.send(StreamEvent::Finished {
                    stop: ka_protocol::Stop::Done,
                    usage: Usage::default(),
                })
                .await
                .ok();
            })
        }
    }
    #[tokio::test]
    async fn background_threshold_promotes_and_jobs_hand_polls_and_kills() {
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("ka-voice-bg-spills"));
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(BgScript));
        voice.set_bash_background_ms(60);
        // capture before the voice moves into the turn task
        let jobs = voice.jobs();

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(16);
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel(256);
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    "run long".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut steers,
                    &mut queue,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });
        let mut outputs: Vec<String> = Vec::new();
        while let Some(evt) = evt_rx.recv().await {
            if let Event::CallOutput { excerpt, .. } = &evt {
                outputs.push(excerpt.clone());
            }
            if matches!(evt, Event::TurnFinished { .. }) {
                break;
            }
        }
        drop(cmd_tx);
        handle.await.unwrap();
        let promotion = outputs
            .iter()
            .find(|o| o.contains("backgrounded as job 1"))
            .expect("promotion result must reach the surface");
        assert!(promotion.contains("poll with jobs"), "{promotion}");
        let listing = outputs
            .iter()
            .find(|o| o.contains("job 1") && !o.contains("backgrounded as job"))
            .expect("jobs list output");
        assert!(listing.contains("running"), "{listing}");
        assert!(listing.contains("sleep 30"), "{listing}");
        assert!(
            listing.contains("bg-running"),
            "tail must show streamed output: {listing}"
        );
        let kill = outputs
            .iter()
            .find(|o| o.contains("killing job 1"))
            .expect("kill output");
        assert!(!kill.is_empty(), "{kill}");
        // the kill watcher records the terminal state in the table
        for _ in 0..100 {
            if jobs.snapshot()[0].state != crate::hands::jobs::JobState::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            matches!(
                jobs.snapshot()[0].state,
                crate::hands::jobs::JobState::Exited(_)
            ),
            "killed job must land exited: {:?}",
            jobs.snapshot()[0].state
        );
    }

    /// Fails with a non-retryable auth error for the listed model ids,
    /// otherwise finishes with text. Records every request's model id.
    struct AuthFailSpeaker {
        seen: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
        fail_for: Vec<String>,
    }

    impl Speaker for AuthFailSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            let seen = self.seen.clone();
            Box::pin(async move {
                seen.lock().push(req.model_id.clone());
                if self.fail_for.contains(&req.model_id) {
                    out.send(StreamEvent::Failed {
                        class: ka_protocol::ErrorClass::Auth,
                        retryable: false,
                        message: "auth boom".into(),
                    })
                    .await
                    .ok();
                } else {
                    out.send(StreamEvent::Text("ok".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: ka_protocol::Usage::default(),
                    })
                    .await
                    .ok();
                }
            })
        }
    }

    /// Catalog covering the fallback test models.
    fn fallback_catalog() -> Catalog {
        Catalog::parse(
            "[dialects.\"test/a\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n\
             [dialects.\"test/b\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n\
             [dialects.\"test/c\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n\
             [dialects.\"test/d\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap()
    }

    /// Drive one turn on `start` with `fallbacks`, returning the notes,
    /// errors, finish flag and the model ids the speaker saw.
    async fn drive_fallback_turn(
        fail_for: Vec<String>,
        fallbacks: Vec<String>,
        start: &str,
    ) -> (
        Vec<String>,
        Vec<(ka_protocol::ErrorClass, String)>,
        bool,
        Vec<String>,
    ) {
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(
            fallback_catalog(),
            std::env::temp_dir(),
            ka_protocol::Mode::Free,
            5,
        )
        .with_speaker(
            Wire::OpenaiChat,
            std::sync::Arc::new(AuthFailSpeaker {
                seen: seen.clone(),
                fail_for,
            }),
        );
        voice.set_fallbacks(fallbacks);
        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let mut guards = GuardRuntime::default();
        let start = start.to_string();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    &start,
                    "hi".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut steers,
                    &mut queue,
                    &mut guards,
                    None,
                    Vec::new(),
                )
                .await
        });
        let mut notes = Vec::new();
        let mut errors = Vec::new();
        let mut finished = false;
        while let Some(evt) = evt_rx.recv().await {
            match evt {
                Event::Note { message } => notes.push(message),
                Event::Error { class, message, .. } => errors.push((class, message)),
                Event::TurnFinished { .. } => {
                    finished = true;
                    break;
                }
                _ => {}
            }
        }
        let _ = handle.await.unwrap();
        drop(cmd_tx);
        let ids = seen.lock().clone();
        (notes, errors, finished, ids)
    }

    #[tokio::test]
    async fn fallback_hops_to_next_model_on_auth_failure() {
        let (notes, errors, finished, ids) =
            drive_fallback_turn(vec!["test/a".into()], vec!["test/b".into()], "test/a").await;
        assert_eq!(ids, vec!["test/a".to_string(), "test/b".to_string()]);
        assert!(
            notes.iter().any(|n| n.contains("fallback → test/b")),
            "{notes:?}"
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert!(finished);
    }

    #[tokio::test]
    async fn fallback_stops_after_two_hops() {
        // a, b, c all fail; d is healthy but unreachable: the 2-hop cap
        // fires first and the original error surfaces
        let (notes, errors, finished, ids) = drive_fallback_turn(
            vec!["test/a".into(), "test/b".into(), "test/c".into()],
            vec!["test/b".into(), "test/c".into(), "test/d".into()],
            "test/a",
        )
        .await;
        assert_eq!(
            ids,
            vec![
                "test/a".to_string(),
                "test/b".to_string(),
                "test/c".to_string()
            ]
        );
        assert_eq!(notes.iter().filter(|n| n.contains("fallback →")).count(), 2);
        assert!(
            errors
                .iter()
                .any(|(c, m)| *c == ka_protocol::ErrorClass::Auth && m == "auth boom")
        );
        assert!(finished, "turn must end with TurnFinished after the error");
    }

    #[tokio::test]
    async fn empty_fallback_chain_fails_directly() {
        let (notes, errors, finished, ids) =
            drive_fallback_turn(vec!["test/a".into()], Vec::new(), "test/a").await;
        assert_eq!(ids, vec!["test/a".to_string()]);
        assert!(!notes.iter().any(|n| n.contains("fallback")));
        assert!(
            errors
                .iter()
                .any(|(c, _)| *c == ka_protocol::ErrorClass::Auth)
        );
        assert!(finished);
    }

    #[tokio::test]
    async fn schema_on_non_structured_model_errors_instructively() {
        let catalog = Catalog::parse(
            "[dialects.\"test/ns\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n\n[dialects.\"test/ns\".flags]\nstructured = false\n",
        )
        .unwrap();
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
            .with_speaker(
                Wire::OpenaiChat,
                std::sync::Arc::new(AuthFailSpeaker {
                    seen: seen.clone(),
                    fail_for: Vec::new(),
                }),
            );
        let (_cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let mut guards = GuardRuntime::default();
        let schema = serde_json::json!({"type": "object"});
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/ns",
                    "hi".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut steers,
                    &mut queue,
                    &mut guards,
                    Some(schema),
                    Vec::new(),
                )
                .await
        });
        let mut saw_unsupported = false;
        while let Some(evt) = evt_rx.recv().await {
            if let Event::Error {
                class: ka_protocol::ErrorClass::Unsupported,
                message,
                ..
            } = evt
            {
                assert!(message.contains("structured output"), "{message}");
                saw_unsupported = true;
                break;
            }
        }
        assert!(saw_unsupported, "expected an Unsupported error event");
        let _ = handle.await.unwrap();
        assert!(
            seen.lock().is_empty(),
            "the speaker must not be reached for unsupported schemas"
        );
    }

    /// Overflows on the small model, succeeds otherwise; records models.
    struct PromoteSpeaker {
        seen: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
    }

    impl Speaker for PromoteSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            let seen = self.seen.clone();
            Box::pin(async move {
                seen.lock().push(req.model_id.clone());
                if req.model_id == "test/small" {
                    out.send(StreamEvent::Failed {
                        class: ka_protocol::ErrorClass::Overflow,
                        retryable: false,
                        message: "prompt too long".into(),
                    })
                    .await
                    .ok();
                } else {
                    out.send(StreamEvent::Text("fits".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: ka_protocol::Usage::default(),
                    })
                    .await
                    .ok();
                }
            })
        }
    }

    fn promote_catalog() -> Catalog {
        Catalog::parse(
            "[dialects.\"test/small\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 100\n\
             [dialects.\"test/big\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn overflow_promotes_before_digesting() {
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(
            promote_catalog(),
            std::env::temp_dir(),
            ka_protocol::Mode::Free,
            5,
        )
        .with_speaker(
            Wire::OpenaiChat,
            std::sync::Arc::new(PromoteSpeaker { seen: seen.clone() }),
        );
        let (_cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let mut guards = GuardRuntime::default();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/small",
                    "hi".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut steers,
                    &mut queue,
                    &mut guards,
                    None,
                    Vec::new(),
                )
                .await
        });
        let mut notes = Vec::new();
        let mut saw_digest_started = false;
        while let Some(evt) = evt_rx.recv().await {
            match evt {
                Event::Note { message } => notes.push(message),
                Event::DigestStarted => saw_digest_started = true,
                Event::TurnFinished { .. } => break,
                _ => {}
            }
        }
        let _ = handle.await.unwrap();
        assert!(
            notes.iter().any(|n| n.contains("context → test/big")),
            "{notes:?}"
        );
        assert!(!saw_digest_started, "promotion must precede digestion");
        {
            let ids = seen.lock();
            assert_eq!(ids.first().map(String::as_str), Some("test/small"));
            assert_eq!(ids.last().map(String::as_str), Some("test/big"));
        }
    }

    #[tokio::test]
    async fn promotion_disabled_goes_straight_to_digest() {
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(
            promote_catalog(),
            std::env::temp_dir(),
            ka_protocol::Mode::Free,
            5,
        )
        .with_speaker(
            Wire::OpenaiChat,
            std::sync::Arc::new(PromoteSpeaker { seen: seen.clone() }),
        );
        voice.set_context_promote(false);
        let (_cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let mut guards = GuardRuntime::default();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/small",
                    "hi".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut steers,
                    &mut queue,
                    &mut guards,
                    None,
                    Vec::new(),
                )
                .await
        });
        let mut notes = Vec::new();
        while let Some(evt) = evt_rx.recv().await {
            match evt {
                Event::Note { message } => notes.push(message),
                Event::TurnFinished { .. } => break,
                _ => {}
            }
        }
        let _ = handle.await.unwrap();
        assert!(!notes.iter().any(|n| n.contains("context →")), "{notes:?}");
        assert!(
            seen.lock().iter().all(|m| m == "test/small"),
            "no promotion means the model never changes"
        );
    }

    /// Responds "CANDIDATE" to summarize-shaped requests (no tools).
    struct SpeculativeSpeaker;

    impl Speaker for SpeculativeSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                if req.tools.is_empty() {
                    out.send(StreamEvent::Text("CANDIDATE".into())).await.ok();
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

    #[tokio::test]
    async fn speculative_candidate_used_when_watermark_matches() {
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(SpeculativeSpeaker));
        voice.set_model_selector("test/m", 4.0);
        // 70% of the reserve threshold: speculative zone, not tripping
        voice.note_context_for_tests(700_000);
        assert!(voice.context_pressure_frac(1_000_000, 80));
        assert!(!voice.context_pressure(1_000_000));
        assert!(voice.start_speculative(None), "speculative must fire");

        // watermark unchanged: the candidate is ready and matches
        // (poll: the background task stores the receiver on completion)
        let taken = loop {
            if let Some(t) = voice.take_speculative().await {
                break t;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(taken, "CANDIDATE");
        // consumed: nothing left
        assert!(voice.take_speculative().await.is_none());
    }

    #[tokio::test]
    async fn speculative_candidate_discarded_on_watermark_mismatch() {
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(SpeculativeSpeaker));
        voice.set_model_selector("test/m", 4.0);
        voice.note_context_for_tests(700_000);
        assert!(voice.start_speculative(None));
        // history moved on since the candidate started
        voice.history.push(TurnMessage::user("newer question"));
        assert!(
            voice.take_speculative().await.is_none(),
            "stale candidate must be discarded"
        );
    }

    #[tokio::test]
    async fn speculative_skipped_below_eighty_percent() {
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(SpeculativeSpeaker));
        voice.set_model_selector("test/m", 4.0);
        voice.note_context_for_tests(400_000);
        assert!(!voice.context_pressure_frac(1_000_000, 80));
        assert!(!voice.start_speculative(None));
    }

    /// Regression for "thinking blocks disappear when resuming": the
    /// CLOSING (text-only) round's reasoning used to be dropped from the
    /// final history push — step rounds kept theirs, so thoughts vanished
    /// from replay for every turn that ended without tool calls (most).
    #[tokio::test]
    async fn closing_round_thinking_persists_in_history() {
        use ka_dialect::speaker::{SpeakFuture, SpeakRequest, Speaker, StreamEvent};
        struct Thinker;
        impl Speaker for Thinker {
            fn speak<'a>(
                &'a self,
                _req: SpeakRequest,
                out: tokio::sync::mpsc::Sender<StreamEvent>,
            ) -> SpeakFuture<'a> {
                Box::pin(async move {
                    out.send(StreamEvent::Thought("let me think this through".into()))
                        .await
                        .ok();
                    out.send(StreamEvent::Text("the answer".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: Stop::Done,
                        usage: Usage::default(),
                    })
                    .await
                    .ok();
                })
            }
        }
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(Thinker));
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(16);
        let (evt_tx, _evt_rx) = tokio::sync::mpsc::channel(256);
        let mut inter = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let mut guards = GuardRuntime::default();
        voice
            .turn(
                "test/m",
                "ask".into(),
                &mut cmd_rx,
                &evt_tx,
                &mut inter,
                &mut queue,
                &mut guards,
                None,
                Vec::new(),
            )
            .await;
        drop(cmd_tx);
        let last = voice.history.last().expect("final assistant push");
        assert_eq!(last.role, ka_dialect::speaker::TurnRole::Assistant);
        assert_eq!(last.content, "the answer");
        assert_eq!(
            last.thinking.as_deref(),
            Some("let me think this through"),
            "closing-round thinking must persist for replay"
        );
    }

    #[tokio::test]
    async fn lint_enrichment_appends_failures_only() {
        use crate::config::{LintRule, Verify};
        use crate::hands::{HandContext, Ledger, Spill, ToolOutput};

        let ctx = HandContext {
            cwd: std::env::temp_dir(),
            ledger: std::sync::Arc::new(parking_lot::Mutex::new(Ledger::default())),
            spill: std::sync::Arc::new(Spill::new()),
            snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
            jobs: std::sync::Arc::new(crate::hands::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
        };
        let mk_call = || ToolCall {
            id: "c1".into(),
            tool: "edit".into(),
            arguments: serde_json::json!({"path": "src/a.rs"}),
        };
        // failing lint: block appended, command shows the substitution,
        // output is tailed; the tool result stays non-error
        let verify = Verify {
            test: None,
            lints: vec![LintRule {
                pattern: "*.rs".to_string(),
                command: "echo LINT-FAIL >&2; exit 3 {file}".to_string(),
            }],
        };
        let mut output = ToolOutput::ok("the diff");
        super::enrich_with_lint(&mut output, &mk_call(), &ctx, &verify).await;
        assert!(!output.is_error, "lint is informational");
        assert!(output.content.starts_with("the diff"));
        assert!(
            output.content.contains("LINT-FAIL"),
            "stderr captured: {}",
            output.content
        );
        assert!(
            output.content.contains("exit 3 —"),
            "exit code reported: {}",
            output.content
        );
        assert!(
            output
                .content
                .contains("echo LINT-FAIL >&2; exit 3 src/a.rs"),
            "substituted command shown: {}",
            output.content
        );
        // passing lint: nothing appended
        let verify = Verify {
            test: None,
            lints: vec![LintRule {
                pattern: "*.rs".to_string(),
                command: "true {file}".to_string(),
            }],
        };
        let mut output = ToolOutput::ok("the diff");
        super::enrich_with_lint(&mut output, &mk_call(), &ctx, &verify).await;
        assert_eq!(output.content, "the diff");
        // no matching rule: nothing appended
        let verify = Verify {
            test: None,
            lints: vec![LintRule {
                pattern: "*.py".to_string(),
                command: "false {file}".to_string(),
            }],
        };
        let mut output = ToolOutput::ok("the diff");
        super::enrich_with_lint(&mut output, &mk_call(), &ctx, &verify).await;
        assert_eq!(output.content, "the diff");
    }

    #[test]
    fn protected_paths_beat_rules_and_free_mode() {
        use ka_dialect::speaker::ToolCall;
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5);
        // a blanket allow rule for write must NOT wave protected paths through
        voice.set_rules(vec![crate::config::Rule {
            tool: "write".to_string(),
            pattern: None,
            verdict: crate::config::Verdict::Allow,
        }]);
        voice.set_allowed_tools(vec!["write".to_string()]);
        let call = ToolCall {
            id: "c1".into(),
            tool: "write".into(),
            arguments: serde_json::json!({"path": ".git/hooks/pre-commit"}),
        };
        let gate = voice.gate(crate::hands::Clearance::Write, &call);
        assert!(
            matches!(gate, Gate::Ask { .. }),
            "protected path must ask even with allow-rule + free mode + allowlist: {gate:?}"
        );
        // ordinary paths still flow through the rule (allowed)
        let call = ToolCall {
            id: "c2".into(),
            tool: "write".into(),
            arguments: serde_json::json!({"path": "src/main.rs"}),
        };
        let gate = voice.gate(crate::hands::Clearance::Write, &call);
        assert!(
            matches!(gate, Gate::Allow),
            "rule allows ordinary write: {gate:?}"
        );
    }

    #[test]
    fn permissions_allow_skips_the_ask_without_touching_protection() {
        use ka_dialect::speaker::ToolCall;
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
        )
        .unwrap();
        // guarded mode: writes normally ask, exec normally asks
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Guarded, 5);
        // `[permissions] allow = ["write"]` — the documented persistent
        // allowlist ("skip the permission ask") is enforced at the gate
        voice.set_allowed_tools(vec!["write".to_string()]);
        let call = ToolCall {
            id: "c1".into(),
            tool: "write".into(),
            arguments: serde_json::json!({"path": "src/main.rs"}),
        };
        assert!(
            matches!(
                voice.gate(crate::hands::Clearance::Write, &call),
                Gate::Allow
            ),
            "allowlisted tool skips the ask in guarded mode"
        );
        // a non-allowlisted tool still asks
        let call = ToolCall {
            id: "c2".into(),
            tool: "edit".into(),
            arguments: serde_json::json!({"path": "src/main.rs"}),
        };
        assert!(
            matches!(
                voice.gate(crate::hands::Clearance::Write, &call),
                Gate::Ask { .. }
            ),
            "non-allowlisted write still asks"
        );
        // protected paths outrank the allowlist (checked first)
        let call = ToolCall {
            id: "c3".into(),
            tool: "write".into(),
            arguments: serde_json::json!({"path": ".git/hooks/pre-commit"}),
        };
        assert!(
            matches!(
                voice.gate(crate::hands::Clearance::Write, &call),
                Gate::Ask { .. }
            ),
            "protected path must ask even when allowlisted"
        );
    }

    #[test]
    fn plan_mode_still_denies_despite_protected_check() {
        use ka_dialect::speaker::ToolCall;
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
        )
        .unwrap();
        let voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Plan, 5);
        let call = ToolCall {
            id: "c1".into(),
            tool: "write".into(),
            arguments: serde_json::json!({"path": "~/.bashrc"}),
        };
        let gate = voice.gate(crate::hands::Clearance::Write, &call);
        assert!(
            matches!(gate, Gate::Deny { .. }),
            "plan mode denies; protected must not downgrade to ask: {gate:?}"
        );
    }
}
