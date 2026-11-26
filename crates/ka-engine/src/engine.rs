//! The engine: a turn machine over two queues. Surfaces send [`Command`]s
//! and consume [`Event`]s; the engine owns all sequencing.
//!
//! Two turn paths: the **canned** speaker (no model configured — keeps
//! `ka run` working keyless) and the **live voice** (real wires via
//! ka-dialect). Both honor steers, queued input, and aborts through the
//! same `select!` seam.

use std::collections::VecDeque;
use std::time::Duration;

use ka_protocol::{
    AskId, AskQuestion, Command, ContextMeter, DeltaKind, ErrorClass, Event, GoalAction,
    LoopAction, McpSummary, Mode, Stop, Usage,
};
use tokio::sync::mpsc;

use crate::canned;
use crate::config::Config;
use crate::fshooks::HookPoint;
use crate::voice::Voice;

/// Handle returned by [`spawn`]: the surface's two queue ends.
pub struct EngineHandle {
    /// Send commands to the engine.
    pub commands: mpsc::Sender<Command>,
    /// Consume events from the engine.
    pub events: mpsc::Receiver<Event>,
}

/// Which strand the engine should attach to.
#[derive(Debug, Clone)]
pub enum StrandChoice {
    /// Create a fresh strand.
    New,
    /// Continue the newest strand for the cwd (create if none).
    Latest,
    /// Open a specific strand file.
    Path(std::path::PathBuf),
}

/// Role selectors resolved against the engine's catalog at startup.
/// Roles are separate from the interactive default model (`/model`
/// keeps touching only `state.model`).
#[derive(Debug, Default, Clone)]
struct EngineRoles {
    /// Main-line selector (`[roles] default`, falling back to the
    /// configured model). Resolved (validated against the catalog) at
    /// startup alongside `fast`; the first non-title role consumers
    /// will read it.
    #[allow(dead_code)]
    default: Option<String>,
    /// Cheap/fast selector (`[roles] fast`); `None` degrades every
    /// fast-role consumer (auto-titles today) to its fallback.
    fast: Option<String>,
    /// GIT-03: `[git] commit_role` resolved against the catalog at
    /// startup (`None` = unset, or set but unresolvable — auto-commit
    /// keeps the deterministic first-prompt-line subject).
    commit: Option<String>,
}

/// Resolve one role selector through the same catalog path as the main
/// model: it must parse, and its `vendor/model` must exist in the
/// catalog (locals arrive via the discovery overlay before the engine
/// starts). Unresolvable → `None`.
fn resolve_role(catalog: &ka_dialect::Catalog, selector: &str) -> Option<String> {
    let id = ka_dialect::parse_selector(selector).ok()?.model_id();
    catalog.get(&id).is_some().then_some(id)
}

/// EXT-11: `@<role>` selectors resolve through the [roles] table
/// (`@default`, `@fast`, or any `[roles.extra.<name>]`); anything else
/// — including unknown `@names` and the `virtual/<name>` router
/// selectors, which the voice routes per turn — stays literal.
fn expand_role_selector(roles: &crate::config::Roles, selector: &str) -> String {
    roles
        .resolve(selector)
        .map(str::to_string)
        .unwrap_or_else(|| selector.to_string())
}

/// GIT-03: `[git] commit_role` — a `[roles]` name resolves through the
/// table (bare `fast`/`default`/extra-name, or `@`-prefixed); anything
/// else is a literal selector (`vendor/model@effort`). Unknown names
/// stay literal and fail catalog resolution downstream — the silent
/// first-prompt-line fallback.
fn commit_role_selector(roles: &crate::config::Roles, raw: &str) -> String {
    roles
        .resolve(&format!("@{raw}"))
        .or_else(|| roles.resolve(raw))
        .map(str::to_string)
        .unwrap_or_else(|| raw.to_string())
}

/// Resolve the `[roles]` table against the catalog. `default` falls
/// back to the configured model; a missing/invalid `fast` simply stays
/// `None` (fast-role-gated features degrade gracefully).
fn resolve_roles(
    catalog: &ka_dialect::Catalog,
    roles: &crate::config::Roles,
    fallback_default: Option<&str>,
) -> EngineRoles {
    EngineRoles {
        default: roles
            .default
            .as_deref()
            .or(fallback_default)
            .and_then(|s| resolve_role(catalog, s)),
        fast: roles.fast.as_deref().and_then(|s| resolve_role(catalog, s)),
        // GIT-03: `[git] commit_role` resolves separately in `run`
        // (it is git config, not a [roles] row) and is assigned onto
        // the state after this constructor returns.
        commit: None,
    }
}

/// Spawn the engine with the embedded catalog. Must be called inside a
/// tokio runtime (the CLI provides one).
pub fn spawn(config: Config) -> EngineHandle {
    spawn_full(config, ka_dialect::Catalog::embedded(), StrandChoice::New)
}

/// Spawn the engine over an explicit catalog (embedded + overlays +
/// discovery, assembled by the caller), attaching to a fresh strand.
pub fn spawn_with(config: Config, catalog: ka_dialect::Catalog) -> EngineHandle {
    spawn_full(config, catalog, StrandChoice::New)
}

/// Spawn with full control: catalog + strand choice.
pub fn spawn_full(
    config: Config,
    catalog: ka_dialect::Catalog,
    strand: StrandChoice,
) -> EngineHandle {
    spawn_speaked(config, catalog, strand, None, None)
}

/// Spawn with a scripted speaker for one wire (contract tests): the
/// real turn machine runs against scripted model responses — no
/// network, no provider. Everything else (gate, tools, verify loop,
/// hooks) behaves exactly as in production.
pub fn spawn_with_speaker(
    config: Config,
    catalog: ka_dialect::Catalog,
    strand: StrandChoice,
    wire: ka_dialect::Wire,
    speaker: std::sync::Arc<dyn ka_dialect::speaker::Speaker>,
) -> EngineHandle {
    spawn_speaked(config, catalog, strand, Some((wire, speaker)), None)
}

/// Contract-test spawn (EXT-12): [`spawn_with_speaker`] plus a factory
/// the team table uses to give every spawned member its own scripted
/// speaker. Production passes None — members inherit the session model
/// through the parent config. Test-only (`team_contracts.rs` is the
/// sole consumer), gated like `trust::test_support`.
#[cfg(any(test, feature = "test-util"))]
pub fn spawn_with_team_speakers(
    config: Config,
    catalog: ka_dialect::Catalog,
    strand: StrandChoice,
    wire: ka_dialect::Wire,
    speaker: std::sync::Arc<dyn ka_dialect::speaker::Speaker>,
    member_speakers: crate::hands::teams::MemberSpeakers,
) -> EngineHandle {
    spawn_speaked(
        config,
        catalog,
        strand,
        Some((wire, speaker)),
        Some(member_speakers),
    )
}

fn spawn_speaked(
    config: Config,
    catalog: ka_dialect::Catalog,
    strand: StrandChoice,
    speaker: Option<(
        ka_dialect::Wire,
        std::sync::Arc<dyn ka_dialect::speaker::Speaker>,
    )>,
    team_speakers: Option<crate::hands::teams::MemberSpeakers>,
) -> EngineHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (evt_tx, evt_rx) = mpsc::channel(256);
    tokio::spawn(async move {
        if let Err(e) = run(
            cmd_rx,
            evt_tx,
            config,
            catalog,
            strand,
            speaker,
            false,
            team_speakers,
        )
        .await
        {
            // The events channel is gone or the engine hit an unrecoverable
            // state; surface-level diagnostics only.
            eprintln!("ka engine ended: {e}");
        }
    });
    EngineHandle {
        commands: cmd_tx,
        events: evt_rx,
    }
}

/// Spawn one EXT-12 team member: a full engine session on a fresh
/// strand (the parent's cwd) with the delegate/teams hands stripped —
/// a leaf: no nested spawns, and bootstrap skips MCP servers, the
/// session lifecycle hooks, and the jobs-file adoption. Returns the
/// surface handle plus the engine task so the team table can cancel it
/// (dismiss, parent drop). Test fixtures script the member's speaker.
pub(crate) fn spawn_leaf(
    config: Config,
    catalog: ka_dialect::Catalog,
    speaker: Option<(
        ka_dialect::Wire,
        std::sync::Arc<dyn ka_dialect::speaker::Speaker>,
    )>,
) -> (EngineHandle, tokio::task::JoinHandle<()>) {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (evt_tx, evt_rx) = mpsc::channel(256);
    let task = tokio::spawn(async move {
        if let Err(e) = run(
            cmd_rx,
            evt_tx,
            config,
            catalog,
            StrandChoice::New,
            speaker,
            true,
            None,
        )
        .await
        {
            eprintln!("ka team member ended: {e}");
        }
    });
    (
        EngineHandle {
            commands: cmd_tx,
            events: evt_rx,
        },
        task,
    )
}

/// Resolve a strand choice into an open strand (creating when allowed),
/// applying the session's `[history]` persistence policy.
fn resolve_strand(
    choice: &StrandChoice,
    cwd: &std::path::Path,
    history: &crate::config::History,
) -> std::io::Result<ka_strand::StrandFile> {
    let mut strand = match choice {
        StrandChoice::New => {
            let repo = repo_snapshot(cwd);
            ka_strand::StrandFile::create(cwd, repo)?
        }
        StrandChoice::Latest => match ka_strand::latest(cwd)? {
            Some(summary) => ka_strand::StrandFile::open(&summary.path)?,
            None => {
                let repo = repo_snapshot(cwd);
                ka_strand::StrandFile::create(cwd, repo)?
            }
        },
        StrandChoice::Path(path) => ka_strand::StrandFile::open(path)?,
    };
    strand.set_persist(history.persistence != Some(crate::config::HistoryPersistence::None));
    strand.set_max_bytes(history.max_bytes);
    Ok(strand)
}

/// Capture the repo snapshot in ka-strand's shape.
fn repo_snapshot(cwd: &std::path::Path) -> Option<ka_strand::RepoSnapshot> {
    let snap = crate::hands::git::RepoSnapshot::capture(cwd);
    (!snap.branch.is_empty()).then_some(ka_strand::RepoSnapshot {
        branch: snap.branch,
        dirty: snap.dirty,
    })
}

/// Convert persisted records into conversation history + active digest.
/// Digest records reset the accumulated history (the summary carries it).
/// Stored image parts re-attach only when `vision` — the active dialect
/// takes image input; replaying them onto a text-only model would
/// hard-error every request at the wire's vision gate, so they drop (the
/// placeholder text inside the persisted tool result still stands).
fn history_from_records(
    records: &[ka_strand::Record],
    vision: bool,
) -> (
    Vec<ka_dialect::speaker::TurnMessage>,
    Vec<ka_protocol::RecordId>,
    Option<String>,
) {
    use ka_dialect::speaker::{ToolCall, ToolResult, TurnMessage, TurnRole};
    let mut out: Vec<TurnMessage> = Vec::new();
    let mut ids: Vec<ka_protocol::RecordId> = Vec::new();
    let mut digest: Option<String> = None;
    for r in records {
        match r {
            ka_strand::Record::Message {
                role,
                content,
                calls,
                results,
                thinking,
                images,
                ..
            } => {
                let turn_role = match role {
                    ka_strand::Role::User => TurnRole::User,
                    ka_strand::Role::Assistant | ka_strand::Role::System => TurnRole::Assistant,
                    ka_strand::Role::Tool => TurnRole::Tool,
                };
                if let Some(id) = r.id() {
                    ids.push(id.clone());
                }
                out.push(TurnMessage {
                    role: turn_role,
                    content: content.clone(),
                    thinking: thinking.clone(),
                    calls: calls
                        .iter()
                        .map(|c| ToolCall {
                            id: c.id.clone(),
                            tool: c.tool.clone(),
                            arguments: c.arguments.clone(),
                        })
                        .collect(),
                    results: results
                        .iter()
                        .map(|r| ToolResult {
                            call_id: r.call_id.clone(),
                            content: r.content.clone(),
                            is_error: r.is_error,
                            images: if vision { r.images.clone() } else { Vec::new() },
                        })
                        .collect(),
                    images: if vision { images.clone() } else { Vec::new() },
                });
            }
            ka_strand::Record::Digest { summary, .. } => {
                // everything before the digest is carried by its summary
                out.clear();
                ids.clear();
                digest = Some(summary.clone());
            }
            ka_strand::Record::Boundary { .. } => {
                out.clear();
                ids.clear();
                digest = None;
            }
            ka_strand::Record::Rewind { kept_from, .. } => {
                // drop messages after the kept point (the tail of the log
                // is the abandoned branch)
                match ids.iter().position(|i| i == kept_from) {
                    Some(pos) => {
                        out.truncate(pos + 1);
                        ids.truncate(pos + 1);
                    }
                    // unknown anchor (e.g. behind a digest that cleared
                    // ids): drop everything replayable — never silently
                    // keep the abandoned tail logically live
                    None => {
                        out.clear();
                        ids.clear();
                    }
                }
            }
            ka_strand::Record::ContextEdit {
                target,
                new_content,
                ..
            } => {
                // SES-09 replay: the same resolution rule every fold
                // shares (ka_strand::resolve_context_edit — last match
                // scanning backwards). `Some(text)` rewrites the
                // message's content from here on; `None` removes it
                // from the window. An out-of-window target is silently
                // ignored (already retracted or unknown).
                if let Some(pos) = ka_strand::resolve_context_edit(&ids, target) {
                    match new_content {
                        Some(text) => out[pos].content = text.clone(),
                        None => {
                            out.remove(pos);
                            ids.remove(pos);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    (out, ids, digest)
}

/// Convert one neutral history message into a persistable record.
fn record_from_message(msg: &ka_dialect::speaker::TurnMessage) -> ka_strand::Record {
    use ka_dialect::speaker::TurnRole;
    let role = match msg.role {
        TurnRole::User => ka_strand::Role::User,
        TurnRole::Assistant => ka_strand::Role::Assistant,
        TurnRole::Tool => ka_strand::Role::Tool,
    };
    ka_strand::Record::Message {
        id: ka_strand::new_record_id(),
        role,
        content: msg.content.clone(),
        thinking: msg.thinking.clone(),
        images: msg.images.clone(),
        calls: msg
            .calls
            .iter()
            .map(|c| ka_strand::StoredCall {
                id: c.id.clone(),
                tool: c.tool.clone(),
                arguments: c.arguments.clone(),
            })
            .collect(),
        results: msg
            .results
            .iter()
            .map(|r| ka_strand::StoredResult {
                call_id: r.call_id.clone(),
                content: r.content.clone(),
                is_error: r.is_error,
                images: r.images.clone(),
            })
            .collect(),
    }
}

/// Build the replay payload from live history: assistant rows carry
/// thinking and their tool calls; tool-role messages merge their
/// excerpts into the matching call's row. Rows with no content, no
/// thinking, and no calls vanish (pure carriers fold into the calls
/// they answer).
fn replay_messages(
    history: &[ka_dialect::speaker::TurnMessage],
) -> Vec<ka_protocol::ReplayedMessage> {
    use ka_dialect::speaker::TurnRole;
    use std::collections::HashMap;
    let mut out: Vec<ka_protocol::ReplayedMessage> = Vec::new();
    // call id -> (message index, call index) within `out`
    let mut pending: HashMap<String, (usize, usize)> = HashMap::new();
    for m in history {
        match m.role {
            TurnRole::Tool => {
                for r in &m.results {
                    if let Some(&(mi, ci)) = pending.get(&r.call_id) {
                        let call = &mut out[mi].calls[ci];
                        call.result = Some(replay_excerpt(&r.content));
                        call.is_error = r.is_error;
                    }
                }
            }
            role => {
                let calls: Vec<ka_protocol::ReplayedCall> = m
                    .calls
                    .iter()
                    .map(|c| ka_protocol::ReplayedCall {
                        id: c.id.clone(),
                        tool: c.tool.clone(),
                        detail: crate::voice::call_detail(&c.tool, &c.arguments),
                        result: None,
                        is_error: false,
                    })
                    .collect();
                let mi = out.len();
                for (ci, c) in m.calls.iter().enumerate() {
                    pending.insert(c.id.clone(), (mi, ci));
                }
                out.push(ka_protocol::ReplayedMessage {
                    role: match role {
                        TurnRole::User => "user",
                        _ => "assistant",
                    }
                    .to_string(),
                    content: m.content.clone(),
                    digest: false,
                    thinking: m.thinking.clone().filter(|t| !t.trim().is_empty()),
                    calls,
                });
            }
        }
    }
    out.retain(|m| !m.content.trim().is_empty() || !m.calls.is_empty() || m.thinking.is_some());
    out
}

/// First non-empty line of a tool output, capped at 120 chars — the
/// compact replay note (the TUI finish-row excerpt's shape).
fn replay_excerpt(s: &str) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    line.chars().take(120).collect()
}

/// Wire-level name of a protocol effort level.
fn effort_name(level: ka_protocol::Effort) -> &'static str {
    match level {
        ka_protocol::Effort::Off => "off",
        ka_protocol::Effort::Low => "low",
        ka_protocol::Effort::Medium => "medium",
        ka_protocol::Effort::High => "high",
        ka_protocol::Effort::Max => "max",
    }
}

/// Live engine settings, derived from the initial config and mutated by
/// commands. Phase 3 persists these as strand `Change` records.
struct EngineState {
    model: Option<String>,
    effort: Option<ka_protocol::Effort>,
    mode: Mode,
    /// Live mirror of [`EngineState::mode`], shared with the code
    /// router's gate (see [`ModeSlot`]). Every mutation of `mode`
    /// goes through [`EngineState::set_live_mode`].
    mode_slot: ModeSlot,
    queue: VecDeque<String>,
    steers: Vec<String>,
    /// Record ids of persisted history messages, aligned with
    /// voice.history[..record_ids.len()] (mod digest truncation).
    record_ids: Vec<ka_protocol::RecordId>,
    /// Checkpoints taken this session: (commit id, timestamp). One
    /// shared log — the `Command::Checkpoint` arm and the `checkpoint`
    /// hand both append to it, so every taker is restorable via
    /// `/restore`.
    checkpoints: std::sync::Arc<crate::hands::checkpoint::CheckpointLog>,
    /// Spend/context guard thresholds + latches for this session.
    guards: crate::voice::GuardRuntime,
    /// Role selectors resolved against the catalog at engine start.
    roles: EngineRoles,
    /// Fresh strand awaiting its first completed turn: the auto-title
    /// fires once after that turn (fast role configured); resumed
    /// strands never re-title.
    needs_title: bool,
    /// `[git] auto_commit`: stage the whole worktree (`git add -A`) and
    /// commit after each completed turn.
    auto_commit: bool,
    /// GIT-03: `[git] trailers` appended under every auto-commit
    /// subject (the role-generated one included).
    commit_trailers: Vec<String>,
    /// `[verify] test` command; runs after edit-carrying turns.
    verify_test: Option<String>,
    /// One automatic verify-fix round per user prompt (loop guard).
    verify_round_done: bool,
    /// `[pokes] max`: auto-poke rounds allowed per user prompt.
    poke_max: u32,
    /// `[history]` persistence policy: strand writes honor it at attach.
    history: crate::config::History,
    /// Auto-poke rounds spent on the current user prompt.
    pokes_used: u32,
    /// Pending-todo set observed at the last poke check: an unchanged
    /// set means no progress — poking stops instead of burning rounds.
    poke_pending: Option<Vec<String>>,
    /// `!` passthrough outputs waiting to ride the next user prompt.
    shell_context: Vec<String>,
    /// CTX-12: live auto-compact trigger as a percent of the window
    /// (1-100). `None` = the reserve-based default trigger
    /// (max(15% of window, 16k) of free space). Set from
    /// `[context] autocompact_pct` and the `SetAutoCompact` command.
    autocompact_pct: Option<u64>,
    /// CTX-07: `[context] idle_minutes` — idle-task gate in minutes.
    /// `None` = idle task off.
    idle_minutes: Option<u64>,
    /// CTX-07: `[context] recap_minutes` — extra gate for the idle
    /// recap. `None` inherits `idle_minutes`.
    recap_minutes: Option<u64>,
    /// CTX-07: `[context] idle_compact_pct` — while idle, digest when
    /// the meter reaches this percent. `None` = no idle compaction.
    idle_compact_pct: Option<u64>,
    /// Last surface activity (prompt received / turn finished). The
    /// idle task measures from here.
    last_activity: std::time::Instant,
    /// Total turns completed this session (monotonic; survives digest
    /// truncation, unlike history indices). Drives the recap
    /// "new turns since the last recap" gate.
    turns_seen: u64,
    /// `turns_seen` value at the last idle recap; the recap fires only
    /// when `turns_seen` has moved past it.
    last_recap_turns: u64,
    /// MODEL-05: `[context] cache_warming = "idle"` armed (default off).
    cache_warming: bool,
    /// MODEL-05: cost of the most recent completed turn (USD); the
    /// warming gate compares it against the $0.05 threshold.
    last_turn_cost: f64,
    /// MODEL-05: `turns_seen` value at the last cache warm; the warm
    /// fires only when new turns happened since.
    last_warm_turns: u64,
    /// CTX-05: the active goal, when one is set (the fast role judges
    /// each settle whether the condition holds; the auto-poke prefix
    /// keeps the model working toward it).
    goal: Option<GoalRuntime>,
    /// CTX-06: the armed loop, when set (the tick re-sends the task on
    /// the interval; never armed in safe mode).
    loop_mode: Option<LoopRuntime>,
    /// CTX-08: `[context] prewalk` — the first prompt of a fresh strand
    /// earns a cheap bounded walk + fast-role orientation brief.
    prewalk: bool,
    /// CTX-08: prewalk still owed for the attached strand (fresh
    /// strands only; cleared once run or on strand switch).
    prewalk_pending: bool,
}

/// CTX-05: the live goal runtime.
#[derive(Debug, Clone)]
struct GoalRuntime {
    /// The condition the fast role judges at each settle.
    condition: String,
    /// Optional USD ceiling measured from the moment the goal was set.
    budget_usd: Option<f64>,
    /// Paused goals are neither judged nor poked (kept for /resume).
    paused: bool,
    /// Session spend at `Set` — the budget measures from here.
    spend_at_set: f64,
    /// `turns_seen` at `Set` — pause/resume notes report the distance.
    turns_at_set: u64,
}

/// CTX-06: the live loop runtime.
#[derive(Debug, Clone)]
struct LoopRuntime {
    /// The task re-sent each cycle.
    task: String,
    /// Minimum time between cycles (`Duration::ZERO` = fire as soon as
    /// the engine settles again).
    interval: Duration,
    /// Last settle (turn completion or arm/fire time) the interval
    /// measures from; refreshed on every completed turn.
    last_settle: std::time::Instant,
}
impl EngineState {
    /// The single mutation path for the session mode: updates the
    /// plain field AND the live cell the code router's gate reads.
    /// Callers still forward to `Voice::set_mode` themselves (the
    /// voice keeps its own copy for the admit gate).
    fn set_live_mode(&mut self, mode: Mode) {
        self.mode = mode;
        *self.mode_slot.write() = mode;
    }
}

impl From<Config> for EngineState {
    fn from(c: Config) -> Self {
        let mode = c.effective_mode();
        let warming = c.effective_cache_warming() == "idle";
        Self {
            model: c.model,
            effort: c.effort,
            mode,
            mode_slot: std::sync::Arc::new(parking_lot::RwLock::new(mode)),
            queue: VecDeque::new(),
            steers: Vec::new(),
            record_ids: Vec::new(),
            checkpoints: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
            guards: crate::voice::GuardRuntime::new(c.guards.spend_usd, c.guards.context_pct),
            roles: EngineRoles::default(),
            needs_title: false,
            auto_commit: c.git.auto_commit.unwrap_or(false),
            commit_trailers: c.git.trailers,
            poke_max: c.pokes.max.unwrap_or(3),
            history: c.history,
            verify_test: c.verify.test.clone(),
            pokes_used: 0,
            poke_pending: None,
            verify_round_done: false,
            shell_context: Vec::new(),
            autocompact_pct: c.context.autocompact_pct,
            idle_minutes: c.context.idle_minutes,
            recap_minutes: c.context.recap_minutes,
            idle_compact_pct: c.context.idle_compact_pct,
            last_activity: std::time::Instant::now(),
            turns_seen: 0,
            last_recap_turns: 0,
            cache_warming: warming,
            last_turn_cost: 0.0,
            last_warm_turns: 0,
            goal: None,
            loop_mode: None,
            prewalk: c.context.prewalk.unwrap_or(false),
            prewalk_pending: false,
        }
    }
}

type DynError = Box<dyn std::error::Error + Send + Sync>;

/// The session permission mode, shared live with the code hand's
/// router (the [`crate::features::FeatureSlot`] pattern): the engine
/// mirrors every `Voice::set_mode` site into this cell
/// ([`EngineState::set_live_mode`]), so `/mode`, a strand restore and
/// hook steering all re-arm the router's gate instead of leaving it on
/// a bootstrap-frozen snapshot. Defined here, ungated: mirroring the
/// mode is core state work even when the `code` hand compiles out.
pub(crate) type ModeSlot = std::sync::Arc<parking_lot::RwLock<Mode>>;

/// The engine's owned state bundle: one place, passed to every command
/// handler. Replaces the run()/side_command() split where arms were
/// partitioned by which pieces they happened to touch.
struct Ctx {
    cwd: std::path::PathBuf,
    events: mpsc::Sender<Event>,
    state: EngineState,
    voice: Voice,
    strand: ka_strand::StrandFile,
    /// Shared handles to the connected MCP servers (watchdog + refresh).
    mcp_shared: Vec<crate::mcp::McpShared>,
    /// Armed cron schedules (the cron hand arms; the tick fires).
    cron: std::sync::Arc<crate::hands::cron::CronTable>,
    /// Detached monitored processes (the monitor hand serves; the tick
    /// drains notes).
    monitors: std::sync::Arc<crate::hands::monitor::MonitorTable>,
    /// Watchdog/refresh bookkeeping from MCP supervisors.
    maintenance: mpsc::Receiver<crate::mcp::Maintenance>,
    /// [tools.mcp] discovery = "lazy": one mcp_call hand instead of a
    /// hand per server tool.
    mcp_lazy: bool,
    /// Background delegate-task registry (/tasks rows).
    agent_tasks: std::sync::Arc<crate::hands::tasks::AgentTaskTable>,
    /// EXT-12: the team member table (`team` hand + /tasks rows). None
    /// for leaf members (no nested teams) and safe mode.
    teams: Option<crate::hands::teams::TeamTable>,
    /// TUI-16: tangential jobs (`/tan`) — notes only, never a turn.
    tangential: std::sync::Arc<crate::hands::tangential::Tangential>,
    /// Live DAP sessions (/tasks rows), when the [debug] tier is on.
    debug: Option<std::sync::Arc<crate::dap::DebugManager>>,
    /// Configured MCP servers (spawn-on-enable source of truth; the
    /// shared set excludes bare-mode stripping).
    mcp_servers_cfg: Vec<crate::mcp::McpServerConfig>,
    /// Supervisor feed sender (spawns on runtime MCP enable).
    maintenance_tx: mpsc::Sender<crate::mcp::Maintenance>,
    /// The always-constructed LSP manager + its effective config
    /// (hands start on bootstrap or runtime enable).
    lsp: std::sync::Arc<crate::lsp::LspManager>,
    lsp_cfg: crate::config::Lsp,
    /// Effective debug config (runtime enable constructs the manager).
    debug_cfg: crate::config::Debug,
    /// Sandbox config (policy recompute on /sandbox).
    sandbox_cfg: crate::config::Sandbox,
    /// MCP prompts gathered at connect time (inventory re-emits replay
    /// this cache filtered by enabled servers).
    mcp_prompts: Vec<String>,
    /// The effective `[roles]` table (EXT-11): `@<role>` selectors in
    /// `SetModel` resolve through it.
    roles_cfg: crate::config::Roles,
}

#[allow(clippy::too_many_arguments)] // one engine bootstrap, every arg a distinct concern
async fn run(
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    mut config: Config,
    catalog: ka_dialect::Catalog,
    strand_choice: StrandChoice,
    speaker: Option<(
        ka_dialect::Wire,
        std::sync::Arc<dyn ka_dialect::speaker::Speaker>,
    )>,
    leaf: bool,
    team_speakers: Option<crate::hands::teams::MemberSpeakers>,
) -> Result<(), DynError> {
    let cwd = config
        .cwd
        .clone()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    // [[model_pins]]: the FIRST pin whose glob matches the session cwd
    // overwrites the configured model (roles and the strand baseline
    // read config.model below); no match leaves it untouched
    if let Some(pinned) = config.effective_model_for(&cwd) {
        config.model = Some(pinned);
    }
    // EXT-11: `--model @<role>` resolves through the [roles] table
    // (default/fast/extra) before anything reads the selector; an
    // unknown @name keeps the literal (the turn machine reports it
    // like any unresolvable selector)
    if let Some(resolved) = config
        .model
        .as_deref()
        .and_then(|sel| config.roles.resolve(sel))
        .map(str::to_string)
    {
        config.model = Some(resolved);
    }
    // safe mode: config-file hooks are customizations too — built-ins,
    // config chain, rules, and auth stay. Team members (leaf) skip MCP
    // servers outright: spawning every user server per member is both
    // heavy and a side-effect multiplier.
    let mcp_servers = if crate::conventions::bare_mode() || leaf {
        Vec::new()
    } else {
        config.mcp.clone()
    };
    // EXT-12: the member template rides the parent's config (same
    // model, rules, identity) — captured here, after pin/role
    // resolution reshaped `model`, before `EngineState::from` consumes
    // the config. `team_max` is read at the same point. (Leaf members
    // never read it; the clone is once-per-engine trivia.)
    let member_template = config.clone();
    let team_max = config.delegate.effective_team_max();
    let mode = config.effective_mode();
    let max_steps = config.effective_max_steps();
    // profiles activate ahead of [[rules]] (PERM-01/06): the gate list
    // is the resolved effective order, and `ka rules check` uses the
    // same fn
    let rules = config.effective_rules();
    let prompt_policy = config.effective_prompt_policy();
    let allowed_tools = config.permissions.allow.clone();
    // TOOL-04 code mode: the `code` hand's script-facing gate mirrors
    // this posture; the locals below move into the voice, so snapshot
    // them now (cfg-gated — no cost without the `code` feature)
    #[cfg(feature = "code")]
    let code_gate = (rules.clone(), prompt_policy, allowed_tools.clone());
    let delegate_rules = rules.clone();
    let hooks = if crate::conventions::bare_mode() || leaf {
        Vec::new()
    } else {
        config.hooks.clone()
    };
    // roles resolve once at engine start, through the same catalog the
    // main model uses (discovery overlays already applied by the caller)
    let roles = resolve_roles(&catalog, &config.roles, config.model.as_deref());
    // GIT-03: `[git] commit_role` — a `[roles]` name or a literal
    // selector, resolved through the catalog like every other role;
    // unresolvable → None and auto-commit keeps its fallback subject
    let commit_selector = config
        .git
        .commit_role
        .as_deref()
        .map(|raw| commit_role_selector(&config.roles, raw));
    let commit_role = commit_selector
        .as_deref()
        .and_then(|s| resolve_role(&catalog, s));
    // passport tier bootstrap (MODEL-01, ratified rule): every selector
    // this session can speak with an `auth = "passport"` dialect must
    // find a registered TokenSource NOW — an unregistered row is an
    // instructive error, never a mid-turn surprise. Safe mode keeps
    // passport auth available (it is auth, not a customization).
    for selector in [
        config.roles.default.as_deref(),
        config.roles.fast.as_deref(),
        config.model.as_deref(),
        commit_selector.as_deref(),
    ]
    .into_iter()
    .flatten()
    .chain(config.roles.extra.values().map(String::as_str))
    {
        let parsed = match ka_dialect::parse_selector(selector) {
            Ok(p) => p,
            Err(_) => continue, // the turn machine reports bad selectors
        };
        let Some(dialect) = catalog.get(&parsed.model_id()) else {
            continue; // synthetic dialects resolve the same tier below
        };
        if dialect.auth.as_deref() == Some("passport")
            && !ka_dialect::auth::passport_serves(&parsed.vendor)
        {
            return Err(format!(
                "model {} uses auth = \"passport\" but no passport source is registered — \
                 build with `--features passport` and run `ka login claude`, or pick a \
                 non-plan model",
                parsed.model_id()
            )
            .into());
        }
    }
    // the full table rides the ctx so `SetModel "@<role>"` resolves
    // the same way (EXT-11)
    let roles_table = config.roles.clone();
    let pathfinder_catalog = catalog.clone();
    // EXT-12: the team table clones the catalog too — members are full
    // engine sessions resolving roles/dialects exactly like the parent
    let teams_catalog = catalog.clone();
    let search_providers = config.effective_search();
    let web_allow_private = config.effective_web_allow_private();
    // resolve [generate_image] BEFORE the config is consumed by
    // EngineState: provider + key gating happens once, at bootstrap
    let gen_image_cfg = config.effective_generate_image(&cwd);
    let mut voice = Voice::new(catalog, cwd.clone(), mode, max_steps);
    if let Some((wire, speaker)) = speaker {
        voice = voice.with_speaker(wire, speaker);
    }
    // [features] disable + allow: validated once, primed into the
    // shared toggle slot every layer of this feature reads (specs
    // filter, admit guard, prompt build, the dynamic hands)
    let disabled_specs = crate::features::parse_specs(&config.features.disable)
        .map_err(|e| -> DynError { format!("[features] disable: {e}").into() })?;
    let features =
        crate::features::FeatureToggles::slot_with(disabled_specs, config.features.allow.clone());
    voice.set_feature_slot(features.clone());
    voice.set_rules(rules);
    voice.set_verify(config.verify.clone());
    // [prompt] identity_file + identity_inline: same trust class as
    // convention discovery (user-authored local text) — inert in safe
    // mode
    voice.set_identity_file(if crate::conventions::bare_mode() {
        None
    } else {
        config.prompt.identity_file.clone()
    });
    voice.set_identity_inline(if crate::conventions::bare_mode() {
        None
    } else {
        config.prompt.identity_inline.clone()
    });
    // project_doc_fallback_filenames: discovery self-gates on safe
    // mode; the names only widen the per-directory search
    voice.set_project_doc_fallbacks(config.project_doc_fallback_filenames.clone());
    voice.set_allowed_tools(allowed_tools);
    voice.set_hooks(hooks.clone());
    // [[mcp]] per-server policies (EXT-04): the ask bypass reads them
    // at admit time; hidden in safe mode like the rest of the tier
    voice.set_mcp_policies(if crate::conventions::bare_mode() {
        Vec::new()
    } else {
        config.mcp.clone()
    });
    voice.set_bash_background_ms(config.effective_bash_background_after_ms());
    voice.set_fallbacks(config.fallback.models.clone());
    // [virtual_models] EXT-11: per-turn routers (round-robin cursor on
    // the voice, failover through the turn fallback chain)
    voice.set_routers(&config.virtual_models);
    voice.set_max_image_mb(config.effective_max_image_mb());
    voice.set_context_promote(config.effective_context_promote());
    // MODEL-03: the pass-through service_tier hint rides every
    // main-line request from here on
    voice.set_service_tier(config.service_tier.clone());
    let delegate_shell_env = config.effective_shell_env();
    voice.set_shell_env(delegate_shell_env.clone());
    let sandbox_policy = ka_sandbox::policy_from_config(&config.sandbox.to_policy_config(), &cwd)?;
    voice.set_sandbox(sandbox_policy);
    // [permissions] prompt_policy: reviewable asks pose or are refused
    // (PERM-07; a config-authored posture, active in safe mode too)
    voice.set_prompt_policy(prompt_policy);
    // [guards] auto_review: the fast-role reviewer pre-screens reviewable
    // asks (opt-in; inert in safe mode — hooks-class customization)
    let auto_review = config.guards.auto_review.unwrap_or(false);
    voice.set_auto_review(
        auto_review && !crate::conventions::bare_mode(),
        roles.fast.clone(),
        config.guards.auto_review_tier.unwrap_or_default(),
    );
    // [guards] injection_check: the fast-role classifier's verdict rides
    // heuristic-flagged web results (PERM-08b; the heuristic itself is
    // always on; the classifier is hooks-class customization → inert in
    // safe mode, and degrades to heuristic-only without a fast role)
    voice.set_injection_check(config.guards.injection_check.unwrap_or(false));
    // safe mode: [lsp] spawns user-configured executables — same risk
    // class as hooks/MCP, so the diagnostics tier is inert there too.
    // The manager is always constructed (cheap, inert until started);
    // hands start below, once the strand has restored any toggles.
    let lsp_cfg = effective_lsp_cfg(&config);
    let lsp = std::sync::Arc::new(crate::lsp::LspManager::new(&cwd, &lsp_cfg));
    voice.set_lsp(lsp.clone());
    // the debug probe (Phase 8.2): a single `debug` hand over DAP,
    // Exec-tier control flow, Read-tier inspection — only under
    // [debug] enable = true (manager + hand start below, post-attach)
    let debug_cfg = effective_debug_cfg(&config);
    voice.set_web_allow_private(config.effective_web_allow_private());
    {
        let slot = voice.pathfinder_slot();
        slot.write().catalog = pathfinder_catalog;
    }
    let mcp_lazy = config.effective_mcp_lazy();
    let sandbox_cfg = config.sandbox.clone();
    // TOOL-04 code mode: the `[code]` limits ride the hand; resolved
    // now, before the config is consumed by EngineState (cfg-gated)
    #[cfg(feature = "code")]
    let code_cfg = config.effective_code();
    let mut state = EngineState::from(config);
    state.roles = roles;
    state.roles.commit = commit_role;
    // checkpoints live in ONE session log shared by the surface
    // command, the `checkpoint` hand, and `/restore`: hand-taken
    // snapshots are restorable exactly like surface-taken ones
    state.checkpoints = voice.tables().checkpoints.clone();
    // strand attach also restores any recorded feature-toggle snapshot
    // (over the config baseline, like model/mode) — so the spawn
    // decisions below honor it
    let strand = attach_strand(&events, &mut state, &mut voice, &cwd, &strand_choice).await?;
    // strand-restored sandbox override: recompute the live policy
    if let Some(mode) = voice.features_snapshot().2 {
        if let Err(e) = apply_sandbox_policy(&mut voice, &sandbox_cfg, &cwd, &mode) {
            events
                .send(Event::Error {
                    class: ErrorClass::Internal,
                    retryable: false,
                    message: format!("restoring sandbox mode {mode:?} failed: {e}"),
                })
                .await
                .ok();
        }
    }
    let agent_tasks = crate::hands::tasks::AgentTaskTable::new();
    let (maintenance_tx, maintenance_rx) = mpsc::channel(16);
    let tables = voice.tables().clone();
    // TUI-16: capture the pathfinder slot before `voice` moves into Ctx.
    let tangential_slot = voice.pathfinder_slot();
    let tangential_events = events.clone();
    let mut ctx = Ctx {
        cwd,
        events,
        state,
        voice,
        strand,
        mcp_shared: Vec::new(),
        cron: tables.cron.clone(),
        monitors: tables.monitors.clone(),
        maintenance: maintenance_rx,
        mcp_lazy,
        agent_tasks: agent_tasks.clone(),
        teams: None,
        tangential: std::sync::Arc::new(crate::hands::tangential::Tangential::new(
            tangential_slot,
            mode,
            agent_tasks.clone(),
            tangential_events,
            features.clone(),
            std::sync::Arc::new(hooks.clone()),
        )),
        debug: None,
        mcp_servers_cfg: mcp_servers.clone(),
        maintenance_tx: maintenance_tx.clone(),
        lsp: lsp.clone(),
        lsp_cfg: lsp_cfg.clone(),
        debug_cfg: debug_cfg.clone(),
        sandbox_cfg,
        mcp_prompts: Vec::new(),
        roles_cfg: roles_table,
    };
    // Leaf team members never get scheduling/monitoring automation hands.
    if leaf {
        ctx.voice.remove_hands(&["cron", "monitor"]);
    }
    // markdown agents: .ka/agents/*.md etc. become one `delegate` hand
    // (empty in safe mode). The hands register even when `agents` is
    // toggled off — disabling hides, so a re-enable is pure set
    // membership. Team members are leaves: no delegate, no tasks hand.
    let agents = if crate::conventions::bare_mode() || leaf {
        Vec::new()
    } else {
        crate::agents::AgentDef::discover(&ctx.cwd)
    };
    if !agents.is_empty() {
        let slot = ctx.voice.pathfinder_slot();
        ctx.voice.push_hand(std::sync::Arc::new(
            crate::hands::delegate::DelegateHand::new(
                agents,
                slot,
                mode,
                agent_tasks.clone(),
                ctx.events.clone(),
                features.clone(),
            )
            .with_hooks(std::sync::Arc::new(hooks.clone()))
            .with_mode_slot(ctx.state.mode_slot.clone())
            .with_policy(crate::hands::delegate::DelegatePolicy {
                // read at wiring time from the LIVE policy: a
                // strand-restored `/sandbox` override has already been
                // applied to the parent by now
                sandbox: Some(ctx.voice.sandbox_policy().clone()),
                rules: delegate_rules,
                shell_env: Some(delegate_shell_env),
            }),
        ));
        ctx.voice
            .push_hand(std::sync::Arc::new(crate::hands::tasks::TasksHand::new(
                agent_tasks.clone(),
            )));
    }
    // EXT-12 agent teams: one `team` hand over the shared member table
    // — the parent is the star router, members are leaf engines
    // (`spawn_leaf`). Safe mode pins the tier off (the cron/monitor
    // precedent: session-spawning automation is a customization tier);
    // `team_max = 0` keeps the hand registered so `team_spawn` refuses
    // informatively. Members never register it (leaf).
    if !leaf && !crate::conventions::bare_mode() {
        // the member template follows the parent's LIVE state, not the
        // config snapshot: the early `member_template` clone predates
        // `attach_strand`, so a resumed session's restored
        // settings.model would be lost. Re-pin model + feature posture
        // here — post-restore, like the `ctx.state.mode` argument below
        // (members start where the parent is; the same live-follow
        // delegate children get through the pathfinder slot).
        let mut member_template = member_template;
        member_template.model = ctx.state.model.clone();
        {
            let toggles = features.read();
            member_template.features.disable = toggles.disabled_specs();
            member_template.features.allow = toggles.allow_entries();
        }
        let table = crate::hands::teams::TeamTable::new(
            team_max,
            &ctx.cwd,
            teams_catalog,
            member_template,
            ctx.state.mode_slot.clone(),
            ctx.events.clone(),
            &ctx.strand,
            team_speakers,
        );
        ctx.voice
            .push_hand(std::sync::Arc::new(crate::hands::teams::TeamsHand::new(
                table.clone(),
            )));
        ctx.teams = Some(table);
    }
    // memory inbox: the model stages durable notes for user review
    // (customization tier — inert in safe mode and leaf members)
    if !crate::conventions::bare_mode() && !leaf {
        ctx.voice
            .push_hand(std::sync::Arc::new(crate::hands::memory::RememberHand));
    }
    // LSP hands: navigation (symbols/definition/references/diagnostics)
    // plus write-through on [lsp] write_through — only when the tier is
    // both configured and not toggled off (stripped on leaf members)
    if !leaf
        && lsp_cfg.enable == Some(true)
        && lsp_cfg.commands.as_ref().is_some_and(|c| !c.is_empty())
        && !features.read().is_disabled(&ka_protocol::FeatureSpec::Lsp)
    {
        lsp.start_all();
        for hand in lsp_hands(lsp.clone(), &lsp_cfg) {
            ctx.voice.push_hand(hand);
        }
    }
    // debug tier: manager + hand only when configured and not toggled
    // off (runtime enable constructs them on demand; stripped on leaves)
    if !leaf
        && debug_cfg.enable == Some(true)
        && !features
            .read()
            .is_disabled(&ka_protocol::FeatureSpec::Debug)
    {
        let debug = std::sync::Arc::new(crate::dap::DebugManager::new(
            debug_cfg.adapters.clone(),
            Some(ctx.events.clone()),
        ));
        ctx.voice
            .push_hand(std::sync::Arc::new(crate::hands::debug::DebugHand::new(
                debug.clone(),
            )));
        ctx.debug = Some(debug);
    }

    // MCP servers: spawn, handshake, list; each tool becomes a hand at
    // exec-tier clearance. Failures are per-server errors, never fatal.
    // Servers disabled by toggle at startup (config or strand) are not
    // spawned — enabling one later spawns it on the spot.
    let mut mcp_summary = Vec::with_capacity(mcp_servers.len());
    for cfg in &mcp_servers {
        if !features.read().server_enabled(&cfg.name) {
            mcp_summary.push(McpSummary {
                name: cfg.name.clone(),
                ok: false,
                tools: 0,
            });
            continue;
        }
        mcp_summary.push(ensure_mcp_server(&mut ctx, cfg.clone()).await);
    }
    features.write().set_live_mcp(
        ctx.mcp_shared
            .iter()
            .map(|s| s.name().to_string())
            .collect(),
    );

    // web search + fetch hands (search only when a provider is set;
    // stripped on leaf team members)
    if !leaf {
        for hand in crate::hands::web::hands(search_providers, web_allow_private) {
            ctx.voice.push_hand(hand);
        }
    }
    // generate_image hand: GATED on [generate_image] provider + API key
    // resolution (env/dotenv/keyring ladder) — an unkeyed config never
    // shows the tool; the `generate_image` feature toggle can still
    // hide a registered one mid-session (stripped on leaf members)
    if !leaf {
        if let Some(gen_cfg) = gen_image_cfg {
            ctx.voice.push_hand(std::sync::Arc::new(
                crate::hands::generate_image::GenerateImageHand::new(gen_cfg),
            ));
        }
    }
    // code hand (TOOL-04): one JavaScript script per call orchestrating
    // ka tools, only the final value back to the model. Gated on the
    // `code` CARGO feature (default-OFF — the default binary stays
    // byte-identical), safe mode, leaf status, and the session
    // `tool:code` runtime toggle.
    #[cfg(feature = "code")]
    if !crate::conventions::bare_mode()
        && !leaf
        && !features
            .read()
            .is_disabled(&ka_protocol::FeatureSpec::Tool("code".into()))
    {
        let (code_rules, code_prompt_policy, code_allowed_tools) = code_gate;
        let fast_role = ctx
            .state
            .roles
            .fast
            .clone()
            .and_then(|selector| ctx.voice.aux_handle(&selector));
        let router = std::sync::Arc::new(crate::hands::code::CodeRouter::new(
            // the built-in session registry, sharing the live tables
            // (jobs/cron/monitors/checkpoints) and the pathfinder slot —
            // routed calls execute through hands that see the same
            // session state the model's calls do
            crate::hands::registry_with_pathfinder(
                ctx.voice.pathfinder_slot(),
                ctx.voice.todo_slot().clone(),
                ctx.voice.tables().clone(),
            ),
            code_rules,
            ctx.state.mode_slot.clone(),
            code_prompt_policy,
            code_allowed_tools,
            features.clone(),
            ctx.events.clone(),
            fast_role,
        ));
        ctx.voice
            .push_hand(std::sync::Arc::new(crate::hands::code::CodeHand::new(
                router, code_cfg,
            )));
    }
    // lazy `mcp_call` + meta `mcp` hands: rebuild_mcp_meta_hands — run
    // by ensure_mcp_server on every path that grows the connected set —
    // is their sole registrar. Pushing either here used to register a
    // second copy, and providers rejected the whole turn with
    // "tools: Tool names must be unique".
    // One bootstrap inventory card replaces the old ad-hoc notes: the
    // surface renders tools/MCP/agents/skills compactly at startup.
    emit_inventory(&ctx).await;
    // surfaces initialize the todo section immediately, before any turn
    ctx.events
        .send(Event::Todos { items: Vec::new() })
        .await
        .ok();
    // session_start hook (roadmap 9.2): strand attached, inventory out.
    // Team members skip it — they are subagent-class sessions, and the
    // delegate children they mirror never fire session hooks either.
    if !leaf {
        ctx.voice
            .lifecycle_hook(
                crate::config::HookEvent::SessionStart,
                serde_json::json!({
                    "event": "session_start",
                    "cwd": ctx.cwd.display().to_string(),
                }),
                &ctx.events,
            )
            .await;
    }
    // attach the jobs persistence file BEFORE the command loop: every
    // registration during the session must persist (adoption of
    // survivors from earlier sessions happens here, at startup — the
    // documented `set_path` semantic). Team members skip the shared
    // file: adoption would pull the parent's persisted jobs into every
    // member (double execution); a member's own bash backgrounds stay
    // in-memory and die with it.
    if !leaf {
        if let Some(path) = crate::hands::jobs::default_jobs_file() {
            ctx.voice.jobs().set_path(path);
        }
    }
    // opportunistic spill-dir GC: detached-job spill files older than
    // the retention window are deleted in the background, at most once
    // a day per machine — no startup path waits on the scan
    crate::hands::gc_spills_at_startup();
    // CTX-07: the idle task polls at a fixed 30s cadence between
    // commands. `interval_at` (not `interval`) skips the immediate
    // first tick, and `Delay` collapses any tick bursts the turn
    // processing caused into one overdue tick.
    let idle_tick_period = idle_tick_period();
    let mut idle_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + idle_tick_period,
        idle_tick_period,
    );
    idle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            maybe = commands.recv() => {
                let Some(cmd) = maybe else { break };
                handle_command(cmd, &mut commands, &mut ctx).await?;
                // CTX-06: an armed loop fires at most ONE cycle per
                // command wakeup, between turn cycles only. A tighter
                // re-fire loop would starve the command channel (Loop
                // Off arriving mid-turn is dropped by the voice), so
                // further cycles wait for the next wakeup or tick.
                if let Some(task) = loop_prompt_due(&mut ctx.state) {
                    handle_command(loop_prompt(&task), &mut commands, &mut ctx).await?;
                }
            }
            maybe = ctx.maintenance.recv() => {
                if let Some(update) = maybe {
                    handle_maintenance(update, &mut ctx, &mut commands).await;
                }
            }
            _ = idle_tick.tick() => {
                idle_tick_work(&mut ctx).await;
                // batch 7: cron schedules + monitor streams share this
                // settle tick
                session_tick(&mut ctx, &mut commands).await?;
                if let Some(task) = loop_prompt_due(&mut ctx.state) {
                    handle_command(loop_prompt(&task), &mut commands, &mut ctx).await?;
                }
            }
        }
    }
    // session_end hook (roadmap 9.2): the command channel closed; the
    // surface may already be gone, so steering surfaces best-effort.
    // Skipped for team members (the session_start twin).
    if !leaf {
        ctx.voice
            .lifecycle_hook(
                crate::config::HookEvent::SessionEnd,
                serde_json::json!({
                    "event": "session_end",
                    "cwd": ctx.cwd.display().to_string(),
                }),
                &ctx.events,
            )
            .await;
    }
    Ok(())
}

/// The single, exhaustive command dispatcher. Every `Command` variant is
/// handled (or explicitly rejected) here — no unreachable arms, no
/// routing split. Arms that need to poll the surface mid-turn (Prompt)
/// also receive the receiver.
/// Apply one MCP supervisor update: swap the server's hands for the
/// fresh tool set and tell the surface. EXT-05: server-initiated
/// requests and notifications land here too (elicitation → ask,
/// sampling → default-role completion, list_changed → refresh,
/// resources/updated → Note).
async fn handle_maintenance(
    update: crate::mcp::Maintenance,
    ctx: &mut Ctx,
    commands: &mut mpsc::Receiver<Command>,
) {
    // EXT-05: server-initiated traffic is handled before the bookkeeping
    // arms — these messages carry their own routing
    match update {
        crate::mcp::Maintenance::McpServerRequest {
            server,
            id,
            method,
            params,
        } => {
            handle_mcp_server_request(server, id, method, params, ctx, commands).await;
            return;
        }
        crate::mcp::Maintenance::McpServerNotification {
            server,
            method,
            params: _,
        } => {
            handle_mcp_server_notification(server, method, ctx).await;
            return;
        }
        _ => {}
    }
    if let crate::mcp::Maintenance::McpVerified {
        server,
        tools,
        changed,
    } = update
    {
        // cache verification: swap hands only on a real change (a
        // matching list keeps the registered hands — and the provider
        // prompt cache — untouched); the cache refreshes either way
        if changed {
            if let Some(shared) = ctx.mcp_shared.iter().find(|s| s.name() == server).cloned() {
                if !ctx.mcp_lazy {
                    let hands: Vec<std::sync::Arc<dyn crate::hands::Hand>> = shared
                        .model_tools()
                        .iter()
                        .map(|t| {
                            std::sync::Arc::new(crate::mcp::McpHand::new(t.clone(), shared.clone()))
                                as _
                        })
                        .collect();
                    ctx.voice.replace_server_hands(&server, hands);
                }
                ctx.events
                    .send(Event::Note {
                        message: format!(
                            "mcp {server}: tool list changed since the cached copy — refreshed \
                             ({} tools)",
                            tools.len()
                        ),
                    })
                    .await
                    .ok();
                emit_inventory(ctx).await;
            }
        }
        // the verified server is live: backfill the prompts list the
        // cache fast path skipped (the /prompt popup and inventory
        // otherwise lose them on every cache-hit startup)
        if let Some(shared) = ctx.mcp_shared.iter().find(|s| s.name() == server).cloned() {
            let listed =
                tokio::time::timeout(std::time::Duration::from_secs(20), shared.list_prompts())
                    .await;
            if let Ok(Ok(items)) = listed {
                for p in items {
                    ctx.mcp_prompts.push(format!(
                        "{}/{}{}",
                        p.server,
                        p.name,
                        if p.arguments.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", p.arguments.join(", "))
                        }
                    ));
                }
                emit_inventory(ctx).await;
            }
        }
        if let Some(cfg) = ctx.mcp_servers_cfg.iter().find(|c| c.name == server) {
            crate::mcp_cache::store_tools(cfg, &tools);
        }
        return;
    }
    let (server, tools, note) = match update {
        crate::mcp::Maintenance::McpReconnected {
            server,
            tools,
            gained,
        } => (
            server,
            tools,
            format!("mcp {{server}} reconnected (+{gained} tools)"),
        ),
        // handled by the if-let above; listed for exhaustiveness
        crate::mcp::Maintenance::McpVerified { .. } => return,
        // handled by the EXT-05 match above; listed for exhaustiveness
        crate::mcp::Maintenance::McpServerRequest { .. }
        | crate::mcp::Maintenance::McpServerNotification { .. } => return,
        crate::mcp::Maintenance::McpRefreshed {
            server,
            tools,
            delta,
        } => (
            server,
            tools,
            format!("mcp {{server}} refreshed ({delta:+} tools)"),
        ),
        crate::mcp::Maintenance::McpGaveUp { server } => {
            ctx.events
                .send(Event::Error {
                    class: ErrorClass::Protocol,
                    retryable: false,
                    message: format!("mcp {server}: reconnect failed; tools error until restart"),
                })
                .await
                .ok();
            return;
        }
    };
    apply_refresh_update(&server, tools, &note, ctx).await;
}

/// The tail [`handle_maintenance`] and the EXT-05 list_changed path
/// share: swap the server's hands for the fresh tool set, refresh the
/// on-disk cache, and surface the note. (A direct recursive call from
/// `handle_mcp_server_notification` would box the whole async fn.)
async fn apply_refresh_update(
    server: &str,
    tools: Vec<crate::mcp::McpTool>,
    note: &str,
    ctx: &mut Ctx,
) {
    let server = server.to_string();
    if let Some(shared) = ctx.mcp_shared.iter().find(|s| s.name() == server).cloned() {
        if !ctx.mcp_lazy {
            let hands: Vec<std::sync::Arc<dyn crate::hands::Hand>> = shared
                .model_tools()
                .iter()
                .map(|t| {
                    std::sync::Arc::new(crate::mcp::McpHand::new(t.clone(), shared.clone())) as _
                })
                .collect();
            ctx.voice.replace_server_hands(&server, hands);
        }
        // lazy mode: the single mcp_call hand reads the refreshed tool
        // list through McpShared — nothing to swap
    }
    // every observed tool set refreshes the on-disk cache, or the next
    // startup registers a stale list and re-triggers the swap churn
    if let Some(cfg) = ctx.mcp_servers_cfg.iter().find(|c| c.name == server) {
        crate::mcp_cache::store_tools(cfg, &tools);
    }
    ctx.events
        .send(Event::Note {
            message: note.replace("{server}", &server),
        })
        .await
        .ok();
}

/// EXT-05: answer one server-initiated request. `roots/list` was
/// already handled in the pump; `elicitation/create` becomes an
/// `Event::Ask` on the existing ask machinery and routes the choice
/// back as the JSON-RPC result; `sampling/createMessage` completes
/// with the session's default role model; anything else is a
/// JSON-RPC "method not supported" error.
async fn handle_mcp_server_request(
    server: String,
    id: serde_json::Value,
    method: String,
    params: serde_json::Value,
    ctx: &mut Ctx,
    commands: &mut mpsc::Receiver<Command>,
) {
    let Some(shared) = ctx.mcp_shared.iter().find(|s| s.name() == server).cloned() else {
        return;
    };
    match method.as_str() {
        "elicitation/create" => {
            let ask = crate::mcp::elicitation_ask(&params);
            let (question, options) = (ask.question.clone(), ask.options.clone());
            let choice = engine_ask(
                &ctx.events,
                commands,
                ka_protocol::AskId(format!("mcp-elicit-{server}")),
                question,
                options,
            )
            .await;
            let result = crate::mcp::elicitation_result(choice, &ask);
            if shared.respond(&id, result).await.is_err() {
                // the server went away mid-ask; nothing to route
            }
        }
        "sampling/createMessage" => {
            if crate::conventions::bare_mode() {
                let _ = shared
                    .respond_error(&id, -32601, "sampling is unavailable in safe mode")
                    .await;
                return;
            }
            let Some(model) = ctx.state.roles.default.clone() else {
                let _ = shared
                    .respond_error(
                        &id,
                        -32000,
                        "no default model is configured for MCP sampling \
                         (set [roles] default)",
                    )
                    .await;
                return;
            };
            let current_mode = *ctx.state.mode_slot.read();
            if current_mode != ka_protocol::Mode::Allow {
                if current_mode == ka_protocol::Mode::DontAsk
                    || ctx.voice.prompt_policy() == crate::config::PromptPolicy::Deny
                {
                    let _ = shared
                        .respond_error(
                            &id,
                            -32000,
                            "MCP sampling denied by permission posture (dontAsk / prompt_policy=deny)",
                        )
                        .await;
                    return;
                }
                let max_tok = params["maxTokens"].as_u64().unwrap_or(512).clamp(1, 4096);
                let choice = engine_ask(
                    &ctx.events,
                    commands,
                    ka_protocol::AskId(format!("mcp-sampling-{server}")),
                    format!(
                        "MCP server `{server}` requests model sampling with {model} \
                         (maxTokens={max_tok}). Allow?"
                    ),
                    vec!["allow".into(), "deny".into()],
                )
                .await;
                if choice != Some(0) {
                    let _ = shared
                        .respond_error(&id, -32000, "MCP sampling request denied by user")
                        .await;
                    return;
                }
            }
            let mut capped_params = params;
            if let Some(obj) = capped_params.as_object_mut() {
                let clamped = obj
                    .get("maxTokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(512)
                    .clamp(1, 4096);
                obj.insert("maxTokens".to_string(), serde_json::json!(clamped));
            }
            match ctx.voice.mcp_sampling(&model, &capped_params).await {
                Ok(result) => {
                    ctx.events
                        .send(Event::Note {
                            message: format!(
                                "mcp {server}: sampling request completed with {model}"
                            ),
                        })
                        .await
                        .ok();
                    let _ = shared.respond(&id, result).await;
                }
                Err(e) => {
                    let _ = shared.respond_error(&id, -32000, &e).await;
                }
            }
        }
        other => {
            let _ = shared
                .respond_error(
                    &id,
                    -32601,
                    &format!("method {other:?} is not supported by this client"),
                )
                .await;
        }
    }
}

/// EXT-05: react to one server-initiated notification.
/// `notifications/tools/list_changed` refreshes that server's catalog
/// through the same rebuild path an explicit refresh uses;
/// `notifications/resources/updated` is informational. Unknown
/// notifications are dropped (they were before, too).
async fn handle_mcp_server_notification(server: String, method: String, ctx: &mut Ctx) {
    if method == "notifications/tools/list_changed" {
        let Some(shared) = ctx.mcp_shared.iter().find(|s| s.name() == server).cloned() else {
            return;
        };
        ctx.events
            .send(Event::Note {
                message: format!("mcp {server}: server signaled tools/list_changed"),
            })
            .await
            .ok();
        match shared.refresh_and_install().await {
            Ok((tools, delta)) => {
                apply_refresh_update(
                    &server,
                    tools,
                    &format!("mcp {{server}} refreshed ({delta:+} tools)"),
                    ctx,
                )
                .await;
            }
            Err(e) => {
                ctx.events
                    .send(Event::Note {
                        message: format!("mcp {server}: refresh after list_changed failed: {e}"),
                    })
                    .await
                    .ok();
            }
        }
        return;
    }
    if method == "notifications/resources/updated" {
        ctx.events
            .send(Event::Note {
                message: format!(
                    "mcp {server}: server reports resources updated — re-read via the \
                     `mcp` tool to see fresh contents"
                ),
            })
            .await
            .ok();
    }
}

/// Recompute and install the live sandbox policy for `mode` ("off" |
/// "fs"). The `fs` policy comes from the config allowlist — unpersisted
/// session grants drop on off→fs (fresh-policy semantics).
fn apply_sandbox_policy(
    voice: &mut Voice,
    cfg: &crate::config::Sandbox,
    cwd: &std::path::Path,
    mode: &str,
) -> Result<(), String> {
    let mut policy_cfg = cfg.to_policy_config();
    policy_cfg.mode = Some(mode.to_string());
    let policy = ka_sandbox::policy_from_config(&policy_cfg, cwd)
        .map_err(|e| format!("sandbox {mode}: {e}"))?;
    voice.set_sandbox(policy);
    Ok(())
}

/// Spawn + handshake + hands for one configured MCP server (bootstrap
/// and runtime enable). Failures are per-server errors, never fatal.
async fn ensure_mcp_server(ctx: &mut Ctx, cfg: crate::mcp::McpServerConfig) -> McpSummary {
    // EXT-03: HTTP-auth servers get a Note sink (the authorize URL
    // lands there) threaded into every connect — including watchdog
    // reconnects — plus the EXT-05 inbound pump. The forwarder owns
    // the receiver end; notes outlive the connect gate.
    let hooks = if cfg.url.is_some() {
        let (notes_tx, mut notes_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn({
            let events = ctx.events.clone();
            async move {
                while let Some(message) = notes_rx.recv().await {
                    events.send(Event::Note { message }).await.ok();
                }
            }
        });
        crate::mcp_oauth::FlowHooks {
            notes: Some(notes_tx),
            allow_headless: std::io::IsTerminal::is_terminal(&std::io::stdin()),
            authorize_timeout: crate::mcp_oauth::DEFAULT_AUTHORIZE_TIMEOUT,
        }
    } else {
        crate::mcp_oauth::FlowHooks::default()
    };
    let spawn_pump = |shared: &crate::mcp::McpShared, ctx: &Ctx| {
        tokio::spawn(crate::mcp::pump_inbound(
            shared.clone(),
            ctx.cwd.clone(),
            ctx.maintenance_tx.clone(),
        ));
    };
    // cache hit: register the remembered hands immediately — startup
    // never blocks on the handshake, the tool list stays stable across
    // runs (provider prompt-cache stability) — and verify in the
    // background; the watchdog spawned alongside owns retries
    if let Some(cached) = crate::mcp_cache::load_tools(&cfg) {
        let count = cached.len();
        let shared = crate::mcp::McpShared::placeholder(cfg.clone(), cached).with_auth_hooks(hooks);
        if !ctx.mcp_lazy {
            for tool in shared.model_tools() {
                ctx.voice
                    .push_hand(std::sync::Arc::new(crate::mcp::McpHand::new(
                        tool,
                        shared.clone(),
                    )));
            }
        }
        ctx.mcp_shared.push(shared.clone());
        spawn_pump(&shared, ctx);
        // verify_initial owns the first connect, then chains into the
        // supervise watchdog — never two spawn_connects in flight
        tokio::spawn(crate::mcp::verify_initial(
            shared,
            ctx.maintenance_tx.clone(),
            crate::mcp::WatchdogTiming::default(),
        ));
        rebuild_mcp_meta_hands(ctx);
        ctx.events
            .send(Event::Note {
                message: format!(
                    "mcp {}: {count} tool(s) from cache — verifying in background",
                    cfg.name
                ),
            })
            .await
            .ok();
        return McpSummary {
            name: cfg.name.clone(),
            ok: true,
            tools: count,
        };
    }
    // interactive OAuth can take minutes (browser round trip): the
    // connect gate widens for servers that may run the flow
    let connect_gate = if cfg.oauth_enabled() {
        std::time::Duration::from_secs(320)
    } else {
        std::time::Duration::from_secs(20)
    };
    let connected = tokio::time::timeout(
        connect_gate,
        crate::mcp::McpClient::spawn_connect_with(&cfg, &hooks),
    )
    .await;
    match connected {
        Ok(Ok((client, tools))) => {
            let tool_count = tools.len();
            // remember the advertised list: the next startup registers
            // from cache and never waits on this handshake
            crate::mcp_cache::store_tools(&cfg, &tools);
            let shared = crate::mcp::McpShared::new(cfg.clone(), client, tools.clone())
                .with_auth_hooks(hooks);
            // eager mode: every policy-visible tool becomes its own
            // hand (EXT-04 enabled_tools/disabled_tools filter); lazy
            // mode fronts the server through the single mcp_call hand
            if !ctx.mcp_lazy {
                for tool in shared.model_tools() {
                    ctx.voice
                        .push_hand(std::sync::Arc::new(crate::mcp::McpHand::new(
                            tool,
                            shared.clone(),
                        )));
                }
            }
            // prompts for the surface's /prompt popup (20s gate like
            // tools), cached for inventory re-emits
            let listed =
                tokio::time::timeout(std::time::Duration::from_secs(20), shared.list_prompts())
                    .await;
            if let Ok(Ok(items)) = listed {
                for p in items {
                    ctx.mcp_prompts.push(format!(
                        "{}/{}{}",
                        p.server,
                        p.name,
                        if p.arguments.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", p.arguments.join(", "))
                        }
                    ));
                }
            }
            ctx.mcp_shared.push(shared.clone());
            spawn_pump(&shared, ctx);
            tokio::spawn(crate::mcp::supervise(
                shared,
                ctx.maintenance_tx.clone(),
                crate::mcp::WatchdogTiming::default(),
            ));
            rebuild_mcp_meta_hands(ctx);
            McpSummary {
                name: cfg.name.clone(),
                ok: true,
                tools: tool_count,
            }
        }
        Ok(Err(e)) => {
            ctx.events
                .send(Event::Error {
                    class: ErrorClass::Protocol,
                    retryable: false,
                    message: format!("mcp {}: {e}", cfg.name),
                })
                .await
                .ok();
            McpSummary {
                name: cfg.name.clone(),
                ok: false,
                tools: 0,
            }
        }
        Err(_) => {
            ctx.events
                .send(Event::Error {
                    class: ErrorClass::Protocol,
                    retryable: false,
                    message: format!("mcp {}: connect timed out", cfg.name),
                })
                .await
                .ok();
            McpSummary {
                name: cfg.name.clone(),
                ok: false,
                tools: 0,
            }
        }
    }
}

/// The lazy `mcp_call` + meta hands front the whole connected set —
/// rebuild both whenever that set grows, so a newly connected server
/// appears in their listings.
fn rebuild_mcp_meta_hands(ctx: &mut Ctx) {
    let slot = ctx.voice.feature_slot();
    if ctx.mcp_lazy && !ctx.mcp_shared.is_empty() {
        ctx.voice.replace_named_hands(
            "mcp_call",
            vec![std::sync::Arc::new(crate::mcp::McpCallHand::new(
                ctx.mcp_shared.clone(),
                slot.clone(),
            ))],
        );
    }
    if !ctx.mcp_shared.is_empty() {
        ctx.voice.replace_named_hands(
            "mcp",
            vec![std::sync::Arc::new(crate::mcp::McpMetaHand::new(
                ctx.mcp_shared.clone(),
                slot,
            ))],
        );
    }
    // `tool_search` ranks whatever is connected right now (it runs on
    // every path that grows the shared set)
    *ctx.voice.tables().mcp_catalog.write() = ctx.mcp_shared.clone();
}

/// Emit the inventory card from live state (bootstrap and after every
/// toggle change): visible tools, per-server MCP, agents, skills,
/// cached prompts filtered to enabled servers, and the disabled-spec
/// snapshot for the surface's toggle UI.
async fn emit_inventory(ctx: &Ctx) {
    let agents: Vec<String> = if crate::conventions::bare_mode() {
        Vec::new()
    } else {
        crate::agents::AgentDef::discover(&ctx.cwd)
            .iter()
            .filter(|a| ctx.voice.feature_slot().read().agent_enabled(&a.name))
            .map(|a| a.name.clone())
            .collect()
    };
    let skills: Vec<ka_protocol::SkillEntry> = crate::conventions::discover_skills(&ctx.cwd)
        .into_iter()
        .filter(|s| ctx.voice.feature_slot().read().skill_enabled(&s.name))
        .map(|s| ka_protocol::SkillEntry {
            name: s.name,
            source: s.source,
        })
        .collect();
    let mcp: Vec<McpSummary> = ctx
        .mcp_servers_cfg
        .iter()
        .map(
            |cfg| match ctx.mcp_shared.iter().find(|s| s.name() == cfg.name) {
                Some(shared) => McpSummary {
                    name: cfg.name.clone(),
                    ok: true,
                    tools: shared.tools().len(),
                },
                None => McpSummary {
                    name: cfg.name.clone(),
                    ok: false,
                    tools: 0,
                },
            },
        )
        .collect();
    let prompts: Vec<String> = {
        let features = ctx.voice.feature_slot();
        let guard = features.read();
        ctx.mcp_prompts
            .iter()
            .filter(|p| {
                let server = p.split('/').next().unwrap_or_default();
                guard.server_enabled(server)
            })
            .cloned()
            .collect()
    };
    let (disabled, _, _) = ctx.voice.features_snapshot();
    ctx.events
        .send(Event::Inventory {
            tools: ctx.voice.visible_hand_names(),
            mcp,
            agents,
            skills,
            prompts,
            disabled,
        })
        .await
        .ok();
}

/// Persist the toggle snapshot to the strand (last record wins on
/// resume), announce it, and refresh the inventory card. The positive
/// allowlist rides the same record as `allow:<entry>` strings
/// ([crate::features::encode_snapshot]).
async fn persist_features(ctx: &mut Ctx) {
    let (disabled, allow, sandbox) = ctx.voice.features_snapshot();
    let snapshot = crate::features::encode_snapshot(&disabled, &allow);
    let _ = ctx.strand.append(ka_strand::Record::Change {
        id: ka_strand::new_record_id(),
        model: None,
        effort: None,
        mode: None,
        features: Some(snapshot),
        sandbox: sandbox.clone(),
    });
    ctx.events
        .send(Event::FeaturesChanged { disabled, sandbox })
        .await
        .ok();
    emit_inventory(ctx).await;
}

/// Start the LSP tier on runtime enable. Disable is pure hiding — the
/// spawned servers idle until the session ends. Returns a refusal note
/// when there is nothing configured to enable.
fn sync_lsp(ctx: &mut Ctx, enabled: bool) -> Option<String> {
    if !enabled {
        return None;
    }
    if lsp_cfg_ready(&ctx.lsp_cfg) {
        if ctx.voice.hand_names().iter().any(|n| n.starts_with("lsp_")) {
            return None; // already installed (was hidden)
        }
        ctx.lsp.start_all();
        for hand in lsp_hands(ctx.lsp.clone(), &ctx.lsp_cfg) {
            ctx.voice.push_hand(hand);
        }
        None
    } else {
        Some("lsp tier is not configured ([lsp] enable + commands) — nothing to enable".into())
    }
}

fn lsp_cfg_ready(cfg: &crate::config::Lsp) -> bool {
    cfg.enable == Some(true) && cfg.commands.as_ref().is_some_and(|c| !c.is_empty())
}

/// Construct the debug tier on runtime enable. Disable is pure hiding.
fn sync_debug(ctx: &mut Ctx, enabled: bool) -> Option<String> {
    if !enabled {
        return None;
    }
    if ctx.debug.is_some() {
        return None; // hand already installed (was hidden)
    }
    if ctx.debug_cfg.enable != Some(true) {
        return Some("debug tier is not configured ([debug] enable) — nothing to enable".into());
    }
    let debug = std::sync::Arc::new(crate::dap::DebugManager::new(
        ctx.debug_cfg.adapters.clone(),
        Some(ctx.events.clone()),
    ));
    ctx.voice
        .push_hand(std::sync::Arc::new(crate::hands::debug::DebugHand::new(
            debug.clone(),
        )));
    ctx.debug = Some(debug);
    None
}

/// The full `SetFeature` path: spawn side effects for the tiers that
/// need them, update the live set, persist + announce. Disabling is
/// always pure hiding (specs filter + admit guard + dynamic listings);
/// enabling an MCP server that was never spawned starts it on the spot.
async fn apply_feature_toggle(ctx: &mut Ctx, spec: &ka_protocol::FeatureSpec, enabled: bool) {
    // bare mode pins every customization tier off — enabling is refused
    // outright so a toggle can never widen a --safe-mode session
    if enabled && crate::conventions::bare_mode() {
        ctx.events
            .send(Event::Note {
                message: "safe mode pins features off — restart without --safe-mode to enable"
                    .to_string(),
            })
            .await
            .ok();
        return;
    }
    let note = match spec {
        ka_protocol::FeatureSpec::Mcp | ka_protocol::FeatureSpec::McpServer(_) => {
            let name = match spec {
                ka_protocol::FeatureSpec::McpServer(n) => Some(n.clone()),
                _ => None,
            };
            if let Some(name) = name {
                if !ctx.mcp_servers_cfg.iter().any(|c| c.name == name) {
                    Some(format!(
                        "no configured mcp server {name:?} — nothing to toggle"
                    ))
                } else {
                    mcp_toggle(ctx, &name, enabled).await
                }
            } else {
                // `mcp` fans out over every configured server
                let names: Vec<String> =
                    ctx.mcp_servers_cfg.iter().map(|c| c.name.clone()).collect();
                for name in names {
                    mcp_toggle(ctx, &name, enabled).await;
                }
                None
            }
        }
        ka_protocol::FeatureSpec::Lsp => sync_lsp(ctx, enabled),
        ka_protocol::FeatureSpec::Debug => sync_debug(ctx, enabled),
        _ => None,
    };
    if let Some(note) = note {
        ctx.events.send(Event::Note { message: note }).await.ok();
    }
    if ctx.voice.set_feature(spec, enabled) {
        persist_features(ctx).await;
    }
}

/// Spawn one MCP server on enable (no-op when it's already connected —
/// it was merely hidden). Disable needs nothing here.
async fn mcp_toggle(ctx: &mut Ctx, name: &str, enabled: bool) -> Option<String> {
    if !enabled || ctx.mcp_shared.iter().any(|s| s.name() == name) {
        return None;
    }
    let Some(cfg) = ctx.mcp_servers_cfg.iter().find(|c| c.name == name).cloned() else {
        return Some(format!("no configured mcp server {name:?}"));
    };
    let summary = ensure_mcp_server(ctx, cfg).await;
    ctx.voice.feature_slot().write().set_live_mcp(
        ctx.mcp_shared
            .iter()
            .map(|s| s.name().to_string())
            .collect(),
    );
    (summary.ok).then_some(None).flatten()
}

/// The full `SetSandbox` path: validate, recompute the policy, record
/// the override, persist + announce.
async fn apply_sandbox_mode(ctx: &mut Ctx, mode: &str) {
    if mode != "off" && mode != "fs" {
        ctx.events
            .send(Event::Note {
                message: format!("unknown sandbox mode {mode:?} (expected off|fs)"),
            })
            .await
            .ok();
        return;
    }
    if mode == "fs" && crate::conventions::bare_mode() {
        ctx.events
            .send(Event::Note {
                message: "safe mode pins the sandbox off — restart without --safe-mode".to_string(),
            })
            .await
            .ok();
        return;
    }
    match apply_sandbox_policy(&mut ctx.voice, &ctx.sandbox_cfg, &ctx.cwd, mode) {
        Ok(()) => {
            ctx.voice
                .feature_slot()
                .write()
                .set_sandbox_override(Some(mode.to_string()));
            persist_features(ctx).await;
        }
        Err(e) => {
            ctx.events
                .send(Event::Error {
                    class: ErrorClass::Internal,
                    retryable: false,
                    message: e,
                })
                .await
                .ok();
        }
    }
}

async fn handle_command(
    cmd: Command,
    commands: &mut mpsc::Receiver<Command>,
    ctx: &mut Ctx,
) -> Result<(), DynError> {
    match cmd {
        Command::Prompt {
            text,
            schema,
            images,
            allowed_tools,
            model,
            skills,
        } => {
            // a fresh user prompt earns a fresh verify-fix round
            ctx.state.verify_round_done = false;
            // and it is surface activity for the idle task (CTX-07)
            ctx.state.last_activity = std::time::Instant::now();
            // CTX-08: the prewalk runs once, before the first turn of a
            // fresh strand — a cheap walk + fast-role brief rides the
            // context as a leading system-note (degrades to a note)
            if ctx.state.prewalk && ctx.state.prewalk_pending {
                ctx.state.prewalk_pending = false;
                run_prewalk(&mut ctx.voice, &ctx.state, &ctx.cwd, &ctx.events).await;
            }
            // explicit skill invocation (/skill:<name>): fail closed
            // BEFORE the turn — a missing or disabled skill must not
            // run a hollow prompt (same admit-time stance as
            // SetFeature: the refusal names the toggle)
            if !skills.is_empty() {
                let known = crate::conventions::discover_skills(&ctx.cwd);
                for name in &skills {
                    let refusal = {
                        let slot = ctx.voice.feature_slot();
                        let features = slot.read();
                        if !known.iter().any(|s| &s.name == name) {
                            Some(format!(
                                "skill {name} not found — nothing ran (⚡ lists what ka found)"
                            ))
                        } else if !features.skill_enabled(name) {
                            if features.skills_enabled() {
                                Some(format!(
                                    "skill {name} is disabled (skill:{name}) — /features on skill:{name}; nothing ran"
                                ))
                            } else {
                                Some(
                                    "skills are disabled (skills) — /features on skills; nothing ran"
                                        .to_string(),
                                )
                            }
                        } else {
                            None
                        }
                    };
                    if let Some(message) = refusal {
                        ctx.events.send(Event::Note { message }).await.ok();
                        ctx.events.send(Event::Idle).await.ok();
                        return Ok(());
                    }
                }
                ctx.voice.set_turn_skills(Some(skills.clone()));
            }
            // `!` passthrough outputs ride along as leading context
            let text = if ctx.state.shell_context.is_empty() {
                text
            } else {
                let block = ctx.state.shell_context.join("\n\n");
                ctx.state.shell_context.clear();
                format!("{block}\n\n{text}")
            };
            // user_prompt_submit hook (roadmap 9.2): before the turn
            // starts; steering (e.g. a mode switch) applies immediately
            ctx.voice
                .lifecycle_hook(
                    crate::config::HookEvent::UserPromptSubmit,
                    serde_json::json!({
                        "event": "user_prompt_submit",
                        "prompt": text,
                    }),
                    &ctx.events,
                )
                .await;
            // per-turn scoping (roadmap 9.7, custom-command frontmatter):
            // `allowed-tools` restricts this turn's toolset, `model`
            // switches the model for this turn only (never persisted —
            // no Change record, the session default is untouched)
            if let Some(tools) = &allowed_tools {
                ctx.voice.set_turn_tools(Some(tools.clone()));
            }
            let model_override = model.as_deref().and_then(|sel| {
                if ka_dialect::parse_selector(sel).is_ok() {
                    Some(sel.to_string())
                } else {
                    None
                }
            });
            if model.is_some() && model_override.is_none() {
                ctx.events
                    .send(Event::Note {
                        message: format!(
                            "command model {:?} is not a valid selector — using the session model",
                            model
                        ),
                    })
                    .await
                    .ok();
            }
            dispatch_turn(
                commands,
                &ctx.events,
                &mut ctx.state,
                &mut ctx.voice,
                text,
                schema,
                images,
                &mut ctx.strand,
                &ctx.cwd,
                model_override,
            )
            .await;
            ctx.voice.set_turn_tools(None);
            ctx.voice.set_turn_skills(None);
            settle_context(&mut ctx.voice, &mut ctx.state, &mut ctx.strand, &ctx.events).await;
            // auto-title once, right after the first completed turn of a
            // fresh strand (queued follow-on turns are later turns)
            maybe_auto_title(&mut ctx.voice, &mut ctx.state, &mut ctx.strand, &ctx.events).await;
            // plan-mode goals earn a hill-climbability rating (fast
            // role): no metric to climb → the harness pushes back
            maybe_rate_hill_climb(&mut ctx.voice, &ctx.state, &ctx.events).await;
            // Settling: drain the queue as follow-on turns (scoped turns
            // do not carry their restriction into queued follow-ups),
            // then auto-poke while the model's own todo list still has
            // pending items ([pokes] max, default 3). Most agent
            // failures are early exits, not wrong answers: the harness
            // continues the work itself, bounded. Pokes stop on a
            // non-Done stop (aborts and errors — wire-level retries
            // have already run for transient failures), in plan mode
            // (research, not execution), and when the pending set
            // stops moving (no progress = stop, don't burn rounds).
            // A poke is a normal turn: its prompt rides the strand as
            // a user message, visible on every surface.
            ctx.state.pokes_used = 0;
            ctx.state.poke_pending = None;
            loop {
                while let Some(queued) = ctx.state.queue.pop_front() {
                    dispatch_turn(
                        commands,
                        &ctx.events,
                        &mut ctx.state,
                        &mut ctx.voice,
                        queued,
                        None,
                        Vec::new(),
                        &mut ctx.strand,
                        &ctx.cwd,
                        None,
                    )
                    .await;
                    settle_context(&mut ctx.voice, &mut ctx.state, &mut ctx.strand, &ctx.events)
                        .await;
                    // fresh user input resets the progress baseline
                    ctx.state.poke_pending = None;
                }
                if ctx.state.pokes_used >= ctx.state.poke_max
                    || ctx.voice.mode() == Mode::Readonly
                    || ctx.voice.last_stop() != Stop::Done
                {
                    break;
                }
                let pending: Vec<String> = ctx
                    .voice
                    .todo_slot()
                    .lock()
                    .iter()
                    .filter(|i| i.state == ka_protocol::TodoState::Pending)
                    .map(|i| i.text.clone())
                    .collect();
                if pending.is_empty() || ctx.state.poke_pending.as_ref() == Some(&pending) {
                    break;
                }
                ctx.state.pokes_used += 1;
                ctx.state.poke_pending = Some(pending.clone());
                let list = pending
                    .iter()
                    .map(|t| format!("- {t}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut poke = format!(
                    "(ka auto-poke {}/{}) Your todo list still has {} pending item(s):\n{list}\n\
                     Continue the work: finish the remaining items, re-verify anything you \
                     marked done without evidence (a confidence jump is a claim, not a \
                     measurement), keep the todo list current, and stop with a short summary \
                     when everything is done.",
                    ctx.state.pokes_used,
                    ctx.state.poke_max,
                    pending.len()
                );
                // CTX-05: an active (unpaused) goal steers the poke —
                // the continue-prompt leads with the condition so the
                // keep-working round pulls toward it
                if let Some(goal) = ctx.state.goal.as_ref().filter(|g| !g.paused) {
                    poke = format!(
                        "goal: {} — keep working toward it\n\n{poke}",
                        goal.condition
                    );
                }
                ctx.events
                    .send(Event::Note {
                        message: format!(
                            "auto-poke {}/{}: {} todo item(s) still pending — continuing",
                            ctx.state.pokes_used,
                            ctx.state.poke_max,
                            pending.len()
                        ),
                    })
                    .await
                    .ok();
                dispatch_turn(
                    commands,
                    &ctx.events,
                    &mut ctx.state,
                    &mut ctx.voice,
                    poke,
                    None,
                    Vec::new(),
                    &mut ctx.strand,
                    &ctx.cwd,
                    None,
                )
                .await;
                settle_context(&mut ctx.voice, &mut ctx.state, &mut ctx.strand, &ctx.events).await;
            }
            // feature toggles buffered mid-turn: replay through the full
            // handler now that the turn (and its follow-ups) settled
            for cmd in ctx.voice.take_pending_features() {
                match cmd {
                    Command::SetFeature { spec, enabled } => {
                        apply_feature_toggle(ctx, &spec, enabled).await;
                    }
                    Command::SetSandbox { mode } => apply_sandbox_mode(ctx, &mode).await,
                    _ => {}
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::RefreshMcp => {
            for shared in &ctx.mcp_shared {
                // hidden servers keep their hands registered; refreshing
                // them would just re-install tools nobody can call
                if !ctx
                    .voice
                    .feature_slot()
                    .read()
                    .server_enabled(shared.name())
                {
                    continue;
                }
                match shared.refresh_and_install().await {
                    Ok((_tools, delta)) => {
                        if delta != 0 {
                            let hands: Vec<std::sync::Arc<dyn crate::hands::Hand>> = shared
                                .model_tools()
                                .iter()
                                .map(|t| {
                                    std::sync::Arc::new(crate::mcp::McpHand::new(
                                        t.clone(),
                                        shared.clone(),
                                    )) as _
                                })
                                .collect();
                            ctx.voice.replace_server_hands(shared.name(), hands);
                        }
                        ctx.events
                            .send(Event::Note {
                                message: format!(
                                    "mcp {} refreshed ({delta:+} tools)",
                                    shared.name()
                                ),
                            })
                            .await
                            .ok();
                    }
                    Err(e) => {
                        ctx.events
                            .send(Event::Error {
                                class: ErrorClass::Protocol,
                                retryable: false,
                                message: e,
                            })
                            .await
                            .ok();
                    }
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::CallPrompt { server, name, args } => {
            if !ctx.voice.feature_slot().read().server_enabled(&server) {
                ctx.events
                    .send(Event::Error {
                        class: ErrorClass::Unsupported,
                        retryable: false,
                        message: format!(
                            "mcp: server {server:?} is disabled by a session feature toggle (`mcp:{server}`)"
                        ),
                    })
                    .await
                    .ok();
                ctx.events.send(Event::Idle).await.ok();
                return Ok(());
            }
            let Some(shared) = ctx.mcp_shared.iter().find(|s| s.name() == server).cloned() else {
                ctx.events
                    .send(Event::Error {
                        class: ErrorClass::Protocol,
                        retryable: false,
                        message: format!("mcp: no such server {server:?}"),
                    })
                    .await
                    .ok();
                ctx.events.send(Event::Idle).await.ok();
                return Ok(());
            };
            match shared.get_prompt(&name, &args).await {
                Ok(text) => {
                    dispatch_turn(
                        commands,
                        &ctx.events,
                        &mut ctx.state,
                        &mut ctx.voice,
                        text,
                        None,
                        Vec::new(),
                        &mut ctx.strand,
                        &ctx.cwd,
                        None,
                    )
                    .await;
                    settle_context(&mut ctx.voice, &mut ctx.state, &mut ctx.strand, &ctx.events)
                        .await;
                    ctx.events.send(Event::Idle).await.ok();
                }
                Err(e) => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Protocol,
                            retryable: false,
                            message: format!("prompt {server}/{name}: {e}"),
                        })
                        .await
                        .ok();
                    ctx.events.send(Event::Idle).await.ok();
                }
            }
        }
        Command::SetModel { selector } => {
            // EXT-11: an `@<role>` selector resolves through the
            // [roles] table (default/fast/extra) before it applies;
            // an unknown @name keeps the literal and fails at the
            // next turn like any unresolvable selector
            let selector = expand_role_selector(&ctx.roles_cfg, &selector);
            // EXT-01: the advisory model-switch hooks bracket the
            // switch (from = the session's current selector); nothing
            // they emit can veto or redirect the switch
            let previous = ctx.state.model.clone();
            ctx.voice
                .lifecycle_hook(
                    crate::config::HookEvent::PreModelSwitch,
                    serde_json::json!({
                        "event": "pre_model_switch",
                        "from": previous,
                        "to": selector,
                    }),
                    &ctx.events,
                )
                .await;
            ctx.state.model = Some(selector.clone());
            ctx.voice.pathfinder_slot().write().model = Some(selector.clone());
            let _ = ctx.strand.append(ka_strand::Record::Change {
                id: ka_strand::new_record_id(),
                model: Some(selector.clone()),
                effort: None,
                mode: None,
                features: None,
                sandbox: None,
            });
            ctx.events
                .send(Event::ModelChanged {
                    selector: selector.clone(),
                })
                .await?;
            ctx.voice
                .lifecycle_hook(
                    crate::config::HookEvent::PostModelSwitch,
                    serde_json::json!({
                        "event": "post_model_switch",
                        "from": previous,
                        "to": selector,
                    }),
                    &ctx.events,
                )
                .await;
            // a model change re-announces the stored level so surfaces stay truthful
            // (the level itself is not model-derived)
            ctx.events
                .send(Event::EffortChanged {
                    level: ctx.state.effort.unwrap_or(ka_protocol::Effort::Off),
                })
                .await
                .ok();
            ctx.events.send(Event::Idle).await.ok();
        }
        // EXT-09: the session's output style. Resolution happens
        // engine-side (fail closed: an unresolvable name changes
        // nothing) and the voice re-resolves the file every prompt
        // assembly. Session-scoped: no Change record v1 — styles do
        // not survive a resume, like the in-memory context trims.
        Command::SetStyle { style } => {
            match style {
                Some(name) => match crate::conventions::resolve_style(&name, &ctx.cwd) {
                    Some(path) => {
                        ctx.voice.set_style(Some(name.clone()));
                        ctx.events
                            .send(Event::Note {
                                message: format!("style · {} ({})", name, path.display()),
                            })
                            .await
                            .ok();
                    }
                    None => {
                        let current = ctx
                            .voice
                            .style_name()
                            .map_or_else(|| "built-in identity".to_string(), |s| format!("{s:?}"));
                        ctx.events
                            .send(Event::Note {
                                message: format!(
                                    "style {name:?} not found (.ka/styles/, \
                                     ~/.config/ka/styles/) — keeping {current}"
                                ),
                            })
                            .await
                            .ok();
                    }
                },
                None => {
                    ctx.voice.set_style(None);
                    ctx.events
                        .send(Event::Note {
                            message: "style · off (built-in identity)".to_string(),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::SetMode { mode } => {
            ctx.state.set_live_mode(mode);
            ctx.voice.set_mode(mode);
            let _ = ctx.strand.append(ka_strand::Record::Change {
                id: ka_strand::new_record_id(),
                model: None,
                effort: None,
                mode: Some(mode),
                features: None,
                sandbox: None,
            });
            ctx.events.send(Event::ModeChanged { mode }).await?;
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::SetEffort { level } => {
            let name = effort_name(level);
            ctx.state.effort = Some(level);
            ctx.voice.set_effort(Some(name.to_string()));
            let _ = ctx.strand.append(ka_strand::Record::Change {
                id: ka_strand::new_record_id(),
                model: None,
                effort: Some(level),
                mode: None,
                features: None,
                sandbox: None,
            });
            ctx.events.send(Event::EffortChanged { level }).await?;
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::SetFeature { spec, enabled } => {
            apply_feature_toggle(ctx, &spec, enabled).await;
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::SetSandbox { mode } => {
            apply_sandbox_mode(ctx, &mode).await;
            ctx.events.send(Event::Idle).await.ok();
        }
        // CTX-12: live auto-compact trigger. Session-scoped (like the
        // model/mode commands it mirrors); `[context] autocompact_pct`
        // is the durable path. Fail-closed: out-of-range percents are
        // rejected with a note, the live setting untouched.
        Command::SetAutoCompact { pct } => {
            match pct {
                Some(p) if !(1..=100).contains(&p) => {
                    let current = match ctx.state.autocompact_pct {
                        Some(p) => format!("{p}%"),
                        None => "default".to_string(),
                    };
                    ctx.events
                        .send(Event::Note {
                            message: format!(
                                "auto-compact trigger: {p}% is out of range (1-100) — keeping \
                                 {current}"
                            ),
                        })
                        .await
                        .ok();
                }
                Some(p) => {
                    ctx.state.autocompact_pct = Some(p);
                    ctx.events
                        .send(Event::Note {
                            message: format!("auto-compact trigger: {p}%"),
                        })
                        .await
                        .ok();
                }
                None => {
                    ctx.state.autocompact_pct = None;
                    ctx.events
                        .send(Event::Note {
                            message: "auto-compact trigger: default".to_string(),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        // CTX-02: deterministic context trim, on demand. Stale tool
        // args and old tool outputs always trim (manual shake: no
        // minimum-savings gate on outputs); `thinking`/`images`
        // additionally strip those parts from everything but the most
        // recent turn. In-memory, like the settle-time ladder — the
        // strand keeps the originals.
        Command::Shake { thinking, images } => {
            let ratio = voice_ratio(&ctx.voice);
            let to_tok = |bytes: u64| (bytes as f64 / ratio.max(0.1)) as u64;
            // one note per applied trim: args → outputs → thinking → images
            let mut trimmed_any = false;
            let (shaken, arg_bytes) = ctx.voice.shake(crate::voice::SHAKE_ARG_CAP);
            if shaken > 0 {
                trimmed_any = true;
                ctx.events
                    .send(Event::Note {
                        message: format!(
                            "shake: truncated {shaken} stale tool call arg(s) (~{} / ~{} tok)",
                            crate::voice::fmt_bytes(arg_bytes),
                            to_tok(arg_bytes),
                        ),
                    })
                    .await
                    .ok();
            }
            let pruned = ctx.voice.prune_tool_outputs_with(ratio, 0);
            if pruned > 0 {
                trimmed_any = true;
                ctx.events
                    .send(Event::Note {
                        message: format!(
                            "shake: pruned old tool output (~{pruned} tok, full text spilled)"
                        ),
                    })
                    .await
                    .ok();
            }
            if thinking {
                let (msgs, bytes) = ctx.voice.strip_old_thinking();
                if msgs > 0 {
                    trimmed_any = true;
                    ctx.events
                        .send(Event::Note {
                            message: format!(
                                "shake: stripped thinking from {msgs} message(s) (~{} / ~{} tok)",
                                crate::voice::fmt_bytes(bytes),
                                to_tok(bytes),
                            ),
                        })
                        .await
                        .ok();
                }
            }
            if images {
                let (n, bytes) = ctx.voice.strip_old_images();
                if n > 0 {
                    trimmed_any = true;
                    ctx.events
                        .send(Event::Note {
                            message: format!(
                                "shake: stripped {n} old image(s) (~{} / ~{} tok)",
                                crate::voice::fmt_bytes(bytes),
                                to_tok(bytes),
                            ),
                        })
                        .await
                        .ok();
                }
            }
            if !trimmed_any {
                ctx.events
                    .send(Event::Note {
                        message: "shake: nothing to trim".to_string(),
                    })
                    .await
                    .ok();
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::ContextBreakdown => {
            // CTX-09: the regular buckets, then the trim-opportunity
            // hints (`hint:`-prefixed; surfaces exclude them from
            // used/free totals).
            let mut parts = ctx.voice.context_breakdown();
            parts.extend(ctx.voice.context_hint_parts());
            let window = ctx.voice.window_tokens();
            ctx.events
                .send(Event::ContextBreakdown { parts, window })
                .await?;
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::ListTasks => {
            // /tasks dashboard rows: background delegates + bash jobs
            let mut rows = ctx.agent_tasks.rows();
            if let Some(teams) = &ctx.teams {
                // EXT-12 member rows (`team <name> …` — the TUI pill
                // ignores them; the modal shows them like any roster)
                rows.extend(teams.rows());
            }
            for j in ctx.voice.jobs().snapshot() {
                let state = match j.state {
                    crate::hands::jobs::JobState::Running => "running".to_string(),
                    crate::hands::jobs::JobState::Exited(c) => format!("exit {c}"),
                };
                let cmd: String = j.cmd.chars().take(60).collect();
                rows.push(format!("job-{}`  {state}  {cmd}", j.id));
            }
            if let Some(debug) = &ctx.debug {
                for s in debug.sessions() {
                    rows.push(format!(
                        "dap {}  {:7}  breakpoints in {} file(s)",
                        s.id(),
                        s.status(),
                        s.breakpoints().len()
                    ));
                }
            }
            if rows.is_empty() {
                rows.push("no background tasks or jobs".to_string());
            }
            ctx.events.send(Event::Tasks { rows }).await.ok();
        }
        Command::TaskDetail { id } => {
            match ctx.agent_tasks.result(id) {
                Some(text) => {
                    ctx.events.send(Event::TaskDetail { id, text }).await.ok();
                }
                None => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Unsupported,
                            retryable: false,
                            message: format!("no such task: t-{id}"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::DebugRoster => {
            let rows = match &ctx.debug {
                Some(debug) => debug.roster(),
                None => vec!["debug tier disabled ([debug] enable)".to_string()],
            };
            ctx.events.send(Event::DebugRoster { rows }).await.ok();
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::Steer { text } => ctx.state.steers.push(text),
        Command::Queue { text } => ctx.state.queue.push_back(text),
        // TUI-16: tangential spawn/list/cancel — answers arrive as Notes,
        // never as a turn. Safe mode strips session-spawning tools (the
        // cron/monitor precedent), so `/tan` refuses there too.
        Command::Tan { text } => {
            if crate::conventions::bare_mode() {
                ctx.events
                    .send(Event::Note {
                        message: "tan is unavailable in safe mode".to_string(),
                    })
                    .await
                    .ok();
            } else {
                ctx.tangential.handle(&text, &ctx.cwd).await;
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        // TRUST INVARIANT: Command::Shell is only ever sent by the local
        // TUI (see the variant docs in ka-protocol) — that is what makes
        // running it without the permission gate sound.
        Command::Shell { command } => {
            // user-typed `!` passthrough: run it, show it, and stage the
            // redacted output as context for the next prompt. Runs only
            // between turns (the voice owns the channel mid-turn).
            let ran = tokio::time::timeout(
                std::time::Duration::from_secs(120),
                tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(&command)
                    .current_dir(&ctx.cwd)
                    // the timeout drops the output future: without this,
                    // a hung child keeps running detached past the bound
                    .kill_on_drop(true)
                    .output(),
            )
            .await;
            let (output, note) = match ran {
                Ok(Ok(out)) => {
                    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
                    text.push_str(&String::from_utf8_lossy(&out.stderr));
                    let note = (!out.status.success())
                        .then(|| format!("exit {}", out.status.code().unwrap_or(-1)));
                    (text, note)
                }
                Ok(Err(e)) => (String::new(), Some(format!("spawn failed: {e}"))),
                Err(_) => (String::new(), Some("timed out after 120s".to_string())),
            };
            let output = crate::hands::secrets::redact(&tail_chars(output.trim(), 4_000));
            while ctx.state.shell_context.len() >= 5 {
                ctx.state.shell_context.remove(0);
            }
            ctx.state.shell_context.push(format!(
                "[the user ran `{command}` in their shell — output follows]\n{output}{}",
                note.as_deref()
                    .map(|n| format!("\n({n})"))
                    .unwrap_or_default()
            ));
            ctx.events
                .send(Event::ShellOutput {
                    command,
                    output,
                    note,
                })
                .await
                .ok();
        }
        Command::Abort => {}
        Command::Compact { focus } => {
            run_digest(
                &mut ctx.voice,
                &mut ctx.state,
                &mut ctx.strand,
                &ctx.events,
                focus,
            )
            .await;
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::TransferStrand { focus } => {
            // /transfer = digest + fresh child strand: the summary
            // becomes the new session's entire carried memory
            let digested = run_digest(
                &mut ctx.voice,
                &mut ctx.state,
                &mut ctx.strand,
                &ctx.events,
                focus,
            )
            .await;
            if matches!(digested, DigestResult::NoModel) {
                ctx.events.send(Event::Idle).await.ok();
                return Ok(());
            }
            let Some(summary) = ctx.voice.digest_text().map(str::to_string) else {
                ctx.events
                    .send(Event::Error {
                        class: ErrorClass::Protocol,
                        retryable: false,
                        message: "transfer needs a digest summary".to_string(),
                    })
                    .await
                    .ok();
                ctx.events.send(Event::Idle).await.ok();
                return Ok(());
            };
            match transfer_strand(&ctx.cwd, &ctx.strand, &summary) {
                Ok(path) => {
                    match attach_strand(
                        &ctx.events,
                        &mut ctx.state,
                        &mut ctx.voice,
                        &ctx.cwd,
                        &StrandChoice::Path(path),
                    )
                    .await
                    {
                        Ok(fresh) => {
                            let id = strand_id(&fresh);
                            ctx.strand = fresh;
                            // the transfer strand already carries its
                            // "(transfer)" Title — keep it; a second
                            // auto-title would leave resume and the
                            // running surface with divergent labels
                            ctx.state.needs_title = false;
                            ctx.events
                                .send(Event::Note {
                                    message: format!(
                                        "transferred to fresh session {id} — context compacted \
                                         into a digest; resume with ka --session {id}"
                                    ),
                                })
                                .await
                                .ok();
                        }
                        Err(e) => {
                            ctx.events
                                .send(Event::Error {
                                    class: ErrorClass::Protocol,
                                    retryable: false,
                                    message: format!("transfer created but attach failed: {e}"),
                                })
                                .await
                                .ok();
                        }
                    }
                }
                Err(e) => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Unsupported,
                            retryable: false,
                            message: format!("transfer failed: {e}"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::SwitchStrand { id } => {
            let choice = if id.trim() == "new" {
                StrandChoice::New
            } else {
                let matched = match ka_strand::resolve_id(&ctx.cwd, &id) {
                    Ok(ka_strand::IdMatch::None) => {
                        // memorable names sit between ids and failure,
                        // same as `ka --session`
                        ka_strand::resolve_name(&ctx.cwd, &id)
                    }
                    other => other,
                };
                match matched {
                    Ok(ka_strand::IdMatch::Unique(summary)) => StrandChoice::Path(summary.path),
                    Ok(ka_strand::IdMatch::Ambiguous(candidates)) => {
                        let ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
                        ctx.events
                            .send(Event::Error {
                                class: ErrorClass::Unsupported,
                                retryable: false,
                                message: format!(
                                    "session id '{}' is ambiguous: {}",
                                    id,
                                    ids.join(", ")
                                ),
                            })
                            .await
                            .ok();
                        ctx.events.send(Event::Idle).await.ok();
                        return Ok(());
                    }
                    other => {
                        let message = match other {
                            Ok(ka_strand::IdMatch::None) => {
                                format!("no session matches '{id}'")
                            }
                            Err(e) => format!("session lookup failed: {e}"),
                            _ => unreachable!(),
                        };
                        ctx.events
                            .send(Event::Error {
                                class: ErrorClass::Unsupported,
                                retryable: false,
                                message,
                            })
                            .await
                            .ok();
                        ctx.events.send(Event::Idle).await.ok();
                        return Ok(());
                    }
                }
            };
            match attach_strand(
                &ctx.events,
                &mut ctx.state,
                &mut ctx.voice,
                &ctx.cwd,
                &choice,
            )
            .await
            {
                Ok(fresh) => {
                    ctx.strand = fresh;
                    // fresh sessions stay completely silent — only an
                    // existing-session switch earns one terse note
                    if !matches!(choice, StrandChoice::New) {
                        ctx.events
                            .send(Event::Note {
                                message: format!("switched to session {}", strand_id(&ctx.strand)),
                            })
                            .await
                            .ok();
                    }
                }
                Err(e) => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Protocol,
                            retryable: false,
                            message: format!("session switch failed: {e}"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::ForkStrand { turns } => {
            match fork_strand(&ctx.cwd, &ctx.strand, turns) {
                Ok(path) => {
                    match attach_strand(
                        &ctx.events,
                        &mut ctx.state,
                        &mut ctx.voice,
                        &ctx.cwd,
                        &StrandChoice::Path(path),
                    )
                    .await
                    {
                        Ok(fresh) => {
                            ctx.strand = fresh;
                            // the fork carries the "(fork)" placeholder title:
                            // let the fast-role auto-title replace it
                            ctx.state.needs_title = true;
                        }
                        Err(e) => {
                            ctx.events
                                .send(Event::Error {
                                    class: ErrorClass::Protocol,
                                    retryable: false,
                                    message: format!("fork created but attach failed: {e}"),
                                })
                                .await
                                .ok();
                        }
                    }
                }
                Err(e) => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Unsupported,
                            retryable: false,
                            message: format!("fork failed: {e}"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::SetName { name } => {
            let clean = name.trim();
            let valid = !clean.is_empty()
                && clean.chars().count() <= 64
                && clean.chars().all(|c| !c.is_whitespace() && !c.is_control());
            if !valid {
                ctx.events
                    .send(Event::Note {
                        message: "usage: /name <word> — 1–64 chars, no spaces".to_string(),
                    })
                    .await
                    .ok();
            } else {
                let _ = ctx.strand.append(ka_strand::Record::Name {
                    id: ka_strand::new_record_id(),
                    name: clean.to_string(),
                });
                ctx.events
                    .send(Event::Note {
                        message: format!(
                            "session name set: {clean} — resume with ka --session {clean}"
                        ),
                    })
                    .await
                    .ok();
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::Checkpoint => {
            // same machinery + same session log as the `checkpoint` hand
            // (crate::hands::checkpoint::take_recorded)
            match crate::hands::checkpoint::take_recorded(&ctx.cwd, &ctx.state.checkpoints) {
                Ok((id, ts)) => {
                    ctx.events
                        .send(Event::Note {
                            message: format!(
                                "checkpoint {} saved ({ts})",
                                crate::checkpoint::short(&id)
                            ),
                        })
                        .await
                        .ok();
                }
                Err(e) => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Internal,
                            retryable: false,
                            message: format!("checkpoint failed: {e}"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::RestoreCheckpoint { id } => {
            if id.trim() == "list" {
                let message = {
                    let log = ctx.state.checkpoints.lock();
                    if log.is_empty() {
                        "no checkpoints in this session yet".to_string()
                    } else {
                        log.iter()
                            .map(|(cid, ts)| format!("{}  {ts}", crate::checkpoint::short(cid)))
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                };
                ctx.events.send(Event::Note { message }).await.ok();
            } else {
                let resolved = {
                    let log = ctx.state.checkpoints.lock();
                    log.iter()
                        .find(|(cid, _)| {
                            cid == &id
                                || crate::checkpoint::short(cid) == id.trim()
                                || cid.starts_with(id.trim())
                        })
                        .cloned()
                };
                match resolved {
                    None => {
                        ctx.events
                            .send(Event::Error {
                                class: ErrorClass::Unsupported,
                                retryable: false,
                                message: format!("no checkpoint matches '{id}' in this session"),
                            })
                            .await
                            .ok();
                    }
                    Some((cid, ts)) => {
                        let answer = engine_ask(
                            &ctx.events,
                            commands,
                            AskId(format!("ckpt-restore-{cid}")),
                            format!(
                                "restore checkpoint {} ({ts})? restores tracked files as of {ts} (untracked/post-checkpoint files remain)",
                                crate::checkpoint::short(&cid)
                            ),
                            vec!["restore".to_string(), "cancel".to_string()],
                        )
                        .await;
                        match answer {
                            Some(0) => match crate::checkpoint::restore(&ctx.cwd, &cid) {
                                Ok(()) => {
                                    ctx.events
                                        .send(Event::Note {
                                            message: format!(
                                                "restored checkpoint {}",
                                                crate::checkpoint::short(&cid)
                                            ),
                                        })
                                        .await
                                        .ok();
                                }
                                Err(e) => {
                                    ctx.events
                                        .send(Event::Error {
                                            class: ErrorClass::Internal,
                                            retryable: false,
                                            message: format!("restore failed: {e}"),
                                        })
                                        .await
                                        .ok();
                                }
                            },
                            _ => {
                                ctx.events
                                    .send(Event::Note {
                                        message: "canceled".to_string(),
                                    })
                                    .await
                                    .ok();
                            }
                        }
                    }
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::Rewind { turns } => {
            // SES-03/CTX-01 share the rewind bookkeeping (strand records
            // + live truncation); the abandoned tail summary is the
            // batch-3 behavior: ≤3 sentences, no focus steer
            match rewind_records(&mut ctx.voice, &mut ctx.state, &mut ctx.strand, turns).await {
                Some((kept, Some((idx, view)))) if kept > 0 && idx == kept => {
                    if !view.trim().is_empty() {
                        record_branch_summary(
                            &mut ctx.voice,
                            &ctx.state,
                            &mut ctx.strand,
                            &ctx.events,
                            view,
                            None,
                            3,
                        )
                        .await;
                    }
                    ctx.events
                        .send(Event::Note {
                            message: format!("rewound {turns} turn(s)"),
                        })
                        .await
                        .ok();
                }
                Some(_) => {
                    ctx.events
                        .send(Event::Note {
                            message: format!("rewound {turns} turn(s)"),
                        })
                        .await
                        .ok();
                }
                None => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Protocol,
                            retryable: false,
                            message: format!("cannot rewind {turns} turn(s): not enough history"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        // CTX-01: rewind + digest fused. Same strand bookkeeping as
        // Rewind, but the abandoned branch is NOT discarded: it is
        // summarized by the fast role (≤6 sentences, `focus` steers)
        // and pinned AFTER the Rewind record as a `x.ka.branch_summary`
        // Custom record. kept == 0 rewinds to the boundary — nothing
        // survives to annotate, so no summary (same rule as Rewind).
        Command::CompactFrom { turns, focus } => {
            match rewind_records(&mut ctx.voice, &mut ctx.state, &mut ctx.strand, turns).await {
                Some((kept, Some((idx, view)))) if kept > 0 && idx == kept => {
                    let summary = if view.trim().is_empty() {
                        None
                    } else {
                        record_branch_summary(
                            &mut ctx.voice,
                            &ctx.state,
                            &mut ctx.strand,
                            &ctx.events,
                            view,
                            focus.as_deref(),
                            6,
                        )
                        .await
                    };
                    let message = match summary {
                        Some(s) => {
                            // first lines only — the record carries it all
                            let head: String = s.lines().take(3).collect::<Vec<_>>().join(" · ");
                            format!("compact-from: rewound {turns} turn(s) — kept summary: {head}")
                        }
                        None => format!("compact-from: rewound {turns} turn(s)"),
                    };
                    ctx.events.send(Event::Note { message }).await.ok();
                }
                Some(_) => {
                    ctx.events
                        .send(Event::Note {
                            message: format!("compact-from: rewound {turns} turn(s)"),
                        })
                        .await
                        .ok();
                }
                None => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Protocol,
                            retryable: false,
                            message: format!("cannot rewind {turns} turn(s): not enough history"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        // SES-09: edit the context view of one message. The target must
        // be in the current window (record ids align 1:1 with the live
        // history); the edit applies live AND lands as an append-only
        // `Record::ContextEdit` — the strand record never mutates, the
        // replay fold (ka-strand) applies it at the target's position.
        Command::ContextEdit {
            target,
            new_content,
        } => {
            match ctx.state.record_ids.iter().position(|r| *r == target) {
                None => {
                    ctx.events
                        .send(Event::Note {
                            message: format!(
                                "context edit: {} is not in the current context window — nothing \
                                 changed",
                                target.0
                            ),
                        })
                        .await
                        .ok();
                }
                Some(idx) => {
                    let message = match &new_content {
                        Some(text) => {
                            ctx.voice.history[idx].content = text.clone();
                            format!(
                                "context edit: {} replaced ({} chars)",
                                target.0,
                                text.chars().count()
                            )
                        }
                        None => {
                            ctx.voice.history.remove(idx);
                            ctx.state.record_ids.remove(idx);
                            format!("context edit: {} dropped", target.0)
                        }
                    };
                    let _ = ctx.strand.append(ka_strand::Record::ContextEdit {
                        id: ka_strand::new_record_id(),
                        target: target.clone(),
                        new_content,
                    });
                    ctx.events.send(Event::Note { message }).await.ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        // SES-01: cumulative session diff — the snapshot journal's
        // pre-mutation originals vs current disk, rendered as a unified
        // diff (the workspace has no diff crate; a small Myers lives in
        // crate::diff). Full diff parked on the spill store; the note
        // carries the summary line, an 8k-capped excerpt, the pointer.
        Command::SessionDiff => {
            session_diff(&ctx.voice, &ctx.cwd, &ctx.strand, &ctx.events).await;
            ctx.events.send(Event::Idle).await.ok();
        }
        // CTX-03: side question — fast role, tools off, the transcript
        // and context untouched (role_complete builds a fresh
        // single-message request); the answer arrives as a Note.
        Command::SideQuestion { text } => {
            let message = match side_question(&mut ctx.voice, &ctx.state, &ctx.cwd, text).await {
                Some(answer) => format!("btw: {answer}"),
                None => "side question unavailable".to_string(),
            };
            ctx.events.send(Event::Note { message }).await.ok();
            ctx.events.send(Event::Idle).await.ok();
        }
        // CTX-05: goal mode — keep working toward a condition.
        Command::Goal { action } => {
            match action {
                GoalAction::Set {
                    condition,
                    budget_usd,
                } => {
                    let condition = condition.trim().to_string();
                    if condition.is_empty() {
                        ctx.events
                            .send(Event::Note {
                                message: "goal set: empty condition — nothing set".to_string(),
                            })
                            .await
                            .ok();
                    } else {
                        ctx.state.goal = Some(GoalRuntime {
                            condition: condition.clone(),
                            budget_usd,
                            paused: false,
                            spend_at_set: ctx.state.guards.session_spend,
                            turns_at_set: ctx.state.turns_seen,
                        });
                        let message = match budget_usd {
                            Some(b) => format!("goal set: {condition} (budget ${b:.2})"),
                            None => format!("goal set: {condition}"),
                        };
                        ctx.events.send(Event::Note { message }).await.ok();
                    }
                }
                GoalAction::Pause => match ctx.state.goal.as_mut() {
                    None => {
                        ctx.events
                            .send(Event::Note {
                                message: "goal paused: no goal active".to_string(),
                            })
                            .await
                            .ok();
                    }
                    Some(goal) => {
                        goal.paused = true;
                        let turns = ctx.state.turns_seen.saturating_sub(goal.turns_at_set);
                        ctx.events
                            .send(Event::Note {
                                message: format!(
                                    "goal paused ({} turn(s) since set, ${:.2} spent from the \
                                     goal budget)",
                                    turns,
                                    goal.budget_usd.unwrap_or(0.0)
                                ),
                            })
                            .await
                            .ok();
                    }
                },
                GoalAction::Resume => match ctx.state.goal.as_mut() {
                    None => {
                        ctx.events
                            .send(Event::Note {
                                message: "goal resumed: no goal active".to_string(),
                            })
                            .await
                            .ok();
                    }
                    Some(goal) => {
                        goal.paused = false;
                        ctx.events
                            .send(Event::Note {
                                message: format!("goal resumed: {}", goal.condition),
                            })
                            .await
                            .ok();
                    }
                },
                GoalAction::Drop => {
                    let had = ctx.state.goal.take().is_some();
                    ctx.events
                        .send(Event::Note {
                            message: if had {
                                "goal dropped".to_string()
                            } else {
                                "goal dropped: no goal active".to_string()
                            },
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        // CTX-06: loop mode — re-run a task on an interval. Never armed
        // in safe mode: an unattended repeat executor is a
        // customization-tier risk the bare surface pins off.
        Command::Loop { action } => {
            match action {
                LoopAction::Set {
                    task,
                    interval_secs,
                } => {
                    if crate::conventions::bare_mode() {
                        ctx.events
                            .send(Event::Note {
                                message: "loop unavailable in safe mode".to_string(),
                            })
                            .await
                            .ok();
                    } else if task.trim().is_empty() {
                        ctx.events
                            .send(Event::Note {
                                message: "loop armed: empty task — nothing armed".to_string(),
                            })
                            .await
                            .ok();
                    } else {
                        let interval = Duration::from_secs(interval_secs.unwrap_or(0));
                        ctx.state.loop_mode = Some(LoopRuntime {
                            task: task.clone(),
                            interval,
                            last_settle: std::time::Instant::now(),
                        });
                        let message = if interval.is_zero() {
                            format!("loop armed: {task}")
                        } else {
                            format!("loop armed: {task} every {}s", interval.as_secs())
                        };
                        ctx.events.send(Event::Note { message }).await.ok();
                    }
                }
                LoopAction::Off => {
                    ctx.state.loop_mode = None;
                    ctx.events
                        .send(Event::Note {
                            message: "loop off".to_string(),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        // CTX-10: memory consolidation — inbox notes + MEMORY.md → the
        // fast role produces the consolidated MEMORY.md (≤80 lines),
        // the file is REWRITTEN (consolidation, not append), the inbox
        // empties. Trust-gated (it writes), fast-role-gated, and inert
        // in safe mode like every memory tier.
        Command::MemoryConsolidate => {
            memory_consolidate(&mut ctx.voice, &ctx.state, &ctx.cwd, &ctx.events).await;
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::UndoFile => {
            let sink = ctx.voice.snapshot_sink();
            let outcome = sink.lock().undo();
            match outcome {
                Ok(Some(entry)) => {
                    let what = if entry.existed {
                        format!("restored {}", entry.path.display())
                    } else {
                        format!(
                            "removed {} (was created this session)",
                            entry.path.display()
                        )
                    };
                    ctx.events
                        .send(Event::Note {
                            message: format!("↩ {what}"),
                        })
                        .await
                        .ok();
                }
                Ok(None) => {
                    ctx.events
                        .send(Event::Note {
                            message: "↩ nothing to undo in this session".to_string(),
                        })
                        .await
                        .ok();
                }
                Err(e) => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Protocol,
                            retryable: false,
                            message: format!("undo failed: {e}"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::SaveSettings {
            model,
            effort,
            mode,
            mouse,
            features,
        } => {
            match crate::config::save_user_settings(
                model.as_deref(),
                effort,
                mode,
                mouse.as_deref(),
                features.as_deref(),
            ) {
                Ok(path) => {
                    ctx.events
                        .send(Event::Note {
                            message: format!("saved settings to {}", path.display()),
                        })
                        .await
                        .ok();
                }
                Err(e) => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Protocol,
                            retryable: false,
                            message: format!("saving settings failed: {e}"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::SaveApiKey { env_var, value } => {
            match crate::config::save_api_key(&env_var, &value) {
                Ok(path) => {
                    ctx.events
                        .send(Event::Note {
                            message: format!("{env_var} saved to {}", path.display()),
                        })
                        .await
                        .ok();
                }
                Err(e) => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Protocol,
                            retryable: false,
                            message: format!("saving {env_var} failed: {e}"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::Answer { .. } => {
            // answers are consumed inside the pending ask; one arriving
            // here means the surface answered an ask that no longer exists
            ctx.events
                .send(Event::Error {
                    class: ErrorClass::Unsupported,
                    retryable: false,
                    message: "no pending question to answer".to_string(),
                })
                .await?;
        }
        Command::ExportMarkdown { out, html } => {
            let msg_count = ctx
                .strand
                .records()
                .iter()
                .filter(|r| matches!(r, ka_strand::Record::Message { .. }))
                .count();
            if msg_count == 0 {
                ctx.events
                    .send(Event::Error {
                        class: ErrorClass::Unsupported,
                        retryable: false,
                        message: "nothing to export — session is empty".to_string(),
                    })
                    .await
                    .ok();
            } else {
                let ext = if html { "html" } else { "md" };
                let path = match out {
                    Some(p) => p,
                    None => {
                        let id = strand_id(&ctx.strand);
                        let tail: String = id
                            .chars()
                            .rev()
                            .take(8)
                            .collect::<String>()
                            .chars()
                            .rev()
                            .collect();
                        ctx.cwd.join(format!("ka-session-{tail}.{ext}"))
                    }
                };
                let md = ka_strand::render_markdown(ctx.strand.records());
                let payload = if html {
                    ka_strand::render_html(&ka_strand::title_of(ctx.strand.records()), &md)
                } else {
                    md
                };
                match std::fs::write(&path, payload) {
                    Ok(()) => {
                        ctx.events
                            .send(Event::Note {
                                message: format!(
                                    "exported {msg_count} messages → {}",
                                    path.display()
                                ),
                            })
                            .await
                            .ok();
                    }
                    Err(e) => {
                        ctx.events
                            .send(Event::Error {
                                class: ErrorClass::Internal,
                                retryable: false,
                                message: format!("export failed: {e}"),
                            })
                            .await
                            .ok();
                    }
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
    }
    Ok(())
}

/// Attach to a strand (startup or mid-session switch): open/create it,
/// replay its settings over the live state, load history into the voice,
/// mark the waypoint, and announce the session to surfaces.
async fn attach_strand(
    events: &mpsc::Sender<Event>,
    state: &mut EngineState,
    voice: &mut Voice,
    cwd: &std::path::Path,
    choice: &StrandChoice,
) -> Result<ka_strand::StrandFile, DynError> {
    let mut strand = resolve_strand(choice, cwd, &state.history)
        .map_err(|e| -> DynError { format!("strand: {e}").into() })?;
    // interrupted-turn synthesis — observable when something was recovered
    let recovered = strand.synthesize_aborted().unwrap_or(false);
    // checkpoints and guard latches are session-scoped; re-arm them from
    // the strand we just attached to
    state.checkpoints.lock().clear();
    // a steer is scoped to the strand it arrived in: replaying it into
    // the strand we are attaching to would present old operator input as
    // if it had been sent during that strand's next tool batch
    state.steers.clear();
    let session_spend = strand
        .records()
        .iter()
        .filter_map(|r| match r {
            ka_strand::Record::Usage { cost, .. } => Some(*cost),
            _ => None,
        })
        .sum();
    state.guards.reset(session_spend);
    if recovered {
        events
            .send(Event::Note {
                message: "↩ recovered interrupted turn".to_string(),
            })
            .await
            .ok();
    }
    // session settings win over whatever the engine was running with
    let settings = strand.settings().clone();
    if settings.model.is_some() {
        state.model = settings.model.clone();
    }
    if settings.effort.is_some() {
        state.effort = settings.effort;
    }
    if settings.mode.is_some() {
        state.set_live_mode(settings.mode.unwrap_or_default());
        voice.set_mode(state.mode);
    }
    voice.pathfinder_slot().write().model = state.model.clone();
    voice.set_effort(state.effort.map(|e| effort_name(e).to_string()));
    let vision = voice.selector_accepts_images(state.model.as_deref());
    let (history, ids, digest) = history_from_records(strand.records(), vision);
    voice.load_history(history, digest);
    state.record_ids = ids;
    // a fresh strand (no prior user turns) may earn an auto-title after
    // its first completed turn; resumed strands never re-title
    state.needs_title = !strand.records().iter().any(|r| {
        matches!(
            r,
            ka_strand::Record::Message {
                role: ka_strand::Role::User,
                ..
            }
        )
    });
    // CTX-08: the prewalk is owed once per strand — fresh strands only,
    // same freshness test as the auto-title
    state.prewalk_pending = state.needs_title;
    if let Some(path) = strand.path() {
        write_waypoint(cwd, path);
    }
    let id = strand
        .records()
        .first()
        .and_then(|r| match r {
            ka_strand::Record::Header { id, .. } => Some(id.0.clone()),
            _ => None,
        })
        .unwrap_or_default();
    events
        .send(Event::SessionInfo { id: id.clone() })
        .await
        .ok();
    // the snapshot journal follows the active strand (undo is session-scoped)
    voice.snapshot_sink().lock().set_strand(id.clone());
    // and so does the per-request session id gateways route by
    // (opencode-go's `x-opencode-session`); nested voices inherit it
    // through the pathfinder slot
    voice.set_session_id(Some(id.clone()));
    voice.pathfinder_slot().write().session_id = Some(id);
    if let Some(selector) = &state.model {
        events
            .send(Event::ModelChanged {
                selector: selector.clone(),
            })
            .await
            .ok();
    }
    // always announce the effective mode at bootstrap so surfaces render
    // the true state instead of an assumed default
    events
        .send(Event::ModeChanged { mode: state.mode })
        .await
        .ok();
    // same for the reasoning level: surfaces render the true state,
    // not an assumed default (models without reasoning control get Off)
    events
        .send(Event::EffortChanged {
            level: state.effort.unwrap_or(ka_protocol::Effort::Off),
        })
        .await
        .ok();
    // feature toggles: a recorded snapshot replaces the live set
    // wholesale (over the config baseline, like model/mode); a strand
    // that never recorded one keeps the current set. The list also
    // carries the positive allowlist as `allow:<entry>` strings
    // ([features::encode_snapshot]); specs that no longer parse
    // (grammar drift) are skipped with a note.
    if let Some(list) = &settings.features {
        let (spec_strs, allow) = crate::features::split_snapshot(list);
        let parsed: Vec<ka_protocol::FeatureSpec> =
            spec_strs.iter().filter_map(|s| s.parse().ok()).collect();
        let skipped: Vec<String> = spec_strs
            .iter()
            .filter(|s| s.parse::<ka_protocol::FeatureSpec>().is_err())
            .cloned()
            .collect();
        voice.restore_feature_snapshot(parsed, allow, settings.sandbox.clone());
        if !skipped.is_empty() {
            events
                .send(Event::Note {
                    message: format!(
                        "ignored unknown feature spec(s) from this session's history: {}",
                        skipped.join(", ")
                    ),
                })
                .await
                .ok();
        }
    }
    let (disabled, _, sandbox) = voice.features_snapshot();
    if !disabled.is_empty() || sandbox.is_some() {
        events
            .send(Event::FeaturesChanged { disabled, sandbox })
            .await
            .ok();
    }
    // one source of truth for titles: `ka_strand::title_of` over the
    // records. Surfaces learn the stored/heuristic title at bootstrap;
    // a live auto-title arrives as its own Title event later.
    let title = ka_strand::title_of(strand.records());
    if title != "(empty)" {
        events.send(Event::Title { title }).await.ok();
    }
    // replay resumed history so surfaces can rebuild the transcript;
    // emit unconditionally — an empty replay tells surfaces to clear
    // (fresh strands via /new rely on this to reset the transcript)
    let mut messages: Vec<ka_protocol::ReplayedMessage> = Vec::new();
    if voice.has_digest() {
        // the digest boundary replays as a divider row: everything
        // before it is carried by the summary in the system prompt
        messages.push(ka_protocol::ReplayedMessage {
            role: "digest".to_string(),
            content: String::new(),
            digest: true,
            thinking: None,
            calls: Vec::new(),
        });
    }
    messages.extend(replay_messages(&voice.history));
    events.send(Event::Replay { messages }).await.ok();
    Ok(strand)
}

/// Auto-title budget: one short fast-role call, hard-capped.
const TITLE_TIMEOUT: Duration = Duration::from_secs(10);
const TITLE_SYSTEM: &str = "You write terse conversation titles.";
/// Chars of each side fed to the title prompt.
const TITLE_SIDE_CAP: usize = 2_000;

/// Below this, a goal is not hill-climbable enough to iterate on.
const HILL_GATE: u32 = 70;
const HILL_SYSTEM: &str = "You rate coding goals for hill-climbability: whether the work has a measurable, \
     iterable objective to optimize against (a test to pass, a benchmark to move, a \
     number to climb). Reply with only the integer score 0-100.";

/// Rate the plan-mode goal's hill-climbability with the fast role and
/// push back when there is nothing to climb: agents are at their best
/// iterating against a metric, and most goals never state one (jcode's
/// hill-climbability study — mean 91/100, ~18% of goals below the
/// gate). One cheap rating per plan turn; fast role unconfigured or a
/// failed/timed-out call stays silent, and non-Done turns never rate.
async fn maybe_rate_hill_climb(
    voice: &mut Voice,
    state: &EngineState,
    events: &mpsc::Sender<Event>,
) {
    if voice.mode() != ka_protocol::Mode::Readonly || voice.last_stop() != Stop::Done {
        return;
    }
    let Some(fast) = state.roles.fast.clone() else {
        return;
    };
    let Some(goal) = voice
        .history
        .iter()
        .rev()
        .find(|m| m.role == ka_dialect::speaker::TurnRole::User)
        .map(|m| m.content.chars().take(400).collect::<String>())
    else {
        return;
    };
    let prompt = format!("<goal>\n{goal}\n</goal>");
    let Some(raw) = voice
        .role_complete(&fast, HILL_SYSTEM, &prompt, TITLE_TIMEOUT)
        .await
    else {
        return;
    };
    // first digit run in the reply is the score
    let score = raw
        .trim()
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse::<u32>()
        .unwrap_or(100);
    let message = if score < HILL_GATE {
        format!(
            "hill-climbability {score}/100 — this goal has no metric to iterate against. \
             Reframe it into a verifiable objective (a test to pass, a benchmark to move, a \
             number to climb) and wire [verify] to measure it."
        )
    } else {
        format!("hill-climbability {score}/100")
    };
    events.send(Event::Note { message }).await.ok();
}

/// After the first completed turn of a fresh strand, ask the fast role
/// (when configured) for a 3-6 word title, persist it as a
/// [`ka_strand::Record::Title`], and announce it via `Event::Title` so
/// the running surface relabels the session live. Every failure path is
/// silent: the existing first-user-message heuristic stays, and no
/// error ever reaches the transcript. One attempt per strand.
async fn maybe_auto_title(
    voice: &mut Voice,
    state: &mut EngineState,
    strand: &mut ka_strand::StrandFile,
    events: &mpsc::Sender<Event>,
) {
    if !state.needs_title {
        return;
    }
    // a completed turn has an assistant reply; without one (error/abort
    // before any text) the turn is not complete enough to title
    let Some(reply) = voice
        .history
        .iter()
        .rev()
        .find(|m| {
            m.role == ka_dialect::speaker::TurnRole::Assistant && !m.content.trim().is_empty()
        })
        .map(|m| side_view(&m.content))
    else {
        return;
    };
    // one attempt, ever — success or failure, the heuristic takes over
    state.needs_title = false;
    let Some(fast) = state.roles.fast.clone() else {
        return; // fast unconfigured: the first-user-message heuristic stays
    };
    let Some(user) = voice
        .history
        .iter()
        // engine-injected system-notes (CTX-08 prewalk) are not the
        // user's voice: titling off one would name the session after
        // the orientation brief
        .find(|m| {
            m.role == ka_dialect::speaker::TurnRole::User && !m.content.starts_with("<system-note>")
        })
        .map(|m| side_view(&m.content))
    else {
        return;
    };
    let prompt = format!(
        "Generate a 3-6 word title for this conversation. Reply with only the title.\n\n\
         <user>\n{user}\n</user>\n\n<assistant>\n{reply}\n</assistant>"
    );
    let Some(raw) = voice
        .role_complete(&fast, TITLE_SYSTEM, &prompt, TITLE_TIMEOUT)
        .await
    else {
        return; // failed/timed out: silent fallback
    };
    let title = clean_title(&raw);
    if title.is_empty() {
        return;
    }
    let _ = strand.append(ka_strand::Record::Title {
        id: ka_strand::new_record_id(),
        title: title.clone(),
    });
    events.send(Event::Title { title }).await.ok();
}

/// Cap one side of the title prompt.
fn side_view(s: &str) -> String {
    s.chars().take(TITLE_SIDE_CAP).collect()
}

/// Normalize a model-proposed title: first non-empty line, wrapping
/// quotes/markup stripped, whitespace collapsed, capped at the same 60
/// chars [`ka_strand::title_of`] uses.
fn clean_title(raw: &str) -> String {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let line = line
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '*' | '#'))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    line.chars().take(60).collect()
}

/// Test factor (engine tests only): seconds per configured idle
/// "minute", so `[context] idle_minutes = 1` gates in seconds instead
/// of a real minute. Production code reads the default (60) — only
/// `mod tests` in this file writes it.
static IDLE_UNIT_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(60);
/// Test factor (engine tests only): the tick period in seconds
/// (production: 30).
static IDLE_TICK_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(30);

fn idle_unit_secs() -> u64 {
    IDLE_UNIT_SECS.load(std::sync::atomic::Ordering::Relaxed)
}

fn idle_tick_period() -> Duration {
    Duration::from_secs(IDLE_TICK_SECS.load(std::sync::atomic::Ordering::Relaxed))
}

/// Fast-role ask that summarizes where the session stands (CTX-07).
const RECAP_SYSTEM: &str = "You write 1-2 sentence session-state recaps: what the session is working on and where \
     it stands. Reply with only the recap.";

/// The 30s settle tick, batch 7: fire due cron schedules (each one a
/// full self-prompt turn through the command channel, preceded by a
/// `cron fired: <id>` note) and surface monitor stream notes (new
/// lines, one-time process exits). Cron is NEVER armed in safe mode —
/// the hand refuses creates there and this guard keeps a stray table
/// inert. Monitor notes are pure surface work and always run.
async fn session_tick(
    ctx: &mut Ctx,
    commands: &mut mpsc::Receiver<Command>,
) -> Result<(), DynError> {
    if !crate::conventions::bare_mode() {
        for (id, prompt) in ctx.cron.take_due(std::time::Instant::now()) {
            ctx.events
                .send(Event::Note {
                    message: format!("cron fired: {id}"),
                })
                .await
                .ok();
            handle_command(
                Command::Prompt {
                    text: prompt,
                    schema: None,
                    images: Vec::new(),
                    allowed_tools: None,
                    model: None,
                    skills: Vec::new(),
                },
                commands,
                ctx,
            )
            .await?;
        }
    }
    for note in ctx.monitors.drain_notes() {
        ctx.events.send(Event::Note { message: note }).await.ok();
    }
    Ok(())
}

/// Minimum last-turn cost before idle cache warming is worth a wire
/// call (MODEL-05, ratified $0.05).
const CACHE_WARM_MIN_COST_USD: f64 = 0.05;
/// The warm touch's round-trip budget (a max_tokens=1 stream).
const CACHE_WARM_TIMEOUT_SECS: u64 = 60;

/// The idle tick (CTX-07): runs only between commands (the turn runs
/// inside the command arm, so ticks never race live work). When
/// `[context] idle_minutes` is configured and the surface has been
/// quiet that long:
/// (a) recap — with `[roles] fast`, ≤2 sentences of session state go
///     to the strand as a `x.ka.recap` Custom record (reachable from
///     the tip) and surface as a note; only when turns happened since
///     the last recap;
/// (b) compaction — when `[context] idle_compact_pct` is set and the
///     meter reached it, the standard digest runs;
/// (c) cache warming (MODEL-05) — when `[context] cache_warming =
///     "idle"`, new turns happened since the last warm, the last turn
///     cost ≥ $0.05, and the active dialect does prompt caching at
///     all, the current prefix is re-sent with max_tokens = 1 (a
///     silent cache-TTL touch; no turn events) and surfaces only as a
///     `cache warmed (saved est. $X)` note.
/// Either action resets the activity baseline: one idle action per
/// quiet span, retried only after another `idle_minutes` pass.
async fn idle_tick_work(ctx: &mut Ctx) {
    let Some(idle_minutes) = ctx.state.idle_minutes else {
        return; // idle task off
    };
    let idle_for = last_activity_for(&ctx.state);
    if idle_for < Duration::from_secs(idle_minutes * idle_unit_secs()) {
        return; // not quiet long enough yet
    }
    // (a) recap: fast role, new turns only
    if recap_due(idle_for, ctx.state.recap_minutes)
        && ctx.state.turns_seen > ctx.state.last_recap_turns
    {
        ctx.state.last_recap_turns = ctx.state.turns_seen;
        idle_recap(&mut ctx.voice, &ctx.state, &mut ctx.strand, &ctx.events).await;
    }
    // (c) MODEL-05 cache warming: silent touch, new turns only, worth
    // it only above the cost floor; the `cache != off` guard lives in
    // Voice::cache_warm_touch (a no-caching provider has no TTL to
    // extend)
    if ctx.state.cache_warming
        && ctx.state.turns_seen > ctx.state.last_warm_turns
        && ctx.state.last_turn_cost >= CACHE_WARM_MIN_COST_USD
    {
        let saved = ctx.state.last_turn_cost;
        if ctx
            .voice
            .cache_warm_touch(Duration::from_secs(CACHE_WARM_TIMEOUT_SECS))
            .await
        {
            ctx.state.last_warm_turns = ctx.state.turns_seen;
            ctx.events
                .send(Event::Note {
                    message: format!("cache warmed (saved est. ${saved:.2})"),
                })
                .await
                .ok();
        }
    }
    // (b) compaction: the standard digest, threshold-gated
    if compact_due(
        ctx.state.idle_compact_pct,
        ctx.voice.used_pct(ctx.voice.window_tokens()),
    ) {
        ctx.events
            .send(Event::Note {
                message: format!(
                    "idle compaction: context at {}% >= {}% threshold",
                    ctx.voice.used_pct(ctx.voice.window_tokens()),
                    ctx.state.idle_compact_pct.unwrap_or(0),
                ),
            })
            .await
            .ok();
        run_digest(
            &mut ctx.voice,
            &mut ctx.state,
            &mut ctx.strand,
            &ctx.events,
            None,
        )
        .await;
    }
    // one idle action per quiet span
    ctx.state.last_activity = std::time::Instant::now();
}

/// Idle duration of the current state (injectable seam for tests).
fn last_activity_for(state: &EngineState) -> Duration {
    state.last_activity.elapsed()
}

/// Pure recap gate (time injected for tests): the extra
/// `recap_minutes` wait (None = fire on the first idle tick past the
/// idle gate).
fn recap_due(idle_for: Duration, recap_minutes: Option<u64>) -> bool {
    idle_for >= Duration::from_secs(recap_minutes.unwrap_or(0) * idle_unit_secs())
}

/// Pure compaction gate (meter injected for tests); the caller has
/// already established the idle elapsed time.
fn compact_due(threshold_pct: Option<u64>, used_pct: u64) -> bool {
    threshold_pct.is_some_and(|pct| used_pct >= pct)
}

/// Generate the idle recap with the fast role and persist it as a
/// `x.ka.recap` Custom record at the strand tip. Unconfigured fast role
/// or a failed call degrades to a note — idle work never blocks.
async fn idle_recap(
    voice: &mut Voice,
    state: &EngineState,
    strand: &mut ka_strand::StrandFile,
    events: &mpsc::Sender<Event>,
) {
    let Some(fast) = state.roles.fast.clone() else {
        events
            .send(Event::Note {
                message: "idle recap unavailable: no [roles] fast configured".to_string(),
            })
            .await
            .ok();
        return;
    };
    let state_view = voice
        .history
        .iter()
        .rev()
        .find(|m| {
            m.role == ka_dialect::speaker::TurnRole::Assistant && !m.content.trim().is_empty()
        })
        .map(|m| side_view(&m.content));
    let last_user = voice
        .history
        .iter()
        .rev()
        .find(|m| m.role == ka_dialect::speaker::TurnRole::User)
        .map(|m| side_view(&m.content));
    let (Some(last_user), Some(state_view)) = (last_user, state_view) else {
        return; // nothing to recap yet
    };
    let prompt = format!(
        "Summarize the current state of this session in at most 2 sentences (what is being \
         worked on and where it stands). Reply with only the recap.\n\n\
         <last_user>\n{last_user}\n</last_user>\n\n<latest_state>\n{state_view}\n</latest_state>"
    );
    let Some(recap) = voice
        .role_complete(&fast, RECAP_SYSTEM, &prompt, TITLE_TIMEOUT)
        .await
    else {
        events
            .send(Event::Note {
                message: "idle recap unavailable: fast role failed".to_string(),
            })
            .await
            .ok();
        return;
    };
    let recap = recap.trim();
    if recap.is_empty() {
        return;
    }
    let _ = strand.append(ka_strand::Record::Custom {
        id: ka_strand::new_record_id(),
        ns: ka_strand::RECAP_NS.to_string(),
        data: serde_json::json!({
            "text": recap,
            "at": unix_now(),
            "turns": state.turns_seen,
        }),
    });
    events
        .send(Event::Note {
            message: format!("idle recap: {recap}"),
        })
        .await
        .ok();
}

/// Unix seconds now (0 when the clock is before the epoch — never fails).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// CTX-06: the loop's fire gate, consumed from the run loop (once per
/// command wakeup and once per idle tick — never re-fired in a loop:
/// a back-to-back cycle would starve the command channel, and Loop
/// Off arriving mid-turn is dropped by the voice). Fires only when
/// armed, the queued-input queue is drained, and the interval elapsed
/// since the last settle (ZERO interval = as soon as the engine
/// settles again — i.e. the next wakeup or tick). Never mid-turn:
/// both call sites run strictly between commands. The fire time
/// becomes the new settle baseline.
fn loop_prompt_due(state: &mut EngineState) -> Option<String> {
    let loop_mode = state.loop_mode.as_mut()?;
    if !state.queue.is_empty() {
        return None;
    }
    if loop_mode.last_settle.elapsed() < loop_mode.interval {
        return None;
    }
    loop_mode.last_settle = std::time::Instant::now();
    Some(loop_mode.task.clone())
}

/// CTX-06: the loop's prompt is a normal full turn (tools on), built
/// as the protocol command so strands, pokes, and settles behave
/// exactly as for a typed prompt.
fn loop_prompt(task: &str) -> Command {
    Command::Prompt {
        text: task.to_string(),
        schema: None,
        images: Vec::new(),
        allowed_tools: None,
        model: None,
        skills: Vec::new(),
    }
}

/// CTX-03: answer a side question with the fast role — tools off and
/// the transcript/context untouched (role_complete builds a fresh
/// single-message request). The prompt carries a MINIMAL session hint
/// (cwd + branch only), never the transcript. Returns the answer
/// (2k chars max), or None when the fast role is unconfigured or the
/// call fails/times out.
async fn side_question(
    voice: &mut Voice,
    state: &EngineState,
    cwd: &std::path::Path,
    text: String,
) -> Option<String> {
    let fast = state.roles.fast.clone()?;
    let branch = crate::hands::git::RepoSnapshot::branch(cwd);
    let branch = if branch.is_empty() {
        "none"
    } else {
        branch.as_str()
    };
    const SIDE_SYSTEM: &str = "You answer quick side questions about the session or codebase. \
     You have no tools; answer from the question text and the minimal session hint alone. Be \
     concise.";
    let cwd_display = cwd.display();
    let prompt = format!(
        "(session: cwd {cwd_display}, git branch {branch})\n\n<question>\n{text}\n</question>"
    );
    let answer = voice
        .role_complete(&fast, SIDE_SYSTEM, &prompt, TITLE_TIMEOUT)
        .await?;
    let trimmed = answer.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(SIDE_ANSWER_CAP).collect())
}

/// CTX-03: side-question answer cap (chars).
const SIDE_ANSWER_CAP: usize = 2_000;

/// SES-01: unified diff of every file this session mutated (the
/// snapshot journal's pre-mutation originals, first mutation per path)
/// against current disk. Model-free and git-free. The full diff parks
/// on the spill store; the note carries the summary line, an 8k-capped
/// excerpt, and the `spill://<id>` pointer.
async fn session_diff(
    voice: &Voice,
    cwd: &std::path::Path,
    strand: &ka_strand::StrandFile,
    events: &mpsc::Sender<Event>,
) {
    const DIFF_EXCERPT_CAP: usize = 8_000;
    let strand_id = strand_id(strand);
    let originals = voice.snapshot_sink().lock().session_originals(&strand_id);
    if originals.is_empty() {
        events
            .send(Event::Note {
                message: "session diff: no changes".to_string(),
            })
            .await
            .ok();
        return;
    }
    let mut full = String::new();
    let mut files = 0usize;
    let mut stat_total = crate::diff::DiffStat::default();
    for (path, blob) in originals {
        let original = match &blob {
            Some(blob) => std::fs::read_to_string(blob).unwrap_or_default(),
            None => String::new(), // created this session: no original
        };
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        let rel = path
            .strip_prefix(cwd)
            .unwrap_or(&path)
            .display()
            .to_string();
        let text = crate::diff::unified(&rel, &original, &current, 3);
        if text.is_empty() {
            continue; // mutated back to its original bytes
        }
        files += 1;
        let file_stat = crate::diff::stat(&original, &current);
        stat_total = crate::diff::DiffStat {
            added: stat_total.added + file_stat.added,
            removed: stat_total.removed + file_stat.removed,
        };
        full.push_str(&text);
    }
    if files == 0 {
        events
            .send(Event::Note {
                message: "session diff: no changes".to_string(),
            })
            .await
            .ok();
        return;
    }
    let excerpt: String = full.chars().take(DIFF_EXCERPT_CAP).collect();
    let spill = crate::hands::Spill::new();
    let message = match spill.park(&full) {
        Ok(ptr) => format!(
            "session diff: {files} file(s), {} +/- lines — full: {ptr}\n\n{excerpt}",
            stat_total.added + stat_total.removed
        ),
        Err(e) => format!(
            "session diff: {files} file(s), {} +/- lines — spilling the full diff failed ({e}); \
             excerpt only\n\n{excerpt}",
            stat_total.added + stat_total.removed
        ),
    };
    events.send(Event::Note { message }).await.ok();
}

/// CTX-08: the prewalk — one cheap bounded repository pass (depth 2,
/// .gitignore-respected, 200 entries max) turned by the fast role into
/// a ≤5-bullet orientation brief that rides the context as a leading
/// system-note (same class as the digest's <context-digest> user
/// message), so the first real prompt starts oriented. Runs once per
/// strand, before the first turn; a missing fast role or a failed call
/// degrades to a note and never blocks.
async fn run_prewalk(
    voice: &mut Voice,
    state: &EngineState,
    cwd: &std::path::Path,
    events: &mpsc::Sender<Event>,
) {
    const PREWALK_SYSTEM: &str = "You write terse repository orientation briefs. Reply with at \
     most 5 bullet points (one line each): what this project is, its layout, its build/test \
     commands, any convention the scan reveals. Reply with only the bullets.";
    const PREWALK_SCAN_CAP: usize = 6_000;
    let Some(fast) = state.roles.fast.clone() else {
        events
            .send(Event::Note {
                message: "prewalk unavailable: no [roles] fast configured".to_string(),
            })
            .await
            .ok();
        return;
    };
    let scan = prewalk_scan(cwd);
    if scan.trim().is_empty() {
        events
            .send(Event::Note {
                message: "prewalk unavailable: empty workspace scan".to_string(),
            })
            .await
            .ok();
        return;
    }
    let scan: String = scan.chars().take(PREWALK_SCAN_CAP).collect();
    let prompt = format!("<workspace-scan>\n{scan}\n</workspace-scan>");
    let Some(brief) = voice
        .role_complete(&fast, PREWALK_SYSTEM, &prompt, TITLE_TIMEOUT)
        .await
    else {
        events
            .send(Event::Note {
                message: "prewalk unavailable: fast role failed".to_string(),
            })
            .await
            .ok();
        return;
    };
    let brief = brief.trim();
    if brief.is_empty() {
        events
            .send(Event::Note {
                message: "prewalk unavailable: fast role failed".to_string(),
            })
            .await
            .ok();
        return;
    }
    // leading system-note: the brief sits at the top of the context for
    // the whole session (the strand's first user record; the auto-title
    // skips system-notes when it picks its user side)
    let note = format!(
        "<system-note>\nSession prewalk (automated repository orientation):\n{brief}\n</system-note>"
    );
    voice
        .history
        .insert(0, ka_dialect::speaker::TurnMessage::user(note));
    events
        .send(Event::Note {
            message: "prewalk ready".to_string(),
        })
        .await
        .ok();
}

/// CTX-08: the bounded walk — depth 2, .gitignore-respected (the
/// `ignore` walker), hard entry cap, hidden/venv/build dirs skipped.
/// Emits: the entry list, a per-extension count, and the heads of the
/// README + the first manifest found.
fn prewalk_scan(cwd: &std::path::Path) -> String {
    const MAX_ENTRIES: usize = 200;
    const HEAD_LINES: usize = 40;
    const MANIFESTS: [&str; 6] = [
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "pom.xml",
        "Gemfile",
    ];
    const SKIP_DIRS: [&str; 6] = [".git", "node_modules", "target", "dist", "build", ".venv"];
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for entry in ignore::WalkBuilder::new(cwd)
        .max_depth(Some(2))
        .filter_entry(|e| !SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref()))
        .build()
    {
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_some_and(|t| t.is_file()) {
            files.push(entry.into_path());
            if files.len() >= MAX_ENTRIES {
                break;
            }
        }
    }
    if files.is_empty() {
        return String::new();
    }
    let mut ext_counts: std::collections::BTreeMap<String, usize> = Default::default();
    for f in &files {
        if let Some(ext) = f.extension().and_then(|e| e.to_str()) {
            *ext_counts.entry(ext.to_string()).or_default() += 1;
        }
    }
    let read_head = |path: &std::path::Path| -> Option<String> {
        let text = std::fs::read_to_string(path).ok()?;
        let head: Vec<&str> = text.lines().take(HEAD_LINES).collect();
        (!head.is_empty()).then(|| head.join("\n"))
    };
    let readme = files
        .iter()
        .find(|f| {
            f.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("README"))
        })
        .and_then(|p| read_head(p));
    let manifest = files
        .iter()
        .find(|f| {
            f.file_name()
                .is_some_and(|n| MANIFESTS.contains(&n.to_string_lossy().as_ref()))
        })
        .and_then(|p| read_head(p));
    let mut out = String::from("workspace scan (depth 2, first 200 entries):\n");
    for f in &files {
        let rel = f.strip_prefix(cwd).unwrap_or(f).display();
        out.push_str(&format!("- {rel}\n"));
    }
    let exts = ext_counts
        .iter()
        .map(|(ext, n)| format!("{ext}: {n}"))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!("\nfile types: {exts}\n"));
    if let Some(head) = readme {
        out.push_str(&format!("\nREADME head:\n{head}\n"));
    }
    if let Some(head) = manifest {
        out.push_str(&format!("\nmanifest head:\n{head}\n"));
    }
    out
}

/// CTX-10: memory consolidation — read `.ka/memory/inbox.md` (the
/// remember hand's staged notes) plus the current MEMORY.md, let the
/// fast role produce the consolidated file (≤80 lines, enforced), then
/// REWRITE MEMORY.md (consolidation is a rewrite, not an append),
/// empty the inbox, and report the line diff. Trust-gated like every
/// write; fast-role-gated like every cheap chore; inert in safe mode.
async fn memory_consolidate(
    voice: &mut Voice,
    state: &EngineState,
    cwd: &std::path::Path,
    events: &mpsc::Sender<Event>,
) {
    const MEMORY_SYSTEM: &str = "You consolidate project memory. Produce the new MEMORY.md \
     content: durable facts, conventions, and preferences — deduplicated, plain markdown, at \
     most 80 lines. Reply with only the file content.";
    const MEMORY_SIDE_CAP: usize = 8_000;
    const MEMORY_MAX_LINES: usize = 80;
    let root = crate::project_root(cwd);
    let inbox_path = root.join(".ka/memory/inbox.md");
    let memory_path = root.join("MEMORY.md");
    if crate::conventions::bare_mode() {
        events
            .send(Event::Note {
                message: "memory consolidation unavailable in safe mode".to_string(),
            })
            .await
            .ok();
        return;
    }
    if !crate::trust::project_trusted(cwd) {
        events
            .send(Event::Note {
                message: "memory consolidate: project not trusted — nothing written".to_string(),
            })
            .await
            .ok();
        return;
    }
    let inbox = std::fs::read_to_string(&inbox_path).unwrap_or_default();
    if inbox.trim().is_empty() {
        events
            .send(Event::Note {
                message: "memory consolidate: inbox is empty — nothing to consolidate".to_string(),
            })
            .await
            .ok();
        return;
    }
    let Some(fast) = state.roles.fast.clone() else {
        events
            .send(Event::Note {
                message: "memory consolidation unavailable: no [roles] fast configured".to_string(),
            })
            .await
            .ok();
        return;
    };
    let current = std::fs::read_to_string(&memory_path).unwrap_or_default();
    let prompt = format!(
        "<current MEMORY.md>\n{}</current MEMORY.md>\n\n<inbox notes>\n{}</inbox notes>\n\nRewrite \
         MEMORY.md consolidating the inbox notes into the durable knowledge. Reply with only the \
         new file content.",
        current.chars().take(MEMORY_SIDE_CAP).collect::<String>(),
        inbox.chars().take(MEMORY_SIDE_CAP).collect::<String>(),
    );
    let Some(raw) = voice
        .role_complete(&fast, MEMORY_SYSTEM, &prompt, TITLE_TIMEOUT)
        .await
    else {
        events
            .send(Event::Note {
                message: "memory consolidation unavailable: fast role failed".to_string(),
            })
            .await
            .ok();
        return;
    };
    let mut new_text = raw.trim().to_string();
    if new_text.is_empty() {
        events
            .send(Event::Note {
                message: "memory consolidation unavailable: fast role failed".to_string(),
            })
            .await
            .ok();
        return;
    }
    // enforce the ≤80-line contract even if the model overran
    {
        let lines: Vec<&str> = new_text.lines().collect();
        if lines.len() > MEMORY_MAX_LINES {
            new_text = lines[..MEMORY_MAX_LINES].join("\n");
        }
        new_text.push('\n');
    }
    // the same safety net under edit/write: the pre-mutation file
    // parks on the snapshot journal so /undo can restore it
    {
        let sink = voice.snapshot_sink();
        let _ = sink.lock().snapshot(&memory_path);
    }
    let stat = crate::diff::stat(&current, &new_text);
    let wrote = std::fs::write(&memory_path, &new_text).is_ok();
    if !wrote {
        events
            .send(Event::Note {
                message: format!(
                    "memory consolidate failed: could not write {}",
                    memory_path.display()
                ),
            })
            .await
            .ok();
        return;
    }
    // the staged notes are now folded in: the inbox empties
    let _ = std::fs::write(&inbox_path, "");
    events
        .send(Event::Note {
            message: format!(
                "memory consolidated (+{} -{} lines)",
                stat.added, stat.removed
            ),
        })
        .await
        .ok();
}

/// SES-03 + CTX-01: rewind the live history `turns` user turns and
/// mirror it on the strand — a Boundary when the session rewinds to
/// zero (a Rewind record pointing at its own dropped anchor would
/// resurrect the first message on replay), a Rewind record otherwise.
/// Returns the kept index (pre-truncation) and the abandoned tail
/// view (the doomed branch is captured BEFORE the truncation —
/// `rewind` cuts it out of the history for good), or None when there
/// is not enough history.
async fn rewind_records(
    voice: &mut Voice,
    state: &mut EngineState,
    strand: &mut ka_strand::StrandFile,
    turns: u32,
) -> Option<(usize, Option<(usize, String)>)> {
    let tail = voice.rewind_tail(turns);
    let kept = voice.rewind(turns)?;
    if kept == 0 {
        // the FIRST turn was rewound: nothing survives.
        let _ = strand.append(ka_strand::Record::Boundary {
            id: ka_strand::new_record_id(),
        });
        state.record_ids.clear();
        // live/replay parity: Boundary clears the digest
        // on replay, so the live session drops it too —
        // else requests keep prepending a summary of the
        // content the rewind deleted
        voice.clear_digest();
    } else {
        // kept = index of the dropped user message; keep
        // everything strictly before it
        let keep_idx = kept - 1;
        let kept_from = state
            .record_ids
            .get(keep_idx)
            .cloned()
            .unwrap_or_else(ka_strand::new_record_id);
        let _ = strand.append(ka_strand::Record::Rewind {
            id: ka_strand::new_record_id(),
            kept_from,
        });
        state.record_ids.truncate(keep_idx + 1);
    }
    Some((kept, tail))
}

/// SES-03 + CTX-01: summarize the abandoned branch with the fast role
/// and append it to the strand as a `x.ka.branch_summary` Custom
/// record — it sits at the tip of the branch you keep (AFTER the
/// Rewind marker, so replay drops only conversation records past
/// `kept_from`), so the abandoned work stays discoverable. Rewind uses
/// ≤3 sentences; CompactFrom steers with `focus` and allows 6.
/// Unconfigured fast role or a failed call degrades to a note; the
/// rewind itself is never blocked. Returns the summary text.
async fn record_branch_summary(
    voice: &mut Voice,
    state: &EngineState,
    strand: &mut ka_strand::StrandFile,
    events: &mpsc::Sender<Event>,
    tail: String,
    focus: Option<&str>,
    max_sentences: usize,
) -> Option<String> {
    const BRANCH_SYSTEM: &str = "You summarize abandoned conversation branches: what the branch \
     was working on, what it accomplished, and what was left unfinished. Reply with only the \
     summary.";
    const BRANCH_TAIL_CAP: usize = 8_000;
    let Some(fast) = state.roles.fast.clone() else {
        events
            .send(Event::Note {
                message: "branch summary unavailable: no [roles] fast configured".to_string(),
            })
            .await
            .ok();
        return None;
    };
    let tail: String = tail.chars().take(BRANCH_TAIL_CAP).collect();
    let focus_line = focus
        .map(|f| format!("\nFocus the summary on: {f}"))
        .unwrap_or_default();
    let prompt = format!(
        "The conversation branch below was just rewound (discarded from the live session). \
         Summarize it in at most {max_sentences} sentences.{focus_line}\n\n{tail}"
    );
    let Some(summary) = voice
        .role_complete(&fast, BRANCH_SYSTEM, &prompt, TITLE_TIMEOUT)
        .await
    else {
        events
            .send(Event::Note {
                message: "branch summary unavailable: fast role failed".to_string(),
            })
            .await
            .ok();
        return None;
    };
    let summary = summary.trim();
    if summary.is_empty() {
        return None;
    }
    let summary = summary.to_string();
    let _ = strand.append(ka_strand::Record::Custom {
        id: ka_strand::new_record_id(),
        ns: ka_strand::BRANCH_SUMMARY_NS.to_string(),
        data: serde_json::json!({
            "text": summary,
            "at": unix_now(),
            "turns": state.turns_seen,
        }),
    });
    events
        .send(Event::Note {
            message: "branch summary saved to the strand".to_string(),
        })
        .await
        .ok();
    Some(summary)
}

/// Post-turn context maintenance: prune old tool outputs, digest while
/// the window is under pressure.
async fn settle_context(
    voice: &mut Voice,
    state: &mut EngineState,
    strand: &mut ka_strand::StrandFile,
    events: &mpsc::Sender<Event>,
) {
    let window = voice.window_tokens();
    let ratio = voice_ratio(voice);
    // the ladder: shake (deterministic stale-arg truncation) → prune
    // (spill oversized outputs) → digest (summarize + cut) later in the
    // caller if pressure still holds
    let (shaken, _) = voice.shake(crate::voice::SHAKE_ARG_CAP);
    let saved = voice.prune_tool_outputs(ratio);
    if shaken > 0 {
        events
            .send(Event::Note {
                message: format!(
                    "shaken: {shaken} stale tool call(s) had their arguments truncated"
                ),
            })
            .await
            .ok();
    }
    if std::env::var("KA_DEBUG_SETTLE").is_ok() {
        eprintln!(
            "[settle] window={window} ratio={ratio} last_context={} pressure={}",
            voice.debug_last_context(),
            voice.context_pressure(window)
        );
    }
    if saved > 0 {
        events
            .send(Event::Note {
                message: format!("pruned ~{saved} tokens of old tool output"),
            })
            .await
            .ok();
    }
    // speculative zone: pressure ≥80% but not yet tripping — fire the
    // background candidate so the next real digest is instant (always
    // reserve-based: it is a warm-up, not a trigger)
    if !voice.context_pressure(window) && voice.context_pressure_frac(window, 80) {
        voice.start_speculative(state.roles.fast.as_deref());
    }
    // CTX-12: the configured percent replaces the reserve rule when
    // set — digest once the meter reaches `autocompact_pct` of the
    // window. Unknown window never trips either trigger.
    let trigger_pct = state.autocompact_pct;
    let tripped = |v: &Voice| match trigger_pct {
        Some(pct) => v.used_pct(window) >= pct,
        None => v.context_pressure(window),
    };
    let mut digests = 0;
    while tripped(voice) && digests < 3 {
        digests += 1;
        match run_digest(voice, state, strand, events, None).await {
            DigestResult::Digested => continue,
            DigestResult::NoModel => break,
        }
    }
    // CTX-05: the goal judge runs on Done settles only, once per turn
    // (settle_context is called exactly once per completed turn, after
    // the turn fully returned — never in parallel with live work).
    if voice.last_stop() == Stop::Done {
        goal_settle(voice, state, events).await;
    }
}

/// CTX-05: the settle-time goal judge. Budget first (deterministic —
/// over the ceiling the goal auto-pauses instead of burning more),
/// then the fast-role yes/no verdict on "is the condition met with
/// what the transcript's latest work shows". Met → note + automatic
/// drop; not met → the auto-poke (goal-prefixed) keeps pushing. A
/// missing fast role or a failed verdict stays silent — the judge is
/// best-effort, never a turn blocker.
async fn goal_settle(voice: &mut Voice, state: &mut EngineState, events: &mpsc::Sender<Event>) {
    let Some(goal) = state.goal.as_ref() else {
        return;
    };
    if goal.paused {
        return;
    }
    // budget: session spend measured from Set crosses the ceiling
    if let Some(budget) = goal.budget_usd {
        let spent = state.guards.session_spend - goal.spend_at_set;
        if spent >= budget {
            if let Some(goal) = state.goal.as_mut() {
                goal.paused = true;
            }
            events
                .send(Event::Note {
                    message: format!(
                        "goal budget exhausted — spent ${spent:.2} of the ${budget:.2} ceiling; \
                         goal paused"
                    ),
                })
                .await
                .ok();
            return;
        }
    }
    let Some(fast) = state.roles.fast.clone() else {
        return;
    };
    let condition = state
        .goal
        .as_ref()
        .map(|g| g.condition.clone())
        .unwrap_or_default();
    const GOAL_JUDGE_SYSTEM: &str =
        "You judge whether a goal condition is already met. Reply with only YES or NO.";
    const GOAL_SIDE_CAP: usize = 4_000;
    let latest = voice
        .history
        .iter()
        .rev()
        .find(|m| {
            m.role == ka_dialect::speaker::TurnRole::Assistant && !m.content.trim().is_empty()
        })
        .map(|m| m.content.chars().take(GOAL_SIDE_CAP).collect::<String>());
    let Some(latest) = latest else {
        return; // nothing to judge yet
    };
    let prompt = format!(
        "<goal>\n{condition}\n</goal>\n\n<latest>\n{latest}\n</latest>\n\nIs the goal condition \
         met by this latest work? Reply YES or NO."
    );
    let Some(verdict) = voice
        .role_complete(&fast, GOAL_JUDGE_SYSTEM, &prompt, TITLE_TIMEOUT)
        .await
    else {
        return; // judge failed: the goal stays armed for the next settle
    };
    if verdict.trim().to_ascii_uppercase().starts_with("YES") {
        state.goal = None;
        events
            .send(Event::Note {
                message: format!("goal met: {condition}"),
            })
            .await
            .ok();
    }
}

async fn run_digest(
    voice: &mut Voice,
    state: &mut EngineState,
    strand: &mut ka_strand::StrandFile,
    events: &mpsc::Sender<Event>,
    focus: Option<String>,
) -> DigestResult {
    let Some(model) = voice.model_selector_cloned() else {
        events
            .send(Event::Error {
                class: ErrorClass::Unsupported,
                retryable: false,
                message: "compact needs an active model".to_string(),
            })
            .await
            .ok();
        return DigestResult::NoModel;
    };
    let _ = model;
    let ratio = voice_ratio(voice);
    // pre_compact hook (roadmap 9.2): advisory only — the digest runs
    // regardless; clean stdout may steer or note
    voice
        .lifecycle_hook(
            crate::config::HookEvent::PreCompact,
            serde_json::json!({
                "event": "pre_compact",
                "focus": focus,
            }),
            events,
        )
        .await;
    // EXT-01: post_compact carries the token estimate before/after
    let tokens_before = voice.debug_last_context();
    // a ready speculative candidate with a matching watermark skips the
    // synchronous summarize entirely
    if focus.is_none() {
        if let Some(summary) = voice.take_speculative().await {
            let _ = voice.apply_digest(summary, ratio);
            persist_delta(voice, state, strand, None);
            reinject_after_digest(voice, events).await;
            voice
                .lifecycle_hook(
                    crate::config::HookEvent::PostCompact,
                    serde_json::json!({
                        "event": "post_compact",
                        "tokens_before": tokens_before,
                        "tokens_after": voice.debug_last_context(),
                    }),
                    events,
                )
                .await;
            events
                .send(Event::Note {
                    message: "context digested (speculative)".to_string(),
                })
                .await
                .ok();
            return DigestResult::Digested;
        }
    }
    events.send(Event::DigestStarted).await.ok();
    match voice
        .summarize(focus.as_deref(), std::time::Duration::from_secs(120))
        .await
    {
        Some(summary) => {
            let kept = voice.apply_digest(summary, ratio);
            let _ = kept;
            persist_delta(voice, state, strand, None);
            reinject_after_digest(voice, events).await;
            voice
                .lifecycle_hook(
                    crate::config::HookEvent::PostCompact,
                    serde_json::json!({
                        "event": "post_compact",
                        "tokens_before": tokens_before,
                        "tokens_after": voice.debug_last_context(),
                    }),
                    events,
                )
                .await;
            events
                .send(Event::Note {
                    message: "context digested".to_string(),
                })
                .await
                .ok();
            DigestResult::Digested
        }
        None => {
            // Mechanical fallback: keep the recent tail with a truncation
            // note. Guarantees progress when the summarizer cannot fit.
            events
                .send(Event::Note {
                    message: "digest summarizer unavailable; truncating to recent tail".to_string(),
                })
                .await
                .ok();
            let summary = "Earlier conversation was truncated automatically (summarizer \
unavailable). The most recent exchange follows; re-read files you need."
                .to_string();
            voice.apply_digest(summary, ratio);
            persist_delta(voice, state, strand, None);
            reinject_after_digest(voice, events).await;
            voice
                .lifecycle_hook(
                    crate::config::HookEvent::PostCompact,
                    serde_json::json!({
                        "event": "post_compact",
                        "tokens_before": tokens_before,
                        "tokens_after": voice.debug_last_context(),
                    }),
                    events,
                )
                .await;
            DigestResult::Digested
        }
    }
}

enum DigestResult {
    Digested,
    NoModel,
}

fn voice_ratio(voice: &Voice) -> f64 {
    voice.model_ratio()
}

/// Route one prompt through the live voice when a model is configured,
#[allow(clippy::too_many_arguments)]
async fn dispatch_turn(
    commands: &mut mpsc::Receiver<Command>,
    events: &mpsc::Sender<Event>,
    state: &mut EngineState,
    voice: &mut Voice,
    text: String,
    schema: Option<serde_json::Value>,
    images: Vec<ka_dialect::ImagePart>,
    strand: &mut ka_strand::StrandFile,
    cwd: &std::path::Path,
    model_override: Option<String>,
) {
    // pre/post-turn file hooks are silenced by the `hooks` feature
    // toggle like every other hook class
    if voice.hooks_enabled() {
        match crate::fshooks::run(HookPoint::PreTurn, cwd, None).await {
            Ok(steer) => apply_steering(steer, events, state, voice, strand).await,
            Err(note) => {
                events.send(Event::Note { message: note }).await.ok();
            }
        }
    }
    let prompt_head = text.lines().next().unwrap_or("").to_string();
    let turn_model = model_override.or_else(|| state.model.clone());
    let usage = if let Some(model) = turn_model {
        // stage auto-commit to run inside the voice just before its
        // terminal TurnFinished: ACP clients break on that event, so
        // the commit (and its notes) must land first. GIT-03: the
        // `[git] commit_role` generator is cloned out of the voice at
        // staging time (the step cannot borrow the voice mid-turn) and
        // the step reads the turn's user/assistant pair off the
        // history at run time.
        voice.pre_finish = state.auto_commit.then(|| {
            let stage = std::sync::Arc::new(AutoCommitStage {
                events: events.clone(),
                cwd: cwd.to_path_buf(),
                head: prompt_head.clone(),
                generator: state
                    .roles
                    .commit
                    .as_deref()
                    .and_then(|id| voice.aux_handle(id)),
                trailers: state.commit_trailers.clone(),
            });
            parking_lot::Mutex::new(Box::new(move |voice: &Voice| {
                let stage = std::sync::Arc::clone(&stage);
                let turn = turn_pair(voice);
                let fut: std::pin::Pin<Box<dyn Future<Output = ()> + Send>> =
                    Box::pin(async move { auto_commit(&stage, turn.as_ref()).await });
                fut
            }) as crate::voice::PreFinish)
        });
        voice
            .turn(
                &model,
                text,
                commands,
                events,
                &mut state.steers,
                &mut state.queue,
                &mut state.guards,
                schema,
                images,
            )
            .await
    } else {
        let mut history = std::mem::take(&mut voice.history);
        let (usage, stop) = turn_canned(commands, events, state, &mut history, text, cwd).await;
        voice.history = history;
        // canned turns get the same stop-hook + last_stop treatment as
        // model turns
        let status = if stop == Stop::Done {
            "done"
        } else {
            "aborted"
        };
        voice.stop_hooks(events, status).await;
        usage
    };
    // session spend accounting + the persisted Usage record feed the
    // spend guard and `ka sessions` stats
    state.guards.session_spend += usage.cost;
    // MODEL-05: the cache-warm gate reads the latest turn's cost
    state.last_turn_cost = usage.cost;
    let usage_record = ka_strand::Record::Usage {
        id: ka_strand::new_record_id(),
        cost: usage.cost,
        input: usage.input,
        output: usage.output,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
    };
    persist_delta(voice, state, strand, Some(usage_record));
    if voice.hooks_enabled() {
        match crate::fshooks::run(HookPoint::PostTurn, cwd, None).await {
            Ok(steer) => apply_steering(steer, events, state, voice, strand).await,
            Err(note) => {
                events.send(Event::Note { message: note }).await.ok();
            }
        }
    }
    // overflow promotion: apply the same switch path as /model
    if let Some(selector) = voice.take_promotion() {
        state.model = Some(selector.clone());
        voice.pathfinder_slot().write().model = Some(selector.clone());
        let _ = strand.append(ka_strand::Record::Change {
            id: ka_strand::new_record_id(),
            model: Some(selector.clone()),
            effort: None,
            mode: None,
            features: None,
            sandbox: None,
        });
        events.send(Event::ModelChanged { selector }).await.ok();
    }
    // [verify] test loop (aider's auto-test): after a turn that edited
    // files, run the configured test command; a failure feeds the
    // output back to the model for ONE automatic fix round — further
    // failures surface as notes for the user
    // drain the edited list unconditionally: with no [verify] test
    // configured it would otherwise grow unbounded across the session
    let edited = voice.take_edited();
    if voice.last_stop() == Stop::Done && !state.verify_round_done {
        if let Some(test) = state.verify_test.clone() {
            if !edited.is_empty() {
                state.verify_round_done = true;
                // the test can run long: keep serving the command channel
                // so Esc/abort (and steer/queue input) still work —
                // an abort cancels the test and skips the fix round
                let test_fut = tokio::time::timeout(
                    std::time::Duration::from_secs(900),
                    tokio::process::Command::new("sh")
                        .arg("-c")
                        .arg(&test)
                        .current_dir(cwd)
                        .kill_on_drop(true)
                        .output(),
                );
                tokio::pin!(test_fut);
                let mut aborted = false;
                let ran = loop {
                    tokio::select! {
                        biased;
                        maybe = commands.recv() => match maybe {
                            None | Some(Command::Abort) => {
                                aborted = true;
                                break None;
                            }
                            Some(Command::Steer { text }) => state.steers.push(text),
                            Some(Command::Queue { text }) => state.queue.push_back(text),
                            Some(_) => {}
                        },
                        out = &mut test_fut => break Some(out),
                    }
                };
                let verdict = match ran {
                    // aborted: the cancel note goes out with the verdict
                    None => None,
                    Some(ran) => match ran {
                        Ok(Ok(out)) if out.status.success() => None,
                        Ok(Ok(out)) => {
                            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
                            text.push_str(&String::from_utf8_lossy(&out.stderr));
                            Some(format!(
                                "exit {} — {}",
                                out.status.code().unwrap_or(-1),
                                tail_chars(text.trim(), 4_000)
                            ))
                        }
                        Ok(Err(e)) => Some(format!("failed to spawn: {e}")),
                        Err(_) => Some("timed out after 15 min".to_string()),
                    },
                };
                match verdict {
                    None if aborted => {
                        events
                            .send(Event::Note {
                                message: format!("[verify] `{test}` cancelled"),
                            })
                            .await
                            .ok();
                    }
                    None => {
                        events
                            .send(Event::Note {
                                message: format!("[verify] `{test}` passed"),
                            })
                            .await
                            .ok();
                    }
                    Some(failure) => {
                        events
                            .send(Event::Note {
                                message: format!(
                                    "[verify] `{test}` failed — feeding failures back for one fix round"
                                ),
                            })
                            .await
                            .ok();
                        let fix = format!(
                            "[verify] `{test}` failed after your edits. Fix the failures without expanding scope; if a failure predates your change, say so and stop.\n\n{failure}"
                        );
                        Box::pin(dispatch_turn(
                            commands,
                            events,
                            state,
                            voice,
                            fix,
                            None,
                            Vec::new(),
                            strand,
                            cwd,
                            None,
                        ))
                        .await;
                    }
                }
            }
        }
    }
    // CTX-07 bookkeeping: a completed turn is activity, and the recap
    // gate counts turns monotonically (digest truncation can shrink
    // the history, never this counter). CTX-06: a settle also anchors
    // the loop interval.
    state.last_activity = std::time::Instant::now();
    state.turns_seen += 1;
    if let Some(loop_mode) = &mut state.loop_mode {
        loop_mode.last_settle = std::time::Instant::now();
    }
    settle_steers(state);
}

/// Settle steers that no tool-batch boundary consumed. A live steer can
/// arrive after the last batch or during the closing zero-tool-call
/// round; converting it to queued input keeps the operator text (queued
/// follow-on turns run on this strand) instead of replaying it into a
/// later turn or another strand as if it had been seen live.
fn settle_steers(state: &mut EngineState) {
    for steer in state.steers.drain(..) {
        state.queue.push_back(steer);
    }
}

/// Last `cap` characters of `text` (test output is tail-relevant).
fn tail_chars(text: &str, cap: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > cap {
        let cut: String = chars[chars.len() - cap..].iter().collect();
        format!("…{cut}")
    } else {
        text.to_string()
    }
}

/// Apply hook steering engine-side: mode reaches the engine's own
/// record, the voice's gate, surfaces (`Event::ModeChanged`), and the
/// strand (`Change` record — a steered mode survives restart like a
/// `/mode` switch); the note surfaces as a transcript row.
async fn apply_steering(
    steer: Option<crate::fshooks::Steering>,
    events: &mpsc::Sender<Event>,
    state: &mut EngineState,
    voice: &mut Voice,
    strand: &mut ka_strand::StrandFile,
) {
    let Some(s) = steer else { return };
    if let Some(mode) = s.mode {
        state.set_live_mode(mode);
        voice.set_mode(mode);
        let _ = strand.append(ka_strand::Record::Change {
            id: ka_strand::new_record_id(),
            model: None,
            effort: None,
            mode: Some(mode),
            features: None,
            sandbox: None,
        });
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
/// Copy the current strand into a new strand file truncated to drop the
/// last `turns` user turns (0 = exact copy), titled "<original> (fork)".
fn fork_strand(
    cwd: &std::path::Path,
    current: &ka_strand::StrandFile,
    turns: u32,
) -> Result<std::path::PathBuf, String> {
    let records = current.records();
    let user_idx: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            matches!(
                r,
                ka_strand::Record::Message {
                    role: ka_strand::Role::User,
                    ..
                }
            )
        })
        .map(|(i, _)| i)
        .collect();
    if turns as usize > user_idx.len() {
        return Err(format!(
            "cannot drop {turns} turn(s): session has {} user turn(s)",
            user_idx.len()
        ));
    }
    let cut = match turns {
        0 => records.len(),
        n => user_idx[user_idx.len() - n as usize],
    };
    let mut fork =
        ka_strand::StrandFile::create(cwd, repo_snapshot(cwd)).map_err(|e| e.to_string())?;
    let parent_id = records.iter().find_map(|r| match r {
        ka_strand::Record::Header { id, .. } => Some(id.0.clone()),
        _ => None,
    });
    if let Some(parent_id) = &parent_id {
        fork.set_parent(parent_id);
    }
    for record in &records[..cut] {
        if matches!(record, ka_strand::Record::Header { .. }) {
            continue;
        }
        fork.append(record.clone()).map_err(|e| e.to_string())?;
    }
    let title = format!("{} (fork)", ka_strand::title_of(records));
    fork.append(ka_strand::Record::Title {
        id: ka_strand::new_record_id(),
        title,
    })
    .map_err(|e| e.to_string())?;
    fork.path()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| "fork was never materialized".to_string())
}

/// Create the `/transfer` handoff strand: a fresh child file whose
/// only content record is the digest — the summary is the new
/// session's entire carried memory. Mirrors [`fork_strand`] minus the
/// record copy.
fn transfer_strand(
    cwd: &std::path::Path,
    current: &ka_strand::StrandFile,
    summary: &str,
) -> Result<std::path::PathBuf, String> {
    let records = current.records();
    let mut fresh =
        ka_strand::StrandFile::create(cwd, repo_snapshot(cwd)).map_err(|e| e.to_string())?;
    let parent_id = records.iter().find_map(|r| match r {
        ka_strand::Record::Header { id, .. } => Some(id.0.clone()),
        _ => None,
    });
    if let Some(parent_id) = &parent_id {
        fresh.set_parent(parent_id);
    }
    fresh
        .append(ka_strand::Record::Digest {
            id: ka_strand::new_record_id(),
            summary: summary.to_string(),
            // no kept history rides over — the digest is everything
            kept_from: ka_strand::new_record_id(),
        })
        .map_err(|e| e.to_string())?;
    let title = format!("{} (transfer)", ka_strand::title_of(records));
    fresh
        .append(ka_strand::Record::Title {
            id: ka_strand::new_record_id(),
            title,
        })
        .map_err(|e| e.to_string())?;
    fresh
        .path()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| "transfer strand was never materialized".to_string())
}

/// Pose a question and wait for the surface's answer. `None` = the
/// surface aborted or went away.
async fn engine_ask(
    events: &mpsc::Sender<Event>,
    commands: &mut mpsc::Receiver<Command>,
    id: AskId,
    question: String,
    options: Vec<String>,
) -> Option<usize> {
    events
        .send(Event::Ask {
            id: id.clone(),
            questions: vec![AskQuestion {
                text: question,
                options,
                detail: None,
            }],
        })
        .await
        .ok();
    loop {
        tokio::select! {
            maybe = commands.recv() => {
                match maybe {
                    None => return None,
                    Some(Command::Abort) => return None,
                    Some(Command::Answer { question: q, choice }) if q == id => {
                        return Some(choice);
                    }
                    // answered asks own the channel until they resolve:
                    // other verbs cannot be replayed here, so
                    // acknowledge instead of vanishing silently
                    Some(_cmd) => {
                        events
                            .send(Event::Note {
                                message: "command ignored while an ask is pending — \
                                          resend it after answering"
                                    .to_string(),
                            })
                            .await
                            .ok();
                    }
                }
            }
        }
    }
}

/// The ladder's last step: re-read the ≤5 ledger-hot files into context
/// (bounded per file), so a digest doesn't summarise away the concrete
/// contents the model was just working with. In-memory only — merged
/// into the trailing user message, never persisted as new records.
async fn reinject_after_digest(voice: &mut Voice, events: &mpsc::Sender<Event>) {
    let files = voice.reinject_hot_files(5, 6144);
    if !files.is_empty() {
        events
            .send(Event::Note {
                message: format!(
                    "re-read {} ledger-hot file(s) into context after the digest",
                    files.len()
                ),
            })
            .await
            .ok();
    }
}

/// Persist `first` (the turn's Usage record), any pending digest (as
/// a Digest record) and the history delta, in that order and in one
/// visit to the strand file.
fn persist_delta(
    voice: &mut Voice,
    state: &mut EngineState,
    strand: &mut ka_strand::StrandFile,
    first: Option<ka_strand::Record>,
) {
    let digest = voice.take_pending_digest().map(|(summary, kept, _rev)| {
        let kept_from = state
            .record_ids
            .get(kept)
            .cloned()
            .unwrap_or_else(ka_strand::new_record_id);
        (
            ka_strand::Record::Digest {
                id: ka_strand::new_record_id(),
                summary,
                kept_from,
            },
            kept,
        )
    });
    // the history up to the first message without a record id
    let mut delta = Vec::new();
    while state.record_ids.len() + delta.len() < voice.history.len() {
        let record = record_from_message(&voice.history[state.record_ids.len() + delta.len()]);
        if record.id().is_none() {
            break;
        }
        delta.push(record);
    }
    let records: Vec<ka_strand::Record> = first
        .into_iter()
        .chain(digest.iter().map(|(record, _)| record.clone()))
        .chain(delta.iter().cloned())
        .collect();
    let _ = strand.append_all(records);
    if let Some((_, kept)) = digest {
        state.record_ids = state.record_ids.get(kept..).unwrap_or(&[]).to_vec();
    }
    state
        .record_ids
        .extend(delta.iter().filter_map(|record| record.id().cloned()));
}

/// The strand's own id from its header record.
fn strand_id(strand: &ka_strand::StrandFile) -> String {
    strand
        .records()
        .first()
        .and_then(|r| match r {
            ka_strand::Record::Header { id, .. } => Some(id.0.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The `[lsp]` tier as the engine sees it: the whole tier — spawned
/// servers, navigation hands, write-through — is inert in bare mode,
/// because starting user-configured executables is the same risk class
/// as hooks/MCP (which bare mode also darkens).
pub fn effective_lsp_cfg(config: &crate::config::Config) -> crate::config::Lsp {
    if crate::conventions::bare_mode() {
        crate::config::Lsp::default()
    } else {
        config.lsp.clone()
    }
}

/// The hands the LSP tier contributes once the tier is enabled:
/// navigation always, write-through (`lsp_rename` / `lsp_actions`) only
/// under `[lsp] write_through = true` — ka acts through the server, but
/// still on the ledger/write path.
pub fn lsp_hands(
    lsp: std::sync::Arc<crate::lsp::LspManager>,
    cfg: &crate::config::Lsp,
) -> Vec<std::sync::Arc<dyn crate::hands::Hand>> {
    let mut hands = crate::hands::lsp_tools::hands(lsp.clone());
    if cfg.write_through == Some(true) {
        hands.extend(crate::hands::lsp_write::hands(lsp));
    }
    hands
}

/// The `[debug]` tier as the engine sees it: spawning debug adapters is
/// a customization tier (user-configured executables) — inert in bare
/// mode like `[lsp]`.
pub fn effective_debug_cfg(config: &crate::config::Config) -> crate::config::Debug {
    if crate::conventions::bare_mode() {
        crate::config::Debug::default()
    } else {
        config.debug.clone()
    }
}

/// Waypoint: tiny per-terminal pointer so `ka -c` continues the right
/// strand per pane. Best-effort.
fn write_waypoint(cwd: &std::path::Path, strand_path: &std::path::Path) {
    let Some(key) = tty_key() else { return };
    let dir = ka_strand::data_dir().join("waypoints");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = std::fs::write(
        dir.join(key),
        format!("{}\n{}", cwd.display(), strand_path.display()),
    );
}

/// Read this terminal's waypoint (cwd + strand path), if any.
pub fn read_waypoint() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let key = tty_key()?;
    let text = std::fs::read_to_string(ka_strand::data_dir().join("waypoints").join(key)).ok()?;
    let mut lines = text.lines();
    let cwd = lines.next()?.to_string();
    let path = lines.next()?.to_string();
    Some((
        std::path::PathBuf::from(cwd),
        std::path::PathBuf::from(path),
    ))
}

fn tty_key() -> Option<String> {
    if let Ok(explicit) = std::env::var("KA_TTY") {
        return Some(format!("{}-{}", explicit.len(), explicit.replace('/', "_")));
    }
    // Windows has no /proc: key by Windows Terminal's per-window GUID
    // when present, else one coarse per-console key. `ka -c` still
    // verifies the waypoint's cwd before resuming, so a shared key can
    // only ever resume this directory's most recently attached strand.
    if cfg!(windows) {
        return Some(match std::env::var("WT_SESSION") {
            Ok(guid) => format!("wt-{}", guid.replace(['\\', '/'], "_")),
            Err(_) => "console".to_string(),
        });
    }
    let link = std::fs::read_link("/proc/self/fd/0").ok()?;
    let name = link.to_string_lossy();
    if name.starts_with("/dev/") && name.contains("tty") {
        Some(format!("fd-{}", name.replace('/', "_")))
    } else {
        None
    }
}

/// One canned turn: stream paced chunks, honoring aborts that arrive
/// mid-stream. Used when no model is configured. Returns the turn's
/// (estimated) usage.
async fn turn_canned(
    commands: &mut mpsc::Receiver<Command>,
    events: &mpsc::Sender<Event>,
    state: &mut EngineState,
    history: &mut Vec<ka_dialect::speaker::TurnMessage>,
    text: String,
    cwd: &std::path::Path,
) -> (Usage, Stop) {
    let est_in = (text.len() as u64).div_ceil(4);
    events
        .send(Event::TurnStarted {
            context: ContextMeter {
                used: est_in,
                window: 0,
            },
        })
        .await
        .ok();

    history.push(ka_dialect::speaker::TurnMessage::user(text.clone()));
    let chunks = canned::reply(&text);
    let mut idx = 0;
    let mut aborted = false;
    while idx < chunks.len() {
        tokio::select! {
            biased;
            maybe = commands.recv() => {
                match maybe {
                    None => {
                        // Surface went away; settle as aborted so the
                        // terminal event still fires and stop hooks run.
                        events
                            .send(Event::TurnFinished {
                                stop: Stop::Aborted,
                                usage: Usage::default(),
                            })
                            .await
                            .ok();
                        return (Usage::default(), Stop::Aborted);
                    }
                    Some(Command::Abort) => {
                        aborted = true;
                        break;
                    }
                    Some(other) => {
                        // canned path: only queue-side effects are safe here
                        match other {
                            Command::Steer { text } => state.steers.push(text),
                            Command::Queue { text } => state.queue.push_back(text),
                            Command::Abort => {}
                            _ => {}
                        }
                    }
                }
            }
            () = tokio::time::sleep(Duration::from_millis(20)) => {
                events
                    .send(Event::Delta { kind: DeltaKind::Text(chunks[idx].clone()) })
                    .await
                    .ok();
                idx += 1;
            }
        }
    }

    if aborted {
        events
            .send(Event::TurnFinished {
                stop: Stop::Aborted,
                usage: Usage::default(),
            })
            .await
            .ok();
        return (Usage::default(), Stop::Aborted);
    }

    let est_out: u64 = chunks
        .iter()
        .map(|c: &String| c.len() as u64)
        .sum::<u64>()
        .div_ceil(4);
    history.push(ka_dialect::speaker::TurnMessage::assistant(chunks.concat()));
    if state.auto_commit {
        // no voice on the canned path (no model configured): the
        // commit-role generator never resolves here, so the subject is
        // the deterministic prompt line — trailers still apply
        let stage = AutoCommitStage {
            events: events.clone(),
            cwd: cwd.to_path_buf(),
            head: text.lines().next().unwrap_or("").to_string(),
            generator: None,
            trailers: state.commit_trailers.clone(),
        };
        auto_commit(&stage, None).await;
    }
    let usage = Usage {
        input: est_in,
        output: est_out,
        ..Usage::default()
    };
    events
        .send(Event::TurnFinished {
            stop: Stop::Done,
            usage,
        })
        .await
        .ok();
    (usage, Stop::Done)
}

/// Everything one auto-commit needs, staged by the engine at dispatch
/// (and rebuilt per canned turn): the commit cannot borrow the turn
/// state, so the decision-relevant bits are cloned out up front.
struct AutoCommitStage {
    events: mpsc::Sender<Event>,
    cwd: std::path::PathBuf,
    /// First prompt line — the deterministic fallback subject.
    head: String,
    /// GIT-03 `[git] commit_role`: the resolved generator handle.
    /// `None` (unset, or failed startup resolution) keeps today's
    /// first-prompt-line subject, silently.
    generator: Option<crate::voice::AuxRole>,
    /// GIT-03 `[git] trailers`: appended under the subject.
    trailers: Vec<String>,
}

/// This turn's last user prompt + assistant reply, straight off the
/// voice history at pre-finish time (GIT-03 feeds the pair to the
/// commit-subject role). Aborted turns never reach the staged step.
fn turn_pair(voice: &Voice) -> Option<(String, String)> {
    let mut user = None;
    let mut assistant = None;
    for msg in voice.history.iter().rev() {
        match msg.role {
            ka_dialect::speaker::TurnRole::User if user.is_none() => {
                user = Some(msg.content.clone())
            }
            ka_dialect::speaker::TurnRole::Assistant if assistant.is_none() => {
                assistant = Some(msg.content.clone())
            }
            _ => {}
        }
        if user.is_some() && assistant.is_some() {
            break;
        }
    }
    Some((user?, assistant?))
}

/// GIT-03: the commit-subject role's instructions — one line, no
/// quotes, no prefix (the deterministic `ka: ` prefix is added here,
/// so the subject reads like every other auto-commit).
const COMMIT_SYSTEM: &str = "You write git commit subjects for a coding agent's staged work: \
one line, at most 72 characters, imperative mood, no quotes, no markdown, no prefix. Reply \
with only the subject line.";
/// Conversation chars fed to the subject role (user + assistant side).
const COMMIT_TALK_CAP: usize = 4_000;
/// Chars of the staged `diff --stat` fed to the subject role.
const COMMIT_STAT_CAP: usize = 2_000;

/// One best-effort subject from the commit role: staged diff-stat plus
/// the turn's user/assistant pair (or the prompt line when the pair is
/// unavailable). Any failure → None → the caller falls back silently.
async fn generated_subject(
    generator: &crate::voice::AuxRole,
    diff_stat: &str,
    turn: Option<&(String, String)>,
    head: &str,
) -> Option<String> {
    let convo = match turn {
        Some((user, assistant)) => {
            format!(
                "User request:\n{}\nAssistant result:\n{}",
                user.trim(),
                assistant.trim()
            )
        }
        None => format!("User request:\n{}", head.trim()),
    };
    let convo: String = convo.chars().take(COMMIT_TALK_CAP).collect();
    let stat: String = diff_stat.chars().take(COMMIT_STAT_CAP).collect();
    let prompt = format!(
        "Write the git commit subject for these staged changes.\n\nStaged diff stat:\n{stat}\n\n{convo}"
    );
    let reply = generator
        .complete(COMMIT_SYSTEM, &prompt, TITLE_TIMEOUT)
        .await?;
    reply
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// `[git] auto_commit`: stage the whole worktree (`git add -A` —
/// untracked files and anything already staged included) and commit.
/// Subject: the `[git] commit_role`'s one-liner over the staged diff
/// stat + the turn's messages (GIT-03), falling back silently to the
/// deterministic `ka: <first prompt line>` (72 chars max, no model
/// roundtrip) when the role is unset, unresolved, or fails. `[git]
/// trailers` append under the subject. Outside a repo: silent no-op.
/// Any git failure is an advisory note, never a turn failure.
async fn auto_commit(stage: &AutoCommitStage, turn: Option<&(String, String)>) {
    let git = |args: &[&str]| {
        tokio::process::Command::new("git")
            .args(args)
            .current_dir(&stage.cwd)
            .output()
    };
    let events = &stage.events;
    match git(&["rev-parse", "--is-inside-work-tree"]).await {
        Ok(out) if String::from_utf8_lossy(&out.stdout).trim() == "true" => {}
        _ => return,
    }
    let dirty = git(&["status", "--porcelain"])
        .await
        .map(|out| !out.stdout.is_empty())
        .unwrap_or(false);
    if !dirty {
        return;
    }
    match git(&["add", "-A"]).await {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            events
                .send(Event::Note {
                    message: format!("auto-commit: git add failed: {}", err.trim()),
                })
                .await
                .ok();
            return;
        }
        Err(e) => {
            events
                .send(Event::Note {
                    message: format!("auto-commit: {e}"),
                })
                .await
                .ok();
            return;
        }
    }
    let first_line: String = stage
        .head
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(72)
        .collect();
    let fallback: String = format!("ka: {first_line}").chars().take(72).collect();
    let msg: String = match &stage.generator {
        Some(generator) => {
            let diff_stat = git(&["diff", "--cached", "--stat"])
                .await
                .map(|out| String::from_utf8_lossy(&out.stdout).to_string())
                .unwrap_or_default();
            generated_subject(generator, &diff_stat, turn, &stage.head)
                .await
                .map(|subject| format!("ka: {subject}").chars().take(72).collect())
                .unwrap_or(fallback)
        }
        None => fallback,
    };
    // trailers ride a second `-m` paragraph so the subject line stays
    // clean (`git log --format=%s` unchanged for trailer-less commits)
    let body = stage
        .trailers
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let mut commit_args: Vec<&str> = vec![
        "-c",
        "user.name=ka",
        "-c",
        "user.email=ka@local",
        "commit",
        "-m",
        &msg,
    ];
    if !body.is_empty() {
        commit_args.extend(["-m", body.as_str()]);
    }
    match git(&commit_args).await {
        Ok(out) if out.status.success() => {
            let id = git(&["rev-parse", "--short", "HEAD"])
                .await
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            events
                .send(Event::Note {
                    message: format!("auto-commit {id}: {msg}"),
                })
                .await
                .ok();
        }
        Ok(out) => {
            let tail = String::from_utf8_lossy(&out.stderr)
                .lines()
                .last()
                .unwrap_or("git commit failed")
                .to_string();
            events
                .send(Event::Note {
                    message: format!("auto-commit failed: {tail}"),
                })
                .await
                .ok();
        }
        Err(e) => {
            events
                .send(Event::Note {
                    message: format!("auto-commit: {e}"),
                })
                .await
                .ok();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ka_dialect::speaker::{SpeakFuture, SpeakRequest, Speaker, StreamEvent, ToolCall};
    use ka_protocol::{Command, ErrorClass, Event, Stop, Usage};
    use std::time::Duration;
    use tokio::sync::mpsc;

    use crate::config::Config;
    use crate::engine::{
        EngineHandle, StrandChoice, commit_role_selector, spawn, spawn_with_speaker,
        spawn_with_team_speakers,
    };

    async fn drain_until_finished(events: &mut mpsc::Receiver<Event>) -> Vec<Event> {
        let mut seen = Vec::new();
        while let Some(evt) = events.recv().await {
            let is_finished = matches!(evt, Event::TurnFinished { .. });
            seen.push(evt);
            if is_finished {
                break;
            }
        }
        seen
    }

    #[tokio::test]
    async fn prompt_streams_deltas_then_done() {
        let mut handle = spawn(Config::default());
        handle
            .commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "hi".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let seen = drain_until_finished(&mut handle.events).await;
        let deltas = seen
            .iter()
            .filter(|e| matches!(e, Event::Delta { .. }))
            .count();
        assert_eq!(deltas, 3);
        match seen.last().unwrap() {
            Event::TurnFinished {
                stop: Stop::Done,
                usage,
            } => {
                assert_eq!(usage.input, 1); // "hi" → ceil(2/4)
            }
            other => panic!("expected TurnFinished, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn abort_mid_turn_finishes_aborted() {
        let mut handle = spawn(Config::default());
        handle
            .commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "slow".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        handle.commands.send(Command::Abort).await.unwrap();
        let seen = drain_until_finished(&mut handle.events).await;
        match seen.last().unwrap() {
            Event::TurnFinished {
                stop: Stop::Aborted,
                ..
            } => {}
            other => panic!("expected aborted finish, got {other:?}"),
        }
    }

    fn git_in(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
    }

    /// A git repo with one baseline commit plus unstaged tracked and
    /// untracked changes.
    fn dirty_repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-autocmt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(git_in(&dir, &["init"]).status.success());
        git_in(&dir, &["config", "user.email", "t@t"]);
        git_in(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
        git_in(&dir, &["add", "."]);
        assert!(git_in(&dir, &["commit", "-m", "init"]).status.success());
        std::fs::write(dir.join("tracked.txt"), "two\n").unwrap();
        std::fs::write(dir.join("extra.txt"), "new\n").unwrap();
        dir
    }

    fn head_count(dir: &std::path::Path) -> usize {
        String::from_utf8_lossy(&git_in(dir, &["rev-list", "--count", "HEAD"]).stdout)
            .trim()
            .parse()
            .unwrap()
    }

    #[tokio::test]
    async fn auto_commit_makes_exactly_one_ka_commit_per_turn() {
        let dir = dirty_repo("on");
        let mut handle = spawn(Config {
            cwd: Some(dir.display().to_string()),
            git: crate::config::Git {
                auto_commit: Some(true),
                ..Default::default()
            },
            ..Default::default()
        });
        handle
            .commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "hello".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let seen = drain_until_finished(&mut handle.events).await;
        assert!(matches!(
            seen.last(),
            Some(Event::TurnFinished {
                stop: Stop::Done,
                ..
            })
        ));
        assert_eq!(head_count(&dir), 2, "exactly one new commit");
        let msg = String::from_utf8_lossy(&git_in(&dir, &["log", "-1", "--format=%s"]).stdout)
            .trim()
            .to_string();
        assert_eq!(msg, "ka: hello");
        let status = git_in(&dir, &["status", "--porcelain"]);
        let status_text = String::from_utf8_lossy(&status.stdout);
        let dirty: Vec<&str> = status_text
            .lines()
            .filter(|l| !l.contains(".ka/"))
            .collect();
        assert!(dirty.is_empty(), "dirty outside .ka/: {dirty:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn auto_commit_off_by_default_leaves_the_repo_untouched() {
        let dir = dirty_repo("off");
        let mut handle = spawn(Config {
            cwd: Some(dir.display().to_string()),
            ..Default::default()
        });
        handle
            .commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "hello".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        drain_until_finished(&mut handle.events).await;
        assert_eq!(head_count(&dir), 1, "no commit without the flag");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn auto_commit_outside_a_repo_is_a_silent_noop() {
        let dir = std::env::temp_dir().join(format!("ka-autocmt-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut handle = spawn(Config {
            cwd: Some(dir.display().to_string()),
            git: crate::config::Git {
                auto_commit: Some(true),
                ..Default::default()
            },
            ..Default::default()
        });
        handle
            .commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "hello".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let seen = drain_until_finished(&mut handle.events).await;
        assert!(matches!(
            seen.last(),
            Some(Event::TurnFinished {
                stop: Stop::Done,
                ..
            })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------ GIT-03

    /// GIT-03 fixture: one speaker for the whole session — the main
    /// turn, the commit-subject role, and the auto-title role —
    /// discriminated by system prompt, recording every request.
    struct CommitSpeaker {
        requests: std::sync::Arc<parking_lot::Mutex<Vec<SpeakRequest>>>,
    }
    impl Speaker for CommitSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            let system = req.system.clone();
            self.requests.lock().push(req);
            Box::pin(async move {
                let reply = if system.contains("commit subjects") {
                    "fix the parser bug"
                } else if system.contains("conversation titles") {
                    "Session title"
                } else {
                    "I rewrote the parser loop"
                };
                out.send(StreamEvent::Text(reply.into())).await.ok();
                out.send(StreamEvent::Finished {
                    stop: Stop::Done,
                    usage: Usage::default(),
                })
                .await
                .ok();
            })
        }
    }

    fn commit_speaker() -> (
        std::sync::Arc<CommitSpeaker>,
        std::sync::Arc<parking_lot::Mutex<Vec<SpeakRequest>>>,
    ) {
        let requests = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        (
            std::sync::Arc::new(CommitSpeaker {
                requests: requests.clone(),
            }),
            requests,
        )
    }

    fn test_catalog() -> ka_dialect::Catalog {
        ka_dialect::Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn auto_commit_commit_role_generates_the_subject_and_appends_trailers() {
        let dir = dirty_repo("role");
        let cfg = Config {
            cwd: Some(dir.display().to_string()),
            model: Some("test/m".into()),
            roles: crate::config::Roles {
                default: None,
                fast: Some("test/m".into()),
                extra: Default::default(),
            },
            git: crate::config::Git {
                auto_commit: Some(true),
                // a [roles] NAME resolves through the table
                commit_role: Some("fast".into()),
                trailers: vec!["Assisted-by: ka".into()],
            },
            ..Default::default()
        };
        let (speaker, requests) = commit_speaker();
        let mut handle = spawn_with_speaker(
            cfg,
            test_catalog(),
            StrandChoice::New,
            ka_dialect::Wire::OpenaiChat,
            speaker,
        );
        handle
            .commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "fix the parser".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let seen = drain_until_finished(&mut handle.events).await;
        assert!(matches!(
            seen.last(),
            Some(Event::TurnFinished {
                stop: Stop::Done,
                ..
            })
        ));
        assert_eq!(head_count(&dir), 2, "exactly one new commit");
        let subject = String::from_utf8_lossy(&git_in(&dir, &["log", "-1", "--format=%s"]).stdout)
            .trim()
            .to_string();
        assert_eq!(subject, "ka: fix the parser bug");
        let body = String::from_utf8_lossy(&git_in(&dir, &["log", "-1", "--format=%b"]).stdout)
            .trim()
            .to_string();
        assert_eq!(body, "Assisted-by: ka", "trailers must be appended");
        // the generator saw the staged diff stat + the turn's two sides
        let commit_req = requests
            .lock()
            .iter()
            .find(|r| r.system.contains("commit subjects"))
            .expect("commit role request")
            .clone();
        let prompt = commit_req
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            prompt.contains("tracked.txt"),
            "diff stat must ride the prompt: {prompt:?}"
        );
        assert!(
            prompt.contains("fix the parser"),
            "user side must ride the prompt: {prompt:?}"
        );
        assert!(
            prompt.contains("I rewrote the parser loop"),
            "assistant side must ride the prompt: {prompt:?}"
        );
        // the fallback stays silent: no role-failure note
        assert!(
            !seen.iter().any(|e| matches!(e, Event::Note { message }
                if message.contains("commit role") || message.contains("fallback"))),
            "generation must not chatter: {seen:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn auto_commit_unresolved_role_falls_back_silently_with_trailers() {
        let dir = dirty_repo("fallback");
        let cfg = Config {
            cwd: Some(dir.display().to_string()),
            model: Some("test/m".into()),
            git: crate::config::Git {
                auto_commit: Some(true),
                // not in the catalog → no generator, no note
                commit_role: Some("test/missing".into()),
                trailers: vec!["Assisted-by: ka".into(), "   ".into()],
            },
            ..Default::default()
        };
        let (speaker, requests) = commit_speaker();
        let mut handle = spawn_with_speaker(
            cfg,
            test_catalog(),
            StrandChoice::New,
            ka_dialect::Wire::OpenaiChat,
            speaker,
        );
        handle
            .commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "hello".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let seen = drain_until_finished(&mut handle.events).await;
        assert!(matches!(
            seen.last(),
            Some(Event::TurnFinished {
                stop: Stop::Done,
                ..
            })
        ));
        assert_eq!(head_count(&dir), 2, "exactly one new commit");
        let subject = String::from_utf8_lossy(&git_in(&dir, &["log", "-1", "--format=%s"]).stdout)
            .trim()
            .to_string();
        assert_eq!(subject, "ka: hello", "fallback keeps the prompt line");
        let body = String::from_utf8_lossy(&git_in(&dir, &["log", "-1", "--format=%b"]).stdout)
            .trim()
            .to_string();
        // the blank trailer line is dropped, the real one stays
        assert_eq!(body, "Assisted-by: ka");
        // silence: no request ever reached a commit-subject role and no
        // note blamed the missing role
        assert!(
            !requests
                .lock()
                .iter()
                .any(|r| r.system.contains("commit subjects")),
            "unresolved role must not fire"
        );
        assert!(
            !seen.iter().any(|e| matches!(e, Event::Note { message }
                if message.contains("commit role") || message.contains("fallback"))),
            "fallback must be silent: {seen:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_role_selector_maps_names_and_keeps_literals() {
        let roles = crate::config::Roles {
            default: Some("vendor/main".into()),
            fast: Some("vendor/fast@low".into()),
            extra: [("review".to_string(), "vendor/r".to_string())]
                .into_iter()
                .collect(),
        };
        assert_eq!(commit_role_selector(&roles, "fast"), "vendor/fast@low");
        assert_eq!(commit_role_selector(&roles, "@fast"), "vendor/fast@low");
        assert_eq!(commit_role_selector(&roles, "review"), "vendor/r");
        assert_eq!(commit_role_selector(&roles, "@review"), "vendor/r");
        assert_eq!(commit_role_selector(&roles, "default"), "vendor/main");
        // anything else stays a literal selector (unknown names fail
        // catalog resolution downstream → silent first-line fallback)
        assert_eq!(
            commit_role_selector(&roles, "vendor/other@high"),
            "vendor/other@high"
        );
        assert_eq!(commit_role_selector(&roles, "@nowhere"), "@nowhere");
    }

    #[tokio::test]
    async fn queued_input_triggers_follow_on_turns() {
        let mut handle = spawn(Config::default());
        handle
            .commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "one".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        handle
            .commands
            .send(Command::Queue { text: "two".into() })
            .await
            .unwrap();
        let first = drain_until_finished(&mut handle.events).await;
        assert!(matches!(
            first.last().unwrap(),
            Event::TurnFinished {
                stop: Stop::Done,
                ..
            }
        ));
        let second = drain_until_finished(&mut handle.events).await;
        assert!(matches!(second.first().unwrap(), Event::TurnStarted { .. }));
        assert!(matches!(
            second.last().unwrap(),
            Event::TurnFinished {
                stop: Stop::Done,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn strand_persists_and_resumes() {
        use crate::engine::{StrandChoice, spawn_full};
        use ka_protocol::Command;

        let data = std::env::temp_dir().join(format!("ka-eng-strand-{}", std::process::id()));
        let work = data.join("work");
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&work).unwrap();
        // thread-local data dir applies to engine tasks on this runtime
        ka_strand::set_data_dir_for_tests(data.clone());

        let cfg = Config {
            cwd: Some(work.display().to_string()),
            ..Default::default()
        };
        let mut h1 = spawn_full(
            cfg.clone(),
            ka_dialect::Catalog::embedded(),
            StrandChoice::New,
        );
        h1.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "hello there".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        while let Some(evt) = h1.events.recv().await {
            if matches!(evt, Event::TurnFinished { .. }) {
                break;
            }
        }
        drop(h1);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // strand exists with the conversation
        let summaries = ka_strand::list(&work).unwrap();
        assert_eq!(summaries.len(), 1, "{summaries:?}");
        let records = ka_strand::read(&summaries[0].path).unwrap();
        let user = records.iter().find_map(|r| match r {
            ka_strand::Record::Message {
                role: ka_strand::Role::User,
                content,
                ..
            } => Some(content.clone()),
            _ => None,
        });
        assert_eq!(user.as_deref(), Some("hello there"));
        assert_eq!(
            summaries[0].title, "hello there",
            "title = first user message"
        );

        // resume: continue the same strand with a second prompt
        let mut h2 = spawn_full(cfg, ka_dialect::Catalog::embedded(), StrandChoice::Latest);
        h2.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "second question".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        while let Some(evt) = h2.events.recv().await {
            if matches!(evt, Event::TurnFinished { .. }) {
                break;
            }
        }
        drop(h2);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let after = ka_strand::list(&work).unwrap();
        assert_eq!(after.len(), 1, "Latest must reuse the strand: {after:?}");
        assert_eq!(
            after[0].messages, 4,
            "two full exchanges expected: {after:?}"
        );
        let resumed = ka_strand::read(&after[0].path).unwrap();
        let users: Vec<&str> = resumed
            .iter()
            .filter_map(|r| match r {
                ka_strand::Record::Message {
                    role: ka_strand::Role::User,
                    content,
                    ..
                } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(users, vec!["hello there", "second question"]);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[tokio::test]
    async fn export_markdown_writes_and_rejects_empty_sessions() {
        use crate::engine::{StrandChoice, spawn_full};

        let data = std::env::temp_dir().join(format!("ka-eng-export-{}", std::process::id()));
        let work = data.join("work");
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&work).unwrap();
        ka_strand::set_data_dir_for_tests(data.join("data"));

        let cfg = Config {
            cwd: Some(work.display().to_string()),
            ..Default::default()
        };
        let mut h = spawn_full(
            cfg.clone(),
            ka_dialect::Catalog::embedded(),
            StrandChoice::New,
        );
        h.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "hi".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        while let Some(evt) = h.events.recv().await {
            if matches!(evt, Event::TurnFinished { .. }) {
                break;
            }
        }
        let out = work.join("out.md");
        h.commands
            .send(Command::ExportMarkdown {
                html: false,
                out: Some(out.clone()),
            })
            .await
            .unwrap();
        let mut saw_note = false;
        while let Some(evt) = h.events.recv().await {
            if let Event::Note { message } = evt {
                assert!(
                    message.contains("exported") && message.contains("out.md"),
                    "{message}"
                );
                saw_note = true;
                break;
            }
        }
        assert!(saw_note, "export must announce its output path");
        let md = std::fs::read_to_string(&out).unwrap();
        assert!(md.contains("### **you**"), "{md}");
        assert!(md.contains("hi"), "{md}");
        drop(h);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // empty session: error, and no default-named file appears in cwd
        let mut h2 = spawn_full(cfg, ka_dialect::Catalog::embedded(), StrandChoice::New);
        h2.commands
            .send(Command::ExportMarkdown {
                out: None,
                html: false,
            })
            .await
            .unwrap();
        let mut saw_err = false;
        while let Some(evt) = h2.events.recv().await {
            if let Event::Error { message, .. } = evt {
                assert!(message.contains("nothing to export"), "{message}");
                saw_err = true;
                break;
            }
        }
        assert!(saw_err, "empty session export must error");
        drop(h2);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let leftovers: Vec<String> = std::fs::read_dir(&work)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("ka-session-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[tokio::test]
    async fn fresh_bootstrap_emits_single_empty_replay() {
        use crate::engine::{StrandChoice, spawn_full};
        use ka_protocol::Command;

        let data = std::env::temp_dir().join(format!("ka-eng-boot-replay-{}", std::process::id()));
        let work = data.join("work");
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&work).unwrap();
        ka_strand::set_data_dir_for_tests(data.clone());

        let cfg = Config {
            cwd: Some(work.display().to_string()),
            ..Default::default()
        };
        // fresh session: exactly one Replay, with zero messages
        let mut h = spawn_full(
            cfg.clone(),
            ka_dialect::Catalog::embedded(),
            StrandChoice::New,
        );
        let mut replays: Vec<usize> = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_millis(300), h.events.recv()).await {
                Ok(Some(Event::Replay { messages })) => replays.push(messages.len()),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        assert_eq!(
            replays,
            vec![0],
            "fresh bootstrap: one empty replay, got {replays:?}"
        );
        drop(h);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // resumed session with content: exactly one Replay, populated
        let mut h2 = spawn_full(
            cfg.clone(),
            ka_dialect::Catalog::embedded(),
            StrandChoice::Latest,
        );
        h2.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "hello".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        while let Some(evt) = h2.events.recv().await {
            if matches!(evt, Event::Idle) {
                break;
            }
        }
        drop(h2);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut h3 = spawn_full(cfg, ka_dialect::Catalog::embedded(), StrandChoice::Latest);
        let mut replays: Vec<usize> = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_millis(300), h3.events.recv()).await {
                Ok(Some(Event::Replay { messages })) => replays.push(messages.len()),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        assert_eq!(
            replays.len(),
            1,
            "resumed bootstrap: exactly one replay, got {replays:?}"
        );
        assert_eq!(
            replays[0], 2,
            "resumed replay covers the prior exchange, got {replays:?}"
        );
        drop(h3);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[tokio::test]
    async fn rewind_persists_and_resume_truncates() {
        use crate::engine::{StrandChoice, spawn_full};
        use ka_protocol::Command;

        let data = std::env::temp_dir().join(format!("ka-eng-rewind-{}", std::process::id()));
        let work = data.join("work");
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&work).unwrap();
        ka_strand::set_data_dir_for_tests(data.clone());

        let cfg = Config {
            cwd: Some(work.display().to_string()),
            ..Default::default()
        };
        let mut h = spawn_full(
            cfg.clone(),
            ka_dialect::Catalog::embedded(),
            StrandChoice::New,
        );
        for prompt in ["first question", "second question"] {
            h.commands
                .send(Command::Prompt {
                    allowed_tools: None,
                    model: None,
                    text: prompt.into(),
                    schema: None,
                    images: Vec::new(),
                    skills: Vec::new(),
                })
                .await
                .unwrap();
            while let Some(evt) = h.events.recv().await {
                if matches!(evt, Event::Idle) {
                    break;
                }
            }
        }
        // rewind one turn
        h.commands.send(Command::Rewind { turns: 1 }).await.unwrap();
        while let Some(evt) = h.events.recv().await {
            if matches!(evt, Event::Idle) {
                break;
            }
        }
        drop(h);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // resume: history must end after the FIRST exchange only
        let mut h2 = spawn_full(cfg, ka_dialect::Catalog::embedded(), StrandChoice::Latest);
        let mut replayed = Vec::new();
        while let Some(evt) = h2.events.recv().await {
            if let Event::Replay { messages } = evt {
                replayed = messages.into_iter().map(|m| m.content).collect();
                break;
            }
        }
        drop(h2);
        assert_eq!(
            replayed,
            vec!["first question".to_string(), "(ka, no model configured) heard: first question — set a model with --model or KA_MODEL to speak for real.".to_string()],
            "rewound session replays only the kept exchange: {replayed:?}"
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    #[tokio::test]
    async fn rewind_of_first_turn_stays_rewound_after_resume() {
        use crate::engine::{StrandChoice, spawn_full};
        use ka_protocol::Command;

        let data = std::env::temp_dir().join(format!("ka-eng-rewind1-{}", std::process::id()));
        let work = data.join("work");
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&work).unwrap();
        ka_strand::set_data_dir_for_tests(data.clone());

        let cfg = Config {
            cwd: Some(work.display().to_string()),
            ..Default::default()
        };
        let mut h = spawn_full(cfg, ka_dialect::Catalog::embedded(), StrandChoice::New);
        h.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "doomed question".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        while let Some(evt) = h.events.recv().await {
            if matches!(evt, Event::Idle) {
                break;
            }
        }
        // rewinding the FIRST turn: nothing survives — a naive Rewind
        // record pointing at the dropped message's own id resurrects
        // it on replay
        h.commands.send(Command::Rewind { turns: 1 }).await.unwrap();
        while let Some(evt) = h.events.recv().await {
            if matches!(evt, Event::Idle) {
                break;
            }
        }
        h.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "fresh start".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        while let Some(evt) = h.events.recv().await {
            if matches!(evt, Event::Idle) {
                break;
            }
        }
        drop(h);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // replay from the persisted strand: the rewound content must
        // stay gone, the post-rewind turn must be there
        let summary = ka_strand::latest(&work).unwrap().unwrap();
        let strand = ka_strand::StrandFile::open(&summary.path).unwrap();
        let (history, _, _) = super::history_from_records(strand.records(), true);
        let joined: Vec<&str> = history.iter().map(|m| m.content.as_str()).collect();
        assert!(
            !joined.iter().any(|c| c.contains("doomed question")),
            "rewound first turn must not resurrect: {joined:?}"
        );
        assert!(
            joined.iter().any(|c| c.contains("fresh start")),
            "post-rewind turn must survive: {joined:?}"
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    #[tokio::test]
    async fn switch_strand_replays_target_history() {
        let data = std::env::temp_dir().join(format!("ka-test-switch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        ka_strand::set_data_dir_for_tests(data.clone());
        let cwd = data.join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        let mut handle = spawn(Config {
            cwd: Some(cwd.clone().to_string_lossy().into()),
            ..Default::default()
        });
        handle
            .commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "first session question".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        // drain to idle
        loop {
            let evt = tokio::time::timeout(Duration::from_millis(2_000), handle.events.recv())
                .await
                .unwrap()
                .unwrap();
            if matches!(evt, Event::Idle) {
                break;
            }
        }
        let sessions = ka_strand::list(&cwd).unwrap();
        let first_id = sessions[0].id.clone();

        // switch to a fresh session
        handle
            .commands
            .send(Command::SwitchStrand { id: "new".into() })
            .await
            .unwrap();
        let mut saw_new_session = false;
        let mut saw_replay_empty = true;
        loop {
            let evt = tokio::time::timeout(Duration::from_millis(2_000), handle.events.recv())
                .await
                .unwrap()
                .unwrap();
            match evt {
                Event::SessionInfo { id } if id != first_id => saw_new_session = true,
                Event::Replay { messages } => saw_replay_empty = messages.is_empty(),
                Event::Note { .. } => {}
                Event::Idle => break,
                _ => {}
            }
        }
        assert!(saw_new_session, "switch announced a new session id");
        assert!(saw_replay_empty, "fresh session replays no history");

        // switch back to the first session by id prefix
        let tail: String = first_id.split('-').nth(1).unwrap_or(&first_id).to_string();
        handle
            .commands
            .send(Command::SwitchStrand { id: tail })
            .await
            .unwrap();
        let mut back_id = String::new();
        let mut replayed_first = false;
        loop {
            let evt = tokio::time::timeout(Duration::from_millis(2_000), handle.events.recv())
                .await
                .unwrap()
                .unwrap();
            match evt {
                Event::SessionInfo { id } => back_id = id,
                Event::Replay { messages } => {
                    replayed_first = messages
                        .iter()
                        .any(|m| m.content.contains("first session question"));
                }
                Event::Note { .. } => {}
                Event::Idle => break,
                _ => {}
            }
        }
        assert_eq!(
            back_id, first_id,
            "returned to the first session by tail prefix"
        );
        assert!(replayed_first, "history of the target session was replayed");
        drop(handle);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[tokio::test]
    async fn unsupported_commands_report_errors() {
        let mut handle = spawn(Config::default());
        handle
            .commands
            .send(Command::Compact { focus: None })
            .await
            .unwrap();
        let evt = loop {
            let evt = tokio::time::timeout(Duration::from_millis(500), handle.events.recv())
                .await
                .unwrap()
                .unwrap();
            match evt {
                Event::SessionInfo { .. }
                | Event::ModelChanged { .. }
                | Event::ModeChanged { .. }
                | Event::EffortChanged { .. }
                | Event::Replay { .. }
                | Event::Inventory { .. }
                | Event::Todos { .. } => continue,
                other => break other,
            }
        };
        assert!(matches!(
            evt,
            Event::Error {
                class: ErrorClass::Unsupported,
                ..
            }
        ));
    }

    /// Drain events until the next Idle, collecting what came before.
    async fn drain_to_idle(handle: &mut EngineHandle) -> Vec<Event> {
        let mut seen = Vec::new();
        loop {
            let evt = tokio::time::timeout(Duration::from_millis(2_000), handle.events.recv())
                .await
                .unwrap()
                .unwrap();
            let idle = matches!(evt, Event::Idle);
            seen.push(evt);
            if idle {
                return seen;
            }
        }
    }

    async fn prompt_and_settle(handle: &mut EngineHandle, text: &str) {
        handle
            .commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: text.to_string(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        drain_to_idle(handle).await;
    }

    #[tokio::test]
    async fn fork_truncates_copies_and_rejects_overlarge_turns() {
        use crate::engine::{StrandChoice, spawn_full};

        let data = std::env::temp_dir().join(format!("ka-eng-fork-{}", std::process::id()));
        let work = data.join("work");
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&work).unwrap();
        ka_strand::set_data_dir_for_tests(data.clone());

        let cfg = Config {
            cwd: Some(work.display().to_string()),
            ..Default::default()
        };
        let mut handle = spawn_full(cfg, ka_dialect::Catalog::embedded(), StrandChoice::New);
        prompt_and_settle(&mut handle, "first question").await;
        prompt_and_settle(&mut handle, "second question").await;

        let sessions = ka_strand::list(&work).unwrap();
        assert_eq!(sessions.len(), 1);
        let original_id = sessions[0].id.clone();
        let original_path = sessions[0].path.clone();

        // fork dropping the last user turn: copy ends after exchange one,
        // titled "<first user message> (fork)", and the engine switches to it
        handle
            .commands
            .send(Command::ForkStrand { turns: 1 })
            .await
            .unwrap();
        let events = drain_to_idle(&mut handle).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::SessionInfo { id } if *id != original_id)),
            "fork must announce a new session: {events:?}"
        );
        let sessions = ka_strand::list(&work).unwrap();
        assert_eq!(sessions.len(), 2, "{sessions:?}");
        let fork = sessions.iter().find(|s| s.id != original_id).unwrap();
        assert_eq!(fork.title, "first question (fork)");
        assert_eq!(fork.messages, 2, "last user turn dropped: {fork:?}");

        // overlarge turns: Error + Idle, no new strand, engine unchanged
        handle
            .commands
            .send(Command::ForkStrand { turns: 3 })
            .await
            .unwrap();
        let events = drain_to_idle(&mut handle).await;
        assert!(
            events.iter().any(|e| matches!(e, Event::Error { .. })),
            "overlarge fork must error: {events:?}"
        );
        assert_eq!(ka_strand::list(&work).unwrap().len(), 2);

        // exact copy (turns = 0) of the ACTIVE strand (the first fork):
        // same messages, fork-of-fork title
        handle
            .commands
            .send(Command::ForkStrand { turns: 0 })
            .await
            .unwrap();
        drain_to_idle(&mut handle).await;
        let sessions = ka_strand::list(&work).unwrap();
        assert_eq!(sessions.len(), 3, "{sessions:?}");
        let copy = sessions
            .iter()
            .find(|s| s.id != original_id && s.path != fork.path)
            .unwrap();
        assert_eq!(copy.title, "first question (fork) (fork)");
        assert_eq!(copy.messages, fork.messages, "exact copy keeps messages");
        let copy_records = ka_strand::read(&copy.path).unwrap();
        let fork_records = ka_strand::read(&fork.path).unwrap();
        let msg_contents = |records: &[ka_strand::Record]| -> Vec<String> {
            records
                .iter()
                .filter_map(|r| match r {
                    ka_strand::Record::Message { content, .. } => Some(content.clone()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(msg_contents(&copy_records), msg_contents(&fork_records));

        // the ORIGINAL strand file is untouched by all three forks
        let original = ka_strand::read(&original_path).unwrap();
        assert_eq!(msg_contents(&original).len(), 4);
        drop(handle);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// Spawn with `cwd` pinned to a fresh empty dir, drop the command
    /// side, and collect everything the engine emits at bootstrap.
    async fn drain_bootstrap(tag: &str, mut cfg: Config) -> Vec<Event> {
        let root = std::env::temp_dir().join(format!(
            "ka-eng-inv-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        cfg.cwd = Some(root.to_string_lossy().into_owned());
        let mut handle = spawn(cfg);
        drop(handle.commands);
        let mut seen = Vec::new();
        while let Some(evt) = handle.events.recv().await {
            seen.push(evt);
        }
        let _ = std::fs::remove_dir_all(&root);
        seen
    }

    #[tokio::test]
    async fn bootstrap_emits_exactly_one_inventory() {
        let seen = drain_bootstrap("bare", Config::default()).await;
        let mut inventories = seen.iter().filter(|e| matches!(e, Event::Inventory { .. }));
        assert!(
            inventories.next().is_some(),
            "bootstrap must emit an Inventory: {seen:?}"
        );
        assert!(
            inventories.next().is_none(),
            "exactly one Inventory expected: {seen:?}"
        );
        // the ad-hoc inventory notes are gone
        assert!(
            seen.iter().all(|e| !matches!(
                e,
                Event::Note { message } if message.contains("agents available")
            )),
            "no agents-available note expected: {seen:?}"
        );
    }

    #[tokio::test]
    async fn bootstrap_emits_the_effort_level_beside_the_mode() {
        // surfaces render the true level from launch, not an assumed
        // default: the bootstrap cluster emits EffortChanged(Off)
        // right after ModeChanged
        let seen = drain_bootstrap("effort", Config::default()).await;
        let mode_at = seen
            .iter()
            .position(|e| matches!(e, Event::ModeChanged { .. }))
            .expect("bootstrap emits ModeChanged");
        let effort_at = seen
            .iter()
            .position(|e| {
                matches!(
                    e,
                    Event::EffortChanged {
                        level: ka_protocol::Effort::Off
                    }
                )
            })
            .expect("bootstrap emits EffortChanged(Off)");
        assert_eq!(effort_at, mode_at + 1, "effort follows the mode: {seen:?}");
        // exactly one emission
        assert_eq!(
            seen.iter()
                .filter(|e| matches!(e, Event::EffortChanged { .. }))
                .count(),
            1,
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn bootstrap_inventory_lists_agents_skills_and_failed_mcp() {
        let root = std::env::temp_dir().join(format!(
            "ka-eng-inv-rich-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".ka/agents")).unwrap();
        // project-scope skills are gated on the trust store: redirect the
        // store to a fresh file and trust the injected project dir
        let _trust = crate::trust::test_support::trust_guard();
        crate::trust::approve(&root);
        std::fs::write(
            root.join(".ka/agents/reviewer.md"),
            "---\nname: reviewer\ndescription: reviews code\n---\nBe harsh.\n",
        )
        .unwrap();
        for name in ["demo", "second"] {
            std::fs::create_dir_all(root.join(format!(".ka/skills/{name}"))).unwrap();
            std::fs::write(
                root.join(format!(".ka/skills/{name}/SKILL.md")),
                format!("---\ndescription: {name} skill\n---\nDo things.\n"),
            )
            .unwrap();
        }
        let cfg = Config {
            cwd: Some(root.to_string_lossy().into_owned()),
            mcp: vec![crate::mcp::McpServerConfig {
                name: "ghost".into(),
                command: Some("ka-definitely-not-a-binary-9x7".into()),
                args: Vec::new(),
                env: Default::default(),
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
            }],
            ..Config::default()
        };
        let mut handle = spawn(cfg);
        drop(handle.commands);
        let mut seen = Vec::new();
        while let Some(evt) = handle.events.recv().await {
            seen.push(evt);
        }
        let _ = std::fs::remove_dir_all(&root);

        // the spawn failure stays visible as an Error naming the server
        let errors: Vec<String> = seen
            .iter()
            .filter_map(|e| match e {
                Event::Error { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(errors.len(), 1, "one mcp failure error: {seen:?}");
        assert!(errors[0].contains("ghost"), "error names the server");

        // exactly one Inventory carrying the injected counts
        let inventories: Vec<&Event> = seen
            .iter()
            .filter(|e| matches!(e, Event::Inventory { .. }))
            .collect();
        assert_eq!(inventories.len(), 1, "exactly one Inventory: {seen:?}");
        let Event::Inventory {
            tools,
            mcp,
            agents,
            skills,
            prompts: _,
            disabled,
        } = inventories[0]
        else {
            unreachable!("filtered above")
        };
        assert!(disabled.is_empty(), "no toggles at bootstrap: {disabled:?}");
        assert_eq!(agents, &["reviewer".to_string()]);
        // discovery also sees the user HOME layer, so only the injected
        // names are asserted exactly
        assert!(
            skills.iter().any(|s| s.name == "demo"),
            "skills: {skills:?}"
        );
        assert!(
            skills.iter().any(|s| s.name == "second"),
            "skills: {skills:?}"
        );
        assert_eq!(
            mcp,
            &[ka_protocol::McpSummary {
                name: "ghost".into(),
                ok: false,
                tools: 0,
            }]
        );
        // 7 built-ins + todo + jobs + checkpoint + cron + monitor +
        // tool_search + delegate + tasks + team + remember + web_fetch
        // (search registers only with a configured [[search]] provider),
        // plus `code` when the TOOL-04 cargo feature is compiled in
        let expected = 18 + usize::from(cfg!(feature = "code"));
        assert_eq!(tools.len(), expected, "tools: {tools:?}");
        assert!(tools.contains(&"delegate".to_string()), "tools: {tools:?}");
        assert!(tools.contains(&"todo".to_string()), "tools: {tools:?}");
        assert!(tools.contains(&"jobs".to_string()), "tools: {tools:?}");
        // and the ad-hoc notes stay gone
        assert!(seen.iter().all(
            |e| !matches!(e, Event::Note { message } if message.contains("agents available") || message.contains("tool(s)"))
        ));
    }

    #[tokio::test]
    async fn bootstrap_with_cached_mcp_registers_the_meta_hand_once() {
        // cache-hit registration is the reported duplicate path: the
        // remembered eager hands load synchronously, the meta hand is
        // rebuilt inside ensure_mcp_server, and a bootstrap push on top
        // used to register a second `mcp` — providers then reject the
        // whole turn with "tools: Tool names must be unique"
        let _cache = crate::mcp_cache::test_support::cache_guard();
        let server = crate::mcp::McpServerConfig {
            name: "cached".into(),
            command: Some("ka-definitely-not-a-binary-9x7".into()),
            args: Vec::new(),
            env: Default::default(),
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
        };
        crate::mcp_cache::store_tools(
            &server,
            &[crate::mcp::McpTool {
                name: "cached__ping".into(),
                raw_name: "ping".into(),
                description: "ping".into(),
                schema: serde_json::json!({"type": "object"}),
                read_only_hint: false,
            }],
        );
        let root = std::env::temp_dir().join(format!(
            "ka-eng-inv-mcpdup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cfg = Config {
            cwd: Some(root.to_string_lossy().into_owned()),
            mcp: vec![server],
            ..Config::default()
        };
        let mut handle = spawn(cfg);
        drop(handle.commands);
        let mut seen = Vec::new();
        while let Some(evt) = handle.events.recv().await {
            seen.push(evt);
        }
        let _ = std::fs::remove_dir_all(&root);

        let inventories: Vec<&Event> = seen
            .iter()
            .filter(|e| matches!(e, Event::Inventory { .. }))
            .collect();
        assert!(!inventories.is_empty(), "bootstrap emitted inventory");
        let Event::Inventory { tools, .. } = inventories[0] else {
            unreachable!("filtered above")
        };
        assert!(
            tools.contains(&"mcp".to_string()),
            "meta hand present: {tools:?}"
        );
        assert_eq!(
            tools.iter().filter(|t| *t == "mcp").count(),
            1,
            "meta hand registered exactly once: {tools:?}"
        );
        let mut sorted = tools.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            tools.len(),
            "no duplicate tool names at bootstrap: {tools:?}"
        );
    }

    #[tokio::test]
    async fn untrusted_project_agents_absent_from_inventory_until_approved() {
        let root = std::env::temp_dir().join(format!(
            "ka-eng-inv-untrusted-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".ka/agents")).unwrap();
        // fresh trust store pointing at nothing: the project is NOT
        // trusted yet
        let _trust = crate::trust::test_support::trust_guard();
        std::fs::write(
            root.join(".ka/agents/reviewer.md"),
            "---\nname: reviewer\ndescription: reviews code\n---\nBe harsh.\n",
        )
        .unwrap();

        let mk_cfg = || Config {
            cwd: Some(root.to_string_lossy().into_owned()),
            ..Config::default()
        };
        let inventory_agents = |cfg: Config| async {
            let mut handle = spawn(cfg);
            drop(handle.commands);
            let mut agents = None;
            while let Some(evt) = handle.events.recv().await {
                if let Event::Inventory { agents: a, .. } = evt {
                    agents = Some(a);
                }
            }
            agents
        };

        // untrusted: project agent dir must contribute NOTHING — the
        // project reviewer must be absent no matter what user-level
        // agents this machine has ($HOME/.config/ka/agents always loads)
        let agents = inventory_agents(mk_cfg()).await;
        assert!(
            !agents.iter().flatten().any(|a| a == "reviewer"),
            "untrusted project agents must be absent: {agents:?}"
        );

        // after approval the roster appears
        crate::trust::approve(&root);
        let agents = inventory_agents(mk_cfg()).await;
        assert!(
            agents.iter().flatten().any(|a| a == "reviewer"),
            "approved project agents appear: {agents:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
    // ── auto-titles ([roles] fast) ────────────────────────────────────

    const TURN_SSE: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Sure thing.\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );
    const TITLE_SSE: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Fix the parser\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    /// Serve a queue of canned SSE bodies over a blocking-socket listener;
    /// each HTTP request pops the next body and is captured.
    fn serve_sse(
        bodies: Vec<&'static str>,
        captured: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
    ) -> std::net::SocketAddr {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for body in bodies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
                    match stream.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if request_complete(&buf) {
                                break;
                            }
                        }
                    }
                }
                captured
                    .lock()
                    .push(String::from_utf8_lossy(&buf).into_owned());
                let http = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: \
                     close\r\n\r\n{body}"
                );
                let _ = stream.write_all(http.as_bytes());
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });
        addr
    }

    fn request_complete(buf: &[u8]) -> bool {
        let Ok(s) = std::str::from_utf8(buf) else {
            return false;
        };
        let Some(pos) = s.find("\r\n\r\n") else {
            return false;
        };
        s.to_ascii_lowercase()
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .is_none_or(|cl| buf.len() >= pos + 4 + cl)
    }

    fn live_catalog(
        main_addr: std::net::SocketAddr,
        fast_addr: Option<std::net::SocketAddr>,
    ) -> ka_dialect::Catalog {
        let mut text = format!(
            "[dialects.\"test/main\"]\nwire = \"openai_chat\"\nbase_url = \
             \"http://{main_addr}/v1\"\ncontext = 100000\n"
        );
        if let Some(fast) = fast_addr {
            text.push_str(&format!(
                "\n[dialects.\"test/fast\"]\nwire = \"openai_chat\"\nbase_url = \
                 \"http://{fast}/v1\"\ncontext = 100000\n"
            ));
        }
        ka_dialect::Catalog::parse(&text).unwrap()
    }

    fn title_workdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ka-eng-title-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn title_cfg(work: &std::path::Path, fast: Option<&str>) -> Config {
        Config {
            model: Some("test/main".into()),
            roles: crate::config::Roles {
                default: None,
                fast: fast.map(str::to_string),
                extra: Default::default(),
            },
            cwd: Some(work.display().to_string()),
            ..Config::default()
        }
    }

    fn title_events(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Title { title } => Some(title.as_str()),
                _ => None,
            })
            .collect()
    }

    fn stored_titles(work: &std::path::Path) -> Vec<String> {
        let sessions = ka_strand::list(work).unwrap();
        sessions
            .first()
            .map(|s| {
                ka_strand::read(&s.path)
                    .unwrap()
                    .iter()
                    .filter_map(|r| match r {
                        ka_strand::Record::Title { title, .. } => Some(title.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn auto_title_generated_via_fast_role_and_persisted() {
        use crate::engine::{StrandChoice, spawn_full};
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let addr = serve_sse(vec![TURN_SSE, TITLE_SSE], captured.clone());
        let work = title_workdir("ok");
        let cfg = title_cfg(&work, Some("test/main"));
        let mut h = spawn_full(cfg, live_catalog(addr, None), StrandChoice::New);
        h.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "please fix the parser in src/x.rs".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let events = drain_to_idle(&mut h).await;
        assert_eq!(
            title_events(&events),
            vec!["Fix the parser"],
            "one cleaned title event: {events:?}"
        );
        assert!(
            events.iter().all(|e| !matches!(e, Event::Error { .. })),
            "title flow must never surface errors: {events:?}"
        );
        // exactly two wire calls: the turn, then the title
        assert_eq!(captured.lock().len(), 2, "requests: {:?}", captured.lock());
        // the record is persisted and the summary reads it back
        assert_eq!(stored_titles(&work), vec!["Fix the parser".to_string()]);
        let sessions = ka_strand::list(&work).unwrap();
        assert_eq!(sessions[0].title, "Fix the parser");
        let _ = std::fs::remove_dir_all(&work);
    }

    #[tokio::test]
    async fn auto_title_failure_is_silent_and_keeps_heuristic() {
        use crate::engine::{StrandChoice, spawn_full};
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let addr = serve_sse(vec![TURN_SSE], captured.clone());
        // a port with no listener: the fast call fails to connect
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);
        let work = title_workdir("fail");
        let cfg = title_cfg(&work, Some("test/fast"));
        let mut h = spawn_full(cfg, live_catalog(addr, Some(dead_addr)), StrandChoice::New);
        h.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "please fix the parser in src/x.rs".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let events = drain_to_idle(&mut h).await;
        assert!(
            title_events(&events).is_empty(),
            "failed title must stay silent: {events:?}"
        );
        assert!(
            events.iter().all(|e| !matches!(e, Event::Error { .. })),
            "title failures never reach the transcript: {events:?}"
        );
        assert_eq!(captured.lock().len(), 1, "only the turn ran");
        assert!(
            stored_titles(&work).is_empty(),
            "no Title record on failure"
        );
        // the heuristic survives: summary title = first user message
        let sessions = ka_strand::list(&work).unwrap();
        assert!(sessions[0].title.starts_with("please fix the parser"));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[tokio::test]
    async fn auto_title_unconfigured_keeps_heuristic() {
        use crate::engine::{StrandChoice, spawn_full};
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let addr = serve_sse(vec![TURN_SSE], captured.clone());
        let work = title_workdir("nofast");
        let cfg = title_cfg(&work, None);
        let mut h = spawn_full(cfg, live_catalog(addr, None), StrandChoice::New);
        h.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "please fix the parser in src/x.rs".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let events = drain_to_idle(&mut h).await;
        assert!(
            title_events(&events).is_empty(),
            "no title without [roles] fast: {events:?}"
        );
        assert_eq!(captured.lock().len(), 1, "only the turn ran");
        assert!(stored_titles(&work).is_empty());
        let sessions = ka_strand::list(&work).unwrap();
        assert!(sessions[0].title.starts_with("please fix the parser"));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[tokio::test]
    async fn auto_title_never_refires_on_resume() {
        use crate::engine::{StrandChoice, spawn_full};
        let work = title_workdir("resume");
        // session 1: the title fires (turn + title calls)
        let cap_a = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let addr_a = serve_sse(vec![TURN_SSE, TITLE_SSE], cap_a.clone());
        let cfg = title_cfg(&work, Some("test/main"));
        let mut h1 = spawn_full(cfg.clone(), live_catalog(addr_a, None), StrandChoice::New);
        h1.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "please fix the parser in src/x.rs".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let first = drain_to_idle(&mut h1).await;
        assert_eq!(title_events(&first), vec!["Fix the parser"]);
        drop(h1);
        // session 2 (resume): the stored title is announced once at
        // bootstrap, no second call ever fires
        let cap_b = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let addr_b = serve_sse(vec![TURN_SSE], cap_b.clone());
        let mut h2 = spawn_full(cfg, live_catalog(addr_b, None), StrandChoice::Latest);
        // bootstrap ends at Replay; the stored title must echo there
        let mut bootstrap = Vec::new();
        while let Ok(Some(evt)) =
            tokio::time::timeout(Duration::from_secs(5), h2.events.recv()).await
        {
            let done = matches!(evt, Event::Replay { .. });
            bootstrap.push(evt);
            if done {
                break;
            }
        }
        assert_eq!(
            title_events(&bootstrap),
            vec!["Fix the parser"],
            "bootstrap carries the stored title: {bootstrap:?}"
        );
        h2.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "second question".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let second = drain_to_idle(&mut h2).await;
        assert!(
            title_events(&second).is_empty(),
            "resume must not re-title: {second:?}"
        );
        assert_eq!(
            cap_b.lock().len(),
            1,
            "only the second turn ran: {:?}",
            cap_b.lock()
        );
        assert_eq!(
            stored_titles(&work),
            vec!["Fix the parser".to_string()],
            "exactly one Title record, ever"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    #[tokio::test]
    async fn resume_replays_stored_image_parts() {
        use crate::engine::{StrandChoice, spawn_full};
        use ka_protocol::ImagePart;

        let vision_catalog = |addr: std::net::SocketAddr| {
            ka_dialect::Catalog::parse(&format!(
                "[dialects.\"test/main\"]\nwire = \"openai_chat\"\nbase_url = \
                 \"http://{addr}/v1\"\ncontext = 100000\ninput = [\"text\", \"image\"]\n"
            ))
            .unwrap()
        };
        let cfg = |work: &std::path::Path| Config {
            model: Some("test/main".into()),
            cwd: Some(work.display().to_string()),
            ..Config::default()
        };
        let work = title_workdir("img-replay");

        // session 1: the prompt carries an image
        let cap_a = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let addr_a = serve_sse(vec![TURN_SSE], cap_a.clone());
        let mut h1 = spawn_full(cfg(&work), vision_catalog(addr_a), StrandChoice::New);
        h1.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "what is in this picture".into(),
                schema: None,
                images: vec![ImagePart {
                    data: "aW1n".into(),
                    media_type: "image/png".into(),
                }],
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let first = drain_to_idle(&mut h1).await;
        assert!(
            first.iter().all(|e| !matches!(e, Event::Error { .. })),
            "{first:?}"
        );
        drop(h1);

        // the strand persisted the image at message level
        let sessions = ka_strand::list(&work).unwrap();
        let stored = ka_strand::read(&sessions[0].path)
            .unwrap()
            .iter()
            .any(|r| matches!(r, ka_strand::Record::Message { images, .. } if !images.is_empty()));
        assert!(stored, "image parts must persist in the strand");

        // session 2 (resume, vision dialect): the request re-sends them
        let cap_b = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let addr_b = serve_sse(vec![TURN_SSE], cap_b.clone());
        let mut h2 = spawn_full(cfg(&work), vision_catalog(addr_b), StrandChoice::Latest);
        h2.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "and now".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let second = drain_to_idle(&mut h2).await;
        assert!(
            second.iter().all(|e| !matches!(e, Event::Error { .. })),
            "{second:?}"
        );
        assert_eq!(cap_b.lock().len(), 1, "requests: {:?}", cap_b.lock());
        assert!(
            cap_b.lock()[0].contains("data:image/png;base64,aW1n"),
            "the replayed image must ride the resumed request"
        );
        drop(h2);

        // session 3 (resume, text-only dialect): replay drops them or
        // the wire's vision gate fails every request
        let cap_c = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let addr_c = serve_sse(vec![TURN_SSE], cap_c.clone());
        let mut h3 = spawn_full(cfg(&work), live_catalog(addr_c, None), StrandChoice::Latest);
        h3.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "again".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let third = drain_to_idle(&mut h3).await;
        assert!(
            third.iter().all(|e| !matches!(e, Event::Error { .. })),
            "text-only resume must not hit the vision gate: {third:?}"
        );
        assert!(!cap_c.lock()[0].contains("base64"), "images dropped");
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn fork_records_parent_linkage() {
        let work = std::env::temp_dir().join(format!("ka-fork-parent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        let mut parent = ka_strand::StrandFile::create(&work, None).unwrap();
        parent
            .append(ka_strand::Record::Message {
                id: ka_strand::new_record_id(),
                images: Vec::new(),
                role: ka_strand::Role::User,
                content: "hello".into(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
            })
            .unwrap();

        let child_path = super::fork_strand(&work, &parent, 0).expect("fork created");
        let child = ka_strand::StrandFile::open(&child_path).expect("child opens");
        let parent_id = parent
            .records()
            .iter()
            .find_map(|r| match r {
                ka_strand::Record::Header { id, .. } => Some(id.0.clone()),
                _ => None,
            })
            .unwrap();
        let child_parent = child.records().iter().find_map(|r| match r {
            ka_strand::Record::Header { parent, .. } => parent.clone(),
            _ => None,
        });
        assert_eq!(child_parent.as_deref(), Some(parent_id.as_str()));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn replay_messages_merge_tool_results_into_calls() {
        use ka_dialect::speaker::{ToolCall, ToolResult, TurnMessage};
        let history = vec![
            TurnMessage::user("list files"),
            TurnMessage {
                role: ka_dialect::speaker::TurnRole::Assistant,
                content: String::new(),
                thinking: Some("scan".into()),
                calls: vec![ToolCall {
                    id: "c1".into(),
                    tool: "read".into(),
                    arguments: serde_json::json!({"path": "lib.rs"}),
                }],
                results: Vec::new(),
                images: Vec::new(),
            },
            TurnMessage::tool(vec![ToolResult {
                call_id: "c1".into(),
                content: "line one\nline two".into(),
                is_error: false,
                images: Vec::new(),
            }]),
            TurnMessage::assistant("found it"),
        ];
        let msgs = super::replay_messages(&history);
        // the calls-only assistant turn survives (it carries calls + a
        // thought); the tool-role message emits no row of its own
        assert_eq!(msgs.len(), 3, "{msgs:?}");
        let call_row = &msgs[1];
        assert!(call_row.content.is_empty());
        assert_eq!(call_row.thinking.as_deref(), Some("scan"));
        let call = &call_row.calls[0];
        assert_eq!(call.id, "c1");
        assert_eq!(call.tool, "read");
        assert!(call.detail.contains("lib.rs"), "detail: {}", call.detail);
        assert_eq!(
            call.result.as_deref(),
            Some("line one"),
            "first-line excerpt"
        );
        assert!(!call.is_error);
    }

    #[test]
    fn record_and_history_roundtrip_thinking() {
        let msg = ka_dialect::speaker::TurnMessage {
            role: ka_dialect::speaker::TurnRole::Assistant,
            content: "answer".into(),
            thinking: Some("pondering".into()),
            calls: Vec::new(),
            results: Vec::new(),
            images: Vec::new(),
        };
        let record = super::record_from_message(&msg);
        let (history, _, _) = super::history_from_records(&[record], true);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].thinking.as_deref(), Some("pondering"));
    }

    #[test]
    fn rewind_with_unknown_anchor_fails_toward_the_rewind() {
        // anchor behind a digest that cleared ids: replay must not keep
        // the abandoned tail logically live
        let msg = |role, content: &str| {
            super::record_from_message(&ka_dialect::speaker::TurnMessage {
                role,
                content: content.into(),
                thinking: None,
                calls: Vec::new(),
                results: Vec::new(),
                images: Vec::new(),
            })
        };
        let user = msg(ka_dialect::speaker::TurnRole::User, "doomed question");
        let anchor = user.id().unwrap().clone();
        let records = vec![
            user,
            msg(ka_dialect::speaker::TurnRole::Assistant, "doomed answer"),
            ka_strand::Record::Digest {
                id: ka_strand::new_record_id(),
                summary: "collapsed".into(),
                kept_from: ka_strand::new_record_id(),
            },
            ka_strand::Record::Rewind {
                id: ka_strand::new_record_id(),
                kept_from: anchor,
            },
        ];
        let (history, ids, digest) = super::history_from_records(&records, true);
        assert!(history.is_empty(), "abandoned tail must not replay");
        assert!(ids.is_empty());
        assert_eq!(digest.as_deref(), Some("collapsed"));
    }

    #[test]
    fn record_and_history_roundtrip_images_vision_gated() {
        let img = || ka_dialect::ImagePart {
            data: "aW1n".into(),
            media_type: "image/png".into(),
        };
        let msg = ka_dialect::speaker::TurnMessage {
            role: ka_dialect::speaker::TurnRole::Tool,
            content: String::new(),
            thinking: None,
            calls: Vec::new(),
            results: vec![ka_dialect::speaker::ToolResult {
                call_id: "c1".into(),
                content: "[image pic.png]".into(),
                is_error: false,
                images: vec![img()],
            }],
            images: vec![img()],
        };
        let record = super::record_from_message(&msg);
        // a vision dialect re-attaches images at both levels
        let (history, _, _) = super::history_from_records(std::slice::from_ref(&record), true);
        assert_eq!(history[0].images.len(), 1);
        assert_eq!(history[0].results[0].images.len(), 1);
        // a text-only dialect drops them: the wire vision gate would
        // otherwise fail the whole request
        let (history, _, _) = super::history_from_records(&[record], false);
        assert!(history[0].images.is_empty());
        assert!(history[0].results[0].images.is_empty());
    }

    // ── CTX-12 · SetAutoCompact ──────────────────────────────────────

    fn notes_of(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Note { message } => Some(message.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn set_autocompact_notes_percent_and_default() {
        let mut handle = spawn(Config::default());
        for (pct, expected) in [
            (Some(70u64), "auto-compact trigger: 70%"),
            (Some(0u64), "out of range"),
            (Some(150u64), "out of range"),
            (None, "auto-compact trigger: default"),
        ] {
            handle
                .commands
                .send(Command::SetAutoCompact { pct })
                .await
                .unwrap();
            let seen = drain_to_idle(&mut handle).await;
            let notes = notes_of(&seen);
            assert!(
                notes.iter().any(|n| n.contains(expected)),
                "expected {expected:?} in {notes:?}"
            );
            if pct.is_some_and(|p| !(1..=100).contains(&p)) {
                // fail-closed: exactly one note, the rejection — the
                // live setting is kept, never applied
                assert_eq!(notes.len(), 1, "only the rejection note: {notes:?}");
                assert!(
                    notes[0].contains("keeping"),
                    "the previous setting is preserved: {notes:?}"
                );
            } else {
                assert_eq!(notes, vec![expected], "single note: {notes:?}");
            }
        }
    }

    #[test]
    fn idle_gates_are_time_and_meter_injected() {
        // these asserts assume the PRODUCTION 60 s unit: serialize
        // against the shrink tests and pin the default while asserting
        let _guard = IDLE_SER.lock();
        use std::sync::atomic::Ordering::Relaxed;
        super::IDLE_UNIT_SECS.store(60, Relaxed);
        // production unit: 60 s per configured minute
        assert!(
            super::recap_due(Duration::from_secs(0), None),
            "None = no extra wait"
        );
        assert!(!super::recap_due(Duration::from_secs(59), Some(1)));
        assert!(super::recap_due(Duration::from_secs(60), Some(1)));
        assert!(!super::compact_due(None, 99), "None = idle compaction off");
        assert!(!super::compact_due(Some(50), 49));
        assert!(super::compact_due(Some(50), 50));
    }

    // ── CTX-02 · manual shake ────────────────────────────────────────

    #[tokio::test]
    async fn shake_command_trims_and_notes() {
        let mut handle = spawn(Config::default());
        // a canned turn: no tools/thinking/images, so nothing can trim —
        // the command still answers with one note and goes idle
        handle
            .commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "hello".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        drain_to_idle(&mut handle).await;
        handle
            .commands
            .send(Command::Shake {
                thinking: true,
                images: true,
            })
            .await
            .unwrap();
        let seen = drain_to_idle(&mut handle).await;
        let notes = notes_of(&seen);
        assert_eq!(
            notes,
            vec!["shake: nothing to trim"],
            "empty history reports no trim: {notes:?}"
        );
    }

    // ── SES-03 · branch summaries on rewind ──────────────────────────

    const RECAP_SSE: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Recap: fixing the parser; build is \
         green.\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    const BRANCH_SSE: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"The branch was fixing the parser; type \
         checks were left unfinished.\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    const DIGEST_SSE: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Earlier: the user asked to fix the \
         parser.\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    /// A turn whose usage pushes the meter to ~5% of the 100k window.
    const TURN_BIG_SSE: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Sure thing.\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5000,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );
    /// A turn whose only output is one `cron` create tool call.
    const CRON_CREATE_SSE: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"cron\",\"arguments\":\"{\\\"prompt\\\": \\\"check the CI\\\", \\\"interval_secs\\\": 1}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );
    /// Same, one-shot.
    const CRON_ONE_SHOT_SSE: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"cron\",\"arguments\":\"{\\\"prompt\\\": \\\"check the CI\\\", \\\"interval_secs\\\": 1, \\\"one_shot\\\": true}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );
    /// A turn whose only output is one `monitor` spawn tool call.
    const MONITOR_SPAWN_SSE: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"monitor\",\"arguments\":\"{\\\"command\\\": \\\"echo line1; echo line2\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );

    fn custom_records(work: &std::path::Path, ns: &str) -> Vec<serde_json::Value> {
        ka_strand::list(work)
            .unwrap()
            .first()
            .map(|s| {
                ka_strand::read(&s.path)
                    .unwrap()
                    .into_iter()
                    .filter_map(|r| match r {
                        ka_strand::Record::Custom { ns: n, data, .. } if n == ns => Some(data),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn rewind_without_fast_notes_unavailable_but_rewinds() {
        let mut handle = spawn(Config::default());
        for text in ["first question", "second question"] {
            handle
                .commands
                .send(Command::Prompt {
                    allowed_tools: None,
                    model: None,
                    text: text.into(),
                    schema: None,
                    images: Vec::new(),
                    skills: Vec::new(),
                })
                .await
                .unwrap();
            drain_to_idle(&mut handle).await;
        }
        handle
            .commands
            .send(Command::Rewind { turns: 1 })
            .await
            .unwrap();
        let seen = drain_to_idle(&mut handle).await;
        let notes = notes_of(&seen);
        assert!(
            notes
                .iter()
                .any(|n| n.starts_with("branch summary unavailable")),
            "unconfigured fast degrades to a note: {notes:?}"
        );
        assert!(notes.contains(&"rewound 1 turn(s)"));
    }

    #[tokio::test]
    async fn rewind_to_boundary_records_no_branch_summary() {
        let mut handle = spawn(Config::default());
        handle
            .commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "only turn".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        drain_to_idle(&mut handle).await;
        handle
            .commands
            .send(Command::Rewind { turns: 1 })
            .await
            .unwrap();
        let seen = drain_to_idle(&mut handle).await;
        let notes = notes_of(&seen);
        assert_eq!(notes, vec!["rewound 1 turn(s)"], "boundary, no summary");
    }

    #[tokio::test]
    async fn rewind_records_branch_summary_reachable_from_tip() {
        use crate::engine::{StrandChoice, spawn_full};
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        // call order: turn 1 → auto-title → turn 2 → branch summary
        let addr = serve_sse(
            vec![TURN_SSE, TITLE_SSE, TURN_SSE, BRANCH_SSE],
            captured.clone(),
        );
        let work = title_workdir("branch");
        let mut h = spawn_full(
            title_cfg(&work, Some("test/fast")),
            live_catalog(addr, Some(addr)),
            StrandChoice::New,
        );
        for text in ["fix the parser in src/x.rs", "also rename the module"] {
            h.commands
                .send(Command::Prompt {
                    allowed_tools: None,
                    model: None,
                    text: text.into(),
                    schema: None,
                    images: Vec::new(),
                    skills: Vec::new(),
                })
                .await
                .unwrap();
            drain_to_idle(&mut h).await;
        }
        h.commands.send(Command::Rewind { turns: 1 }).await.unwrap();
        let seen = drain_to_idle(&mut h).await;
        let notes = notes_of(&seen);
        assert!(notes.contains(&"branch summary saved to the strand"));
        // exactly 4 wire calls: two turns, one title, one summary
        assert_eq!(captured.lock().len(), 4, "requests: {:?}", captured.lock());
        // the record carries the summarized text
        let records = custom_records(&work, ka_strand::BRANCH_SUMMARY_NS);
        assert_eq!(records.len(), 1, "one branch summary record: {records:?}");
        assert!(
            records[0]["text"]
                .as_str()
                .is_some_and(|t| t.contains("parser")),
            "summary text: {records:?}"
        );
        // reachable from the tip: replay the strand's live chain (the
        // rewind retracts the abandoned conversation records; Custom
        // records survive) — the summary must still be there
        let sessions = ka_strand::list(&work).unwrap();
        let live = ka_strand::read(&sessions[0].path).unwrap();
        let rendered = ka_strand::render_jsonl(&live).unwrap();
        let tip_records = parse_records(&rendered);
        let reachable = tip_records
            .iter()
            .filter(|r| {
                matches!(r, ka_strand::Record::Custom { ns, .. } if ns == ka_strand::BRANCH_SUMMARY_NS)
            })
            .count();
        assert_eq!(reachable, 1, "summary survives tip replay");
        let _ = std::fs::remove_dir_all(&work);
    }

    fn parse_records(jsonl: &str) -> Vec<ka_strand::Record> {
        jsonl
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    // ── CTX-07 · idle task ───────────────────────────────────────────

    /// The idle-test factors are process-global; idle tests serialize
    /// on this and restore the defaults when done.
    static IDLE_SER: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// Shrink the idle factors (1 unit second, 1s ticks) for the
    /// holding test; the process-global defaults are restored on drop.
    /// The guard is held, never read — its lock IS the serialization.
    #[allow(dead_code)]
    struct IdleFactors(parking_lot::MutexGuard<'static, ()>);
    impl IdleFactors {
        fn shrink() -> Self {
            let guard = IDLE_SER.lock();
            use std::sync::atomic::Ordering::Relaxed;
            super::IDLE_UNIT_SECS.store(1, Relaxed);
            super::IDLE_TICK_SECS.store(1, Relaxed);
            Self(guard)
        }
    }
    impl Drop for IdleFactors {
        fn drop(&mut self) {
            use std::sync::atomic::Ordering::Relaxed;
            super::IDLE_UNIT_SECS.store(60, Relaxed);
            super::IDLE_TICK_SECS.store(30, Relaxed);
        }
    }

    #[tokio::test]
    async fn idle_task_recaps_and_compacts_once_per_quiet_span() {
        use crate::engine::{StrandChoice, spawn_full};
        let _guard = IdleFactors::shrink();
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        // call order: turn → auto-title → idle recap → idle digest
        let addr = serve_sse(
            vec![TURN_BIG_SSE, TITLE_SSE, RECAP_SSE, DIGEST_SSE],
            captured.clone(),
        );
        let work = title_workdir("idle");
        let cfg = Config {
            model: Some("test/main".into()),
            roles: crate::config::Roles {
                default: None,
                fast: Some("test/fast".into()),
                extra: Default::default(),
            },
            cwd: Some(work.display().to_string()),
            context: crate::config::Context {
                idle_minutes: Some(1),
                recap_minutes: None,       // inherits idle_minutes
                idle_compact_pct: Some(1), // meter at ~5% >= 1%
                ..Default::default()
            },
            ..Config::default()
        };
        let mut h = spawn_full(cfg, live_catalog(addr, Some(addr)), StrandChoice::New);
        h.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "fix the parser".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let first = drain_to_idle(&mut h).await;
        assert!(
            notes_of(&first).iter().all(|n| !n.contains("idle recap")),
            "no recap before the idle span: {:?}",
            notes_of(&first)
        );
        // quiet past the idle gate: recap + idle compaction fire. Tick
        // work never emits Idle (headless surfaces would exit), so the
        // phase ends at the digest's own events.
        let mut idle_events = Vec::new();
        loop {
            // 30s per event: under loaded CI the SSE fixture turn can
            // overshoot shorter windows (same tolerance as cron tests)
            let evt = tokio::time::timeout(Duration::from_secs(30), h.events.recv())
                .await
                .unwrap()
                .unwrap();
            // tick work never emits Idle (headless surfaces would
            // exit); the digest's completion note ends the phase
            let done = matches!(&evt, Event::Note { message } if message == "context digested");
            idle_events.push(evt);
            if done {
                break;
            }
        }
        let idle_notes = notes_of(&idle_events);
        assert!(
            idle_notes
                .iter()
                .any(|n| n.starts_with("idle recap: Recap: fixing the parser")),
            "recap note: {idle_notes:?}"
        );
        assert!(
            idle_notes
                .iter()
                .any(|n| n.starts_with("idle compaction: context at 5% >= 1% threshold")),
            "compaction note: {idle_notes:?}"
        );
        assert!(
            idle_events
                .iter()
                .any(|e| matches!(e, Event::DigestStarted)),
            "idle digest ran: {idle_events:?}"
        );
        // the recap persists as a x.ka.recap Custom record
        let records = custom_records(&work, ka_strand::RECAP_NS);
        assert_eq!(records.len(), 1, "one recap record: {records:?}");
        assert_eq!(
            records[0]["text"], "Recap: fixing the parser; build is green.",
            "recap text: {records:?}"
        );
        // one idle action per quiet span: a second quiet window brings
        // ticks, but with no new turns nothing re-fires
        let requests_after_first_span = captured.lock().len();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            custom_records(&work, ka_strand::RECAP_NS).len(),
            1,
            "no recap without new turns"
        );
        assert_eq!(
            captured.lock().len(),
            requests_after_first_span,
            "no extra wire calls in the second quiet span"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// MODEL-05 · idle cache warming: a quiet span after a costly turn
    /// on a caching dialect fires exactly ONE silent max_tokens=1 touch
    /// (no turn events, only the note); a second quiet span without new
    /// turns stays silent.
    #[tokio::test]
    async fn idle_cache_warming_touches_once_per_new_turns() {
        use crate::engine::{StrandChoice, spawn_full};
        let _guard = IdleFactors::shrink();
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        // call order: the turn → the warm touch (no fast role → no
        // title/recap wire calls; no compaction)
        const WARM_SSE: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\".\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n",
        );
        let addr = serve_sse(vec![TURN_BIG_SSE, WARM_SSE], captured.clone());
        let work = title_workdir("warm");
        // priced caching dialect: the turn costs 5000/1M×$15 = $0.075 —
        // over the $0.05 warm gate
        let catalog = ka_dialect::Catalog::parse(&format!(
            "[dialects.\"test/main\"]\nwire = \"openai_chat\"\nbase_url = \
             \"http://{addr}/v1\"\ncontext = 100000\npriced = true\ncache = \"control\"\n\
             [dialects.\"test/main\".price]\ninput_per_mtok = 15\noutput_per_mtok = 30\n"
        ))
        .unwrap();
        let cfg = Config {
            model: Some("test/main".into()),
            roles: crate::config::Roles {
                default: None,
                fast: None,
                extra: Default::default(),
            },
            cwd: Some(work.display().to_string()),
            context: crate::config::Context {
                idle_minutes: Some(1),
                cache_warming: Some("idle".to_string()),
                ..Default::default()
            },
            ..Config::default()
        };
        let mut h = spawn_full(cfg, catalog, StrandChoice::New);
        h.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "fix the parser".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let first = drain_to_idle(&mut h).await;
        assert!(
            notes_of(&first).iter().all(|n| !n.contains("cache warmed")),
            "no warm before the idle span: {:?}",
            notes_of(&first)
        );
        // the quiet span: the warm note ends the phase; everything in
        // between is notes — a warm NEVER emits turn events
        let mut warm_events = Vec::new();
        loop {
            let evt = tokio::time::timeout(Duration::from_secs(10), h.events.recv())
                .await
                .unwrap()
                .unwrap();
            let done =
                matches!(&evt, Event::Note { message } if message.starts_with("cache warmed"));
            warm_events.push(evt);
            if done {
                break;
            }
        }
        assert!(
            warm_events.iter().all(|e| matches!(e, Event::Note { .. })),
            "warming stays silent (no TurnStarted/Delta/TurnFinished): {warm_events:?}"
        );
        // exactly one wire call beyond the turn, with max_tokens = 1
        {
            let reqs = captured.lock();
            assert_eq!(reqs.len(), 2, "turn + one warm touch: {reqs:?}");
            assert!(
                reqs[1].contains("\"max_tokens\":1"),
                "warm touch caps output at 1: {}",
                reqs[1]
            );
        }
        // one warm per new-turn batch: a second quiet span stays quiet
        let after = captured.lock().len();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(captured.lock().len(), after, "no warm without new turns");
        let _ = std::fs::remove_dir_all(&work);
    }

    /// MODEL-05 off-switch: `cache_warming = "off"` (default) never
    /// warms, even over an idle span with an expensive last turn.
    #[tokio::test]
    async fn cache_warming_off_stays_cold() {
        use crate::engine::{StrandChoice, spawn_full};
        let _guard = IdleFactors::shrink();
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let addr = serve_sse(vec![TURN_BIG_SSE], captured.clone());
        let work = title_workdir("warmoff");
        let catalog = ka_dialect::Catalog::parse(&format!(
            "[dialects.\"test/main\"]\nwire = \"openai_chat\"\nbase_url = \
             \"http://{addr}/v1\"\ncontext = 100000\npriced = true\ncache = \"control\"\n\
             [dialects.\"test/main\".price]\ninput_per_mtok = 15\noutput_per_mtok = 30\n"
        ))
        .unwrap();
        let cfg = Config {
            model: Some("test/main".into()),
            roles: crate::config::Roles {
                default: None,
                fast: None,
                extra: Default::default(),
            },
            cwd: Some(work.display().to_string()),
            context: crate::config::Context {
                idle_minutes: Some(1),
                cache_warming: Some("off".to_string()),
                ..Default::default()
            },
            ..Config::default()
        };
        let mut h = spawn_full(cfg, catalog, StrandChoice::New);
        h.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "fix the parser".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        drain_to_idle(&mut h).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            captured.lock().len(),
            1,
            "warming off: only the turn's own wire call"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// Lote 7 · cron end to end: the model arms a schedule through the
    /// `cron` hand, the settle tick fires it as a FULL self-prompt turn
    /// (the `cron fired` note precedes it). Tick + interval factors are
    /// shrunk like the idle tests.
    #[tokio::test]
    async fn cron_hand_tick_fires_a_full_self_prompt_turn() {
        use crate::engine::{StrandChoice, spawn_full};
        let _guard = IdleFactors::shrink();
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        // call order: arming turn → its tool-result turn → the cron-fired
        // self-prompt turn
        let addr = serve_sse(vec![CRON_CREATE_SSE, TURN_SSE, TURN_SSE], captured.clone());
        let work = title_workdir("cron-tick");
        let cfg = Config {
            model: Some("test/main".into()),
            cwd: Some(work.display().to_string()),
            ..Config::default()
        };
        let mut h = spawn_full(cfg, live_catalog(addr, None), StrandChoice::New);
        h.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "watch the CI every second".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let _ = drain_to_idle(&mut h).await;
        // within a couple of ticks the cron entry fires as its own turn
        let mut fired = false;
        let mut post_fire_turn = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(300), h.events.recv()).await {
                Ok(Some(Event::Note { message, .. })) => {
                    if message.starts_with("cron fired: cron-1") {
                        fired = true;
                    }
                }
                Ok(Some(Event::TurnFinished { .. })) if fired => {
                    post_fire_turn = true;
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) => panic!("engine ended before the cron fired"),
                Err(_) => {}
            }
        }
        assert!(fired, "cron fired note never arrived");
        assert!(post_fire_turn, "the fired prompt never ran as a turn");
        // the fired prompt went through the wire as a THIRD request
        let requests = captured.lock();
        assert!(
            requests.len() >= 3 && requests[2].contains("check the CI"),
            "the self-prompt turn must carry the cron prompt"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// Lote 7 · one-shot cron disarms after firing: a second quiet window
    /// brings ticks but never a second `cron fired` note.
    #[tokio::test]
    async fn cron_one_shot_fires_exactly_once() {
        use crate::engine::{StrandChoice, spawn_full};
        let _guard = IdleFactors::shrink();
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let addr = serve_sse(
            vec![CRON_ONE_SHOT_SSE, TURN_SSE, TURN_SSE],
            captured.clone(),
        );
        let work = title_workdir("cron-once");
        let cfg = Config {
            model: Some("test/main".into()),
            cwd: Some(work.display().to_string()),
            ..Config::default()
        };
        let mut h = spawn_full(cfg, live_catalog(addr, None), StrandChoice::New);
        h.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "remind me once".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let _ = drain_to_idle(&mut h).await;
        let mut firings = 0usize;
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(300), h.events.recv()).await {
                Ok(Some(Event::Note { message, .. }))
                    if message.starts_with("cron fired: cron-1") =>
                {
                    firings += 1;
                }
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => {} // quiet window: keep waiting until the deadline
            }
        }
        assert_eq!(firings, 1, "one-shot fires exactly once (got {firings})");
        let _ = std::fs::remove_dir_all(&work);
    }

    /// Lote 7 · monitor end to end: the model spawns a monitor through the
    /// hand; the settle tick surfaces the new-line note and the one-time
    /// exit note, and repeats neither.
    #[tokio::test]
    async fn monitor_hand_tick_reports_new_lines_and_exit_once() {
        use crate::engine::{StrandChoice, spawn_full};
        let _guard = IdleFactors::shrink();
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let addr = serve_sse(vec![MONITOR_SPAWN_SSE, TURN_SSE], captured.clone());
        let work = title_workdir("mon-tick");
        let cfg = Config {
            model: Some("test/main".into()),
            cwd: Some(work.display().to_string()),
            ..Config::default()
        };
        let mut h = spawn_full(cfg, live_catalog(addr, None), StrandChoice::New);
        h.commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "watch the build".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();
        let _ = drain_to_idle(&mut h).await;
        let mut new_lines_notes = 0usize;
        let mut exit_notes = 0usize;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(300), h.events.recv()).await {
                Ok(Some(Event::Note { message, .. })) => {
                    if message.starts_with("monitor mon-1: 2 new lines") {
                        new_lines_notes += 1;
                    }
                    if message.starts_with("monitor mon-1: exited (0)") {
                        exit_notes += 1;
                    }
                }
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => {} // quiet window: keep waiting until the deadline
            }
        }
        assert_eq!(new_lines_notes, 1, "exactly one new-lines note");
        assert_eq!(exit_notes, 1, "the exit note lands exactly once");
        // a further quiet window repeats neither note
        let extra = std::time::Instant::now() + Duration::from_millis(1500);
        while std::time::Instant::now() < extra {
            match tokio::time::timeout(Duration::from_millis(200), h.events.recv()).await {
                Ok(Some(Event::Note { message, .. })) => {
                    assert!(
                        !message.starts_with("monitor mon-1: exited"),
                        "exit note repeated"
                    );
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        let _ = std::fs::remove_dir_all(&work);
    }

    /// The parent's scripted speaker: one team-spawn tool call on the
    /// first request, plain text afterwards (team_contracts fixture
    /// shape, in-crate minimal).
    struct TeamSpawnOnce(std::sync::atomic::AtomicBool);

    impl Speaker for TeamSpawnOnce {
        fn speak<'a>(
            &'a self,
            _req: SpeakRequest,
            out: mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                if !self.0.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    out.send(StreamEvent::Call(ToolCall {
                        id: "c1".into(),
                        tool: "team".into(),
                        arguments: serde_json::json!({
                            "action": "spawn",
                            "name": "scout",
                            "prompt": "survey the repo"
                        }),
                    }))
                    .await
                    .ok();
                } else {
                    out.send(StreamEvent::Text("spawned the scout.".into()))
                        .await
                        .ok();
                }
                out.send(StreamEvent::Finished {
                    stop: Stop::Done,
                    usage: Usage::default(),
                })
                .await
                .ok();
            })
        }
    }

    /// A member speaker that records the model id of every request it
    /// receives — the probe for what model the member actually runs
    /// on. The vec is shared across instances (one spawn here, but the
    /// shape matches the team_contracts factory).
    struct ModelProbeMember(std::sync::Arc<parking_lot::Mutex<Vec<String>>>);

    impl Speaker for ModelProbeMember {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                self.0.lock().push(req.model_id);
                out.send(StreamEvent::Text("done".into())).await.ok();
                out.send(StreamEvent::Finished {
                    stop: Stop::Done,
                    usage: Usage::default(),
                })
                .await
                .ok();
            })
        }
    }

    /// P2: team members spawn on the parent's LIVE model — the member
    /// template is re-pinned from the live state AFTER `attach_strand`
    /// restored the strand's `settings.model`, so a resumed session
    /// whose strand baseline is `test/b` spawns members on `test/b`,
    /// not on the config baseline `test/a` the early template clone
    /// carried.
    #[tokio::test]
    async fn team_members_spawn_on_the_parent_live_model() {
        let dir = std::env::temp_dir().join(format!("ka-team-live-{}", std::process::id()));
        let data = std::env::temp_dir().join(format!("ka-team-live-data-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        // thread-local: the current-thread test runtime shares it with
        // the spawned member engines (the team_contracts redirect
        // pattern)
        ka_strand::set_data_dir_override(Some(data.clone()));

        // a resumable strand carrying a restored model baseline: the
        // Change record folds into Settings.model at open time
        let mut strand = ka_strand::StrandFile::create(&dir, None).unwrap();
        strand
            .append(ka_strand::Record::Change {
                id: ka_strand::new_record_id(),
                model: Some("test/b".into()),
                effort: None,
                mode: None,
                features: None,
                sandbox: None,
            })
            .unwrap();
        let strand_path = strand.path().unwrap().to_path_buf();
        drop(strand);

        let models = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let member_speakers: crate::hands::teams::MemberSpeakers = {
            let models = std::sync::Arc::clone(&models);
            std::sync::Arc::new(move || {
                (
                    ka_dialect::Wire::OpenaiChat,
                    std::sync::Arc::new(ModelProbeMember(std::sync::Arc::clone(&models)))
                        as std::sync::Arc<dyn Speaker>,
                )
            })
        };

        let catalog = ka_dialect::Catalog::parse(
            "[dialects.\"test/a\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n\
             [dialects.\"test/b\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
        )
        .unwrap();
        let cfg = Config {
            cwd: Some(dir.display().to_string()),
            model: Some("test/a".into()),
            ..Default::default()
        };
        let mut handle = spawn_with_team_speakers(
            cfg,
            catalog,
            StrandChoice::Path(strand_path),
            ka_dialect::Wire::OpenaiChat,
            std::sync::Arc::new(TeamSpawnOnce(std::sync::atomic::AtomicBool::new(false))),
            member_speakers,
        );
        handle
            .commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: "spawn a scout".into(),
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .unwrap();

        // the member's opening turn completes with exactly one note
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut note = String::new();
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(2), handle.events.recv()).await {
                Ok(Some(Event::Note { message }))
                    if message.starts_with(crate::hands::teams::TEAM_NOTE_PREFIX) =>
                {
                    note = message;
                    break;
                }
                Ok(Some(_)) => {}
                _ => break,
            }
        }
        assert!(
            note.starts_with("team · scout — "),
            "the member opening turn completed: {note:?}"
        );
        // the member ran on the parent's LIVE model — the
        // strand-restored settings.model — not on the config baseline
        // the early template clone carried
        let seen = models.lock();
        assert_eq!(
            seen.as_slice(),
            ["test/b"],
            "member request model ids (config baseline is test/a)"
        );
        ka_strand::set_data_dir_override(None);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&data);
    }
}

/// CTX-06 (unit): the loop fire gate — armed, queue drained, interval
/// elapsed; a fire consumes the cycle and re-arms the interval.
#[test]
fn loop_prompt_due_gates_on_interval_and_queue() {
    let mut state = EngineState::from(Config::default());
    // disarmed: nothing due
    assert!(loop_prompt_due(&mut state).is_none());
    // armed, interval not elapsed: not due
    state.loop_mode = Some(LoopRuntime {
        task: "tick work".into(),
        interval: Duration::from_secs(60),
        last_settle: std::time::Instant::now(),
    });
    assert!(loop_prompt_due(&mut state).is_none());
    // queued user input drains first — the loop never interleaves
    if let Some(l) = state.loop_mode.as_mut() {
        l.interval = Duration::ZERO;
    }
    state.queue.push_back("user input".into());
    assert!(loop_prompt_due(&mut state).is_none());
    state.queue.clear();
    // interval elapsed: due, and the fire consumes the cycle
    assert_eq!(loop_prompt_due(&mut state).as_deref(), Some("tick work"));
    if let Some(l) = state.loop_mode.as_mut() {
        l.interval = Duration::from_secs(60);
    }
    assert!(loop_prompt_due(&mut state).is_none());
}

/// P1: every live mode switch mirrors into the shared cell the code
/// router's gate reads ([`ModeSlot`]) — `set_live_mode` is the single
/// mutation path (SetMode, strand restore, hook steering all call it).
#[test]
fn set_live_mode_updates_the_shared_cell() {
    let mut state = EngineState::from(Config::default());
    assert_eq!(*state.mode_slot.read(), state.mode);
    state.set_live_mode(Mode::Readonly);
    assert_eq!(state.mode, Mode::Readonly);
    assert_eq!(*state.mode_slot.read(), Mode::Readonly);
    state.set_live_mode(Mode::Allow);
    assert_eq!(*state.mode_slot.read(), Mode::Allow);
}
