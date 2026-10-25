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
    AskId, AskQuestion, Command, ContextMeter, DeltaKind, ErrorClass, Event, McpSummary, Mode,
    Stop, Usage,
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
}

/// Resolve one role selector through the same catalog path as the main
/// model: it must parse, and its `vendor/model` must exist in the
/// catalog (locals arrive via the discovery overlay before the engine
/// starts). Unresolvable → `None`.
fn resolve_role(catalog: &ka_dialect::Catalog, selector: &str) -> Option<String> {
    let id = ka_dialect::parse_selector(selector).ok()?.model_id();
    catalog.get(&id).is_some().then_some(id)
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
    spawn_speaked(config, catalog, strand, None)
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
    spawn_speaked(config, catalog, strand, Some((wire, speaker)))
}

fn spawn_speaked(
    config: Config,
    catalog: ka_dialect::Catalog,
    strand: StrandChoice,
    speaker: Option<(
        ka_dialect::Wire,
        std::sync::Arc<dyn ka_dialect::speaker::Speaker>,
    )>,
) -> EngineHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (evt_tx, evt_rx) = mpsc::channel(256);
    tokio::spawn(async move {
        if let Err(e) = run(cmd_rx, evt_tx, config, catalog, strand, speaker).await {
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

/// Resolve a strand choice into an open strand (creating when allowed).
fn resolve_strand(
    choice: &StrandChoice,
    cwd: &std::path::Path,
) -> std::io::Result<ka_strand::StrandFile> {
    match choice {
        StrandChoice::New => {
            let repo = repo_snapshot(cwd);
            ka_strand::StrandFile::create(cwd, repo)
        }
        StrandChoice::Latest => match ka_strand::latest(cwd)? {
            Some(summary) => ka_strand::StrandFile::open(&summary.path),
            None => {
                let repo = repo_snapshot(cwd);
                ka_strand::StrandFile::create(cwd, repo)
            }
        },
        StrandChoice::Path(path) => ka_strand::StrandFile::open(path),
    }
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
fn history_from_records(
    records: &[ka_strand::Record],
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
                            images: Vec::new(),
                        })
                        .collect(),
                    images: Vec::new(),
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
                if let Some(pos) = ids.iter().position(|i| i == kept_from) {
                    let keep_msgs = pos + 1;
                    out.truncate(keep_msgs);
                    ids.truncate(keep_msgs);
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
    queue: VecDeque<String>,
    steers: Vec<String>,
    /// Record ids of persisted history messages, aligned with
    /// voice.history[..record_ids.len()] (mod digest truncation).
    record_ids: Vec<ka_protocol::RecordId>,
    /// Checkpoints taken this session: (commit id, timestamp).
    checkpoints: Vec<(String, String)>,
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
    /// `[verify] test` command; runs after edit-carrying turns.
    verify_test: Option<String>,
    /// One automatic verify-fix round per user prompt (loop guard).
    verify_round_done: bool,
    /// `[pokes] max`: auto-poke rounds allowed per user prompt.
    poke_max: u32,
    /// Auto-poke rounds spent on the current user prompt.
    pokes_used: u32,
    /// Pending-todo set observed at the last poke check: an unchanged
    /// set means no progress — poking stops instead of burning rounds.
    poke_pending: Option<Vec<String>>,
    /// `!` passthrough outputs waiting to ride the next user prompt.
    shell_context: Vec<String>,
}
impl From<Config> for EngineState {
    fn from(c: Config) -> Self {
        let mode = c.effective_mode();
        Self {
            model: c.model,
            effort: c.effort,
            mode,
            queue: VecDeque::new(),
            steers: Vec::new(),
            record_ids: Vec::new(),
            checkpoints: Vec::new(),
            guards: crate::voice::GuardRuntime::new(c.guards.spend_usd, c.guards.context_pct),
            roles: EngineRoles::default(),
            needs_title: false,
            auto_commit: c.git.auto_commit.unwrap_or(false),
            poke_max: c.pokes.max.unwrap_or(3),
            verify_test: c.verify.test.clone(),
            pokes_used: 0,
            poke_pending: None,
            verify_round_done: false,
            shell_context: Vec::new(),
        }
    }
}

type DynError = Box<dyn std::error::Error + Send + Sync>;

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
    /// Watchdog/refresh bookkeeping from MCP supervisors.
    maintenance: mpsc::Receiver<crate::mcp::Maintenance>,
    /// [tools.mcp] discovery = "lazy": one mcp_call hand instead of a
    /// hand per server tool.
    mcp_lazy: bool,
    /// Background delegate-task registry (/tasks rows).
    agent_tasks: std::sync::Arc<crate::hands::tasks::AgentTaskTable>,
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
}

async fn run(
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    config: Config,
    catalog: ka_dialect::Catalog,
    strand_choice: StrandChoice,
    speaker: Option<(
        ka_dialect::Wire,
        std::sync::Arc<dyn ka_dialect::speaker::Speaker>,
    )>,
) -> Result<(), DynError> {
    let cwd = config
        .cwd
        .clone()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    // safe mode: config-file hooks are customizations too — built-ins,
    // config chain, rules, and auth stay
    let mcp_servers = if crate::conventions::bare_mode() {
        Vec::new()
    } else {
        config.mcp.clone()
    };
    let mode = config.effective_mode();
    let max_steps = config.effective_max_steps();
    let rules = config.rules.clone();
    let allowed_tools = config.permissions.allow.clone();
    let hooks = if crate::conventions::bare_mode() {
        Vec::new()
    } else {
        config.hooks.clone()
    };
    // roles resolve once at engine start, through the same catalog the
    // main model uses (discovery overlays already applied by the caller)
    let roles = resolve_roles(&catalog, &config.roles, config.model.as_deref());
    let pathfinder_catalog = catalog.clone();
    let search_provider = config.effective_search();
    let web_allow_private = config.effective_web_allow_private();
    let mut voice = Voice::new(catalog, cwd.clone(), mode, max_steps);
    if let Some((wire, speaker)) = speaker {
        voice = voice.with_speaker(wire, speaker);
    }
    // [features] disable: validated once, primed into the shared toggle
    // slot every layer of this feature reads (specs filter, admit guard,
    // prompt build, the dynamic hands)
    let disabled_specs = crate::features::parse_specs(&config.features.disable)
        .map_err(|e| -> DynError { format!("[features] disable: {e}").into() })?;
    let features = crate::features::FeatureToggles::slot(disabled_specs);
    voice.set_feature_slot(features.clone());
    voice.set_rules(rules);
    voice.set_verify(config.verify.clone());
    // [prompt] identity_file: same trust class as convention discovery
    // (a user-authored local file) — inert in safe mode
    voice.set_identity_file(if crate::conventions::bare_mode() {
        None
    } else {
        config.prompt.identity_file.clone()
    });
    voice.set_allowed_tools(allowed_tools);
    voice.set_hooks(hooks);
    voice.set_bash_background_ms(config.effective_bash_background_after_ms());
    voice.set_fallbacks(config.fallback.models.clone());
    voice.set_max_image_mb(config.effective_max_image_mb());
    voice.set_context_promote(config.effective_context_promote());
    let sandbox_policy = ka_sandbox::policy_from_config(&config.sandbox.to_policy_config(), &cwd)?;
    voice.set_sandbox(sandbox_policy);
    // [guards] auto_review: the fast-role reviewer pre-screens reviewable
    // exec asks (opt-in; inert in safe mode — hooks-class customization)
    let auto_review = config.guards.auto_review.unwrap_or(false);
    voice.set_auto_review(
        auto_review && !crate::conventions::bare_mode(),
        roles.fast.clone(),
    );
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
    let mut state = EngineState::from(config);
    state.roles = roles;
    // strand attach also restores any recorded feature-toggle snapshot
    // (over the config baseline, like model/mode) — so the spawn
    // decisions below honor it
    let strand = attach_strand(&events, &mut state, &mut voice, &cwd, &strand_choice).await?;
    // strand-restored sandbox override: recompute the live policy
    if let Some(mode) = voice.features_snapshot().1 {
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
    let mut ctx = Ctx {
        cwd,
        events,
        state,
        voice,
        strand,
        mcp_shared: Vec::new(),
        maintenance: maintenance_rx,
        mcp_lazy,
        agent_tasks: agent_tasks.clone(),
        debug: None,
        mcp_servers_cfg: mcp_servers.clone(),
        maintenance_tx: maintenance_tx.clone(),
        lsp: lsp.clone(),
        lsp_cfg: lsp_cfg.clone(),
        debug_cfg: debug_cfg.clone(),
        sandbox_cfg,
        mcp_prompts: Vec::new(),
    };
    // markdown agents: .ka/agents/*.md etc. become one `delegate` hand
    // (empty in safe mode). The hands register even when `agents` is
    // toggled off — disabling hides, so a re-enable is pure set
    // membership.
    let agents = if crate::conventions::bare_mode() {
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
            ),
        ));
        ctx.voice
            .push_hand(std::sync::Arc::new(crate::hands::tasks::TasksHand::new(
                agent_tasks.clone(),
            )));
    }
    // memory inbox: the model stages durable notes for user review
    // (customization tier — inert in safe mode)
    if !crate::conventions::bare_mode() {
        ctx.voice
            .push_hand(std::sync::Arc::new(crate::hands::memory::RememberHand));
    }
    // LSP hands: navigation (symbols/definition/references/diagnostics)
    // plus write-through on [lsp] write_through — only when the tier is
    // both configured and not toggled off
    if lsp_cfg.enable == Some(true)
        && lsp_cfg.commands.as_ref().is_some_and(|c| !c.is_empty())
        && !features.read().is_disabled(&ka_protocol::FeatureSpec::Lsp)
    {
        lsp.start_all();
        for hand in lsp_hands(lsp.clone(), &lsp_cfg) {
            ctx.voice.push_hand(hand);
        }
    }
    // debug tier: manager + hand only when configured and not toggled
    // off (runtime enable constructs them on demand)
    if debug_cfg.enable == Some(true)
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

    // web search + fetch hands (search only when a provider is set)
    for hand in crate::hands::web::hands(search_provider, web_allow_private) {
        ctx.voice.push_hand(hand);
    }
    // lazy MCP: one hand fronts every server (listing + calls)
    if ctx.mcp_lazy && !ctx.mcp_shared.is_empty() {
        ctx.voice
            .push_hand(std::sync::Arc::new(crate::mcp::McpCallHand::new(
                ctx.mcp_shared.clone(),
                features.clone(),
            )));
    }
    // browse hand over every server: resources + prompts discovery
    if !ctx.mcp_shared.is_empty() {
        ctx.voice
            .push_hand(std::sync::Arc::new(crate::mcp::McpMetaHand::new(
                ctx.mcp_shared.clone(),
                features.clone(),
            )));
    }
    // One bootstrap inventory card replaces the old ad-hoc notes: the
    // surface renders tools/MCP/agents/skills compactly at startup.
    emit_inventory(&ctx).await;
    // surfaces initialize the todo section immediately, before any turn
    ctx.events
        .send(Event::Todos { items: Vec::new() })
        .await
        .ok();
    // session_start hook (roadmap 9.2): strand attached, inventory out
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
    loop {
        tokio::select! {
            maybe = commands.recv() => {
                let Some(cmd) = maybe else { break };
                handle_command(cmd, &mut commands, &mut ctx).await?;
            }
            maybe = ctx.maintenance.recv() => {
                if let Some(update) = maybe {
                    handle_maintenance(update, &mut ctx).await;
                }
            }
        }
    }
    // session shutdown: no backgrounded bash job may outlive the engine
    // detached jobs intentionally SURVIVE session exit; adopt any
    // survivors from earlier sessions into this table
    if let Some(path) = crate::hands::jobs::default_jobs_file() {
        ctx.voice.jobs().set_path(path);
    }
    // session_end hook (roadmap 9.2): the command channel closed; the
    // surface may already be gone, so steering surfaces best-effort
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
    Ok(())
}

/// The single, exhaustive command dispatcher. Every `Command` variant is
/// handled (or explicitly rejected) here — no unreachable arms, no
/// routing split. Arms that need to poll the surface mid-turn (Prompt)
/// also receive the receiver.
/// Apply one MCP supervisor update: swap the server's hands for the
/// fresh tool set and tell the surface.
async fn handle_maintenance(update: crate::mcp::Maintenance, ctx: &mut Ctx) {
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
                    let hands: Vec<std::sync::Arc<dyn crate::hands::Hand>> = tools
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
    if let Some(shared) = ctx.mcp_shared.iter().find(|s| s.name() == server).cloned() {
        if !ctx.mcp_lazy {
            let hands: Vec<std::sync::Arc<dyn crate::hands::Hand>> = tools
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
    // cache hit: register the remembered hands immediately — startup
    // never blocks on the handshake, the tool list stays stable across
    // runs (provider prompt-cache stability) — and verify in the
    // background; the watchdog spawned alongside owns retries
    if let Some(cached) = crate::mcp_cache::load_tools(&cfg) {
        let count = cached.len();
        let shared = crate::mcp::McpShared::placeholder(cfg.clone(), cached);
        if !ctx.mcp_lazy {
            for tool in shared.tools() {
                ctx.voice
                    .push_hand(std::sync::Arc::new(crate::mcp::McpHand::new(
                        tool,
                        shared.clone(),
                    )));
            }
        }
        ctx.mcp_shared.push(shared.clone());
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
    let connected = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        crate::mcp::McpClient::spawn_connect(&cfg),
    )
    .await;
    match connected {
        Ok(Ok((client, tools))) => {
            let tool_count = tools.len();
            // remember the advertised list: the next startup registers
            // from cache and never waits on this handshake
            crate::mcp_cache::store_tools(&cfg, &tools);
            let shared = crate::mcp::McpShared::new(cfg.clone(), client, tools.clone());
            // eager mode: every tool becomes its own hand; lazy mode
            // fronts the server through the single mcp_call hand
            if !ctx.mcp_lazy {
                for tool in tools {
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
    let skills: Vec<String> = crate::conventions::discover_skills(&ctx.cwd)
        .into_iter()
        .filter(|s| ctx.voice.feature_slot().read().skill_enabled(&s.name))
        .map(|s| s.name)
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
    let (disabled, _) = ctx.voice.features_snapshot();
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
/// resume), announce it, and refresh the inventory card.
async fn persist_features(ctx: &mut Ctx) {
    let (disabled, sandbox) = ctx.voice.features_snapshot();
    let _ = ctx.strand.append(ka_strand::Record::Change {
        id: ka_strand::new_record_id(),
        model: None,
        effort: None,
        mode: None,
        features: Some(disabled.clone()),
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
                    || ctx.voice.mode() == Mode::Plan
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
                let poke = format!(
                    "(ka auto-poke {}/{}) Your todo list still has {} pending item(s):\n{list}\n\
                     Continue the work: finish the remaining items, re-verify anything you \
                     marked done without evidence (a confidence jump is a claim, not a \
                     measurement), keep the todo list current, and stop with a short summary \
                     when everything is done.",
                    ctx.state.pokes_used,
                    ctx.state.poke_max,
                    pending.len()
                );
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
                    Ok((tools, delta)) => {
                        if delta != 0 {
                            let hands: Vec<std::sync::Arc<dyn crate::hands::Hand>> = tools
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
            ctx.events.send(Event::ModelChanged { selector }).await?;
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
        Command::SetMode { mode } => {
            ctx.state.mode = mode;
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
        Command::ContextBreakdown => {
            let parts = ctx.voice.context_breakdown();
            let window = ctx.voice.window_tokens();
            ctx.events
                .send(Event::ContextBreakdown { parts, window })
                .await?;
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::ListTasks => {
            // /tasks dashboard rows: background delegates + bash jobs
            let mut rows = ctx.agent_tasks.rows();
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
            match crate::checkpoint::snapshot(&ctx.cwd) {
                Ok(id) => {
                    let ts = ka_strand::now_rfc3339();
                    ctx.state.checkpoints.push((id.clone(), ts.clone()));
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
                let message = if ctx.state.checkpoints.is_empty() {
                    "no checkpoints in this session yet".to_string()
                } else {
                    ctx.state
                        .checkpoints
                        .iter()
                        .map(|(cid, ts)| format!("{}  {ts}", crate::checkpoint::short(cid)))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                ctx.events.send(Event::Note { message }).await.ok();
            } else {
                let resolved = ctx.state.checkpoints.iter().find(|(cid, _)| {
                    cid == &id
                        || crate::checkpoint::short(cid) == id.trim()
                        || cid.starts_with(id.trim())
                });
                match resolved.cloned() {
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
                                "restore checkpoint {} ({ts})? overwrites working tree",
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
            match ctx.voice.rewind(turns) {
                Some(kept) => {
                    // kept = index of the dropped user message; keep
                    // everything strictly before it
                    let keep_idx = kept.saturating_sub(1);
                    let kept_from = ctx
                        .state
                        .record_ids
                        .get(keep_idx)
                        .cloned()
                        .unwrap_or_else(ka_strand::new_record_id);
                    let _ = ctx.strand.append(ka_strand::Record::Rewind {
                        id: ka_strand::new_record_id(),
                        kept_from,
                    });
                    ctx.state.record_ids.truncate(keep_idx + 1);
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
    let mut strand =
        resolve_strand(choice, cwd).map_err(|e| -> DynError { format!("strand: {e}").into() })?;
    // interrupted-turn synthesis — observable when something was recovered
    let recovered = strand.synthesize_aborted().unwrap_or(false);
    // checkpoints and guard latches are session-scoped; re-arm them from
    // the strand we just attached to
    state.checkpoints.clear();
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
        state.mode = settings.mode.unwrap_or_default();
        voice.set_mode(state.mode);
    }
    voice.pathfinder_slot().write().model = state.model.clone();
    voice.set_effort(state.effort.map(|e| effort_name(e).to_string()));
    let (history, ids, digest) = history_from_records(strand.records());
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
    // that never recorded one keeps the current set. Specs that no
    // longer parse (grammar drift) are skipped with a note.
    if let Some(list) = &settings.features {
        let parsed: Vec<ka_protocol::FeatureSpec> =
            list.iter().filter_map(|s| s.parse().ok()).collect();
        let skipped: Vec<String> = list
            .iter()
            .filter(|s| s.parse::<ka_protocol::FeatureSpec>().is_err())
            .cloned()
            .collect();
        voice.restore_feature_snapshot(parsed, settings.sandbox.clone());
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
    let (disabled, sandbox) = voice.features_snapshot();
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
    if voice.mode() != ka_protocol::Mode::Plan || voice.last_stop() != Stop::Done {
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
        .find(|m| m.role == ka_dialect::speaker::TurnRole::User)
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
    let shaken = voice.shake(240);
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
    // background candidate so the next real digest is instant
    if !voice.context_pressure(window) && voice.context_pressure_frac(window, 80) {
        voice.start_speculative(state.roles.fast.as_deref());
    }
    let mut digests = 0;
    while voice.context_pressure(window) && digests < 3 {
        digests += 1;
        match run_digest(voice, state, strand, events, None).await {
            DigestResult::Digested => continue,
            DigestResult::NoModel => break,
        }
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
    // a ready speculative candidate with a matching watermark skips the
    // synchronous summarize entirely
    if focus.is_none() {
        if let Some(summary) = voice.take_speculative().await {
            let _ = voice.apply_digest(summary, ratio);
            persist_delta(voice, state, strand);
            reinject_after_digest(voice, events).await;
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
            persist_delta(voice, state, strand);
            reinject_after_digest(voice, events).await;
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
            persist_delta(voice, state, strand);
            reinject_after_digest(voice, events).await;
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
        // the commit (and its notes) must land first
        voice.pre_finish = state.auto_commit.then(|| {
            let events = events.clone();
            let cwd = cwd.to_path_buf();
            let head = prompt_head.clone();
            let fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
                Box::pin(async move { auto_commit(&events, &cwd, &head).await });
            parking_lot::Mutex::new(fut)
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
    let _ = strand.append(ka_strand::Record::Usage {
        id: ka_strand::new_record_id(),
        cost: usage.cost,
        input: usage.input,
        output: usage.output,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
    });
    persist_delta(voice, state, strand);
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
        state.mode = mode;
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
                    Some(_) => {}
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

/// Persist any pending digest (as a Digest record) and the history delta.
fn persist_delta(voice: &mut Voice, state: &mut EngineState, strand: &mut ka_strand::StrandFile) {
    if let Some((summary, kept, _rev)) = voice.take_pending_digest() {
        let kept_from = state
            .record_ids
            .get(kept)
            .cloned()
            .unwrap_or_else(ka_strand::new_record_id);
        let _ = strand.append(ka_strand::Record::Digest {
            id: ka_strand::new_record_id(),
            summary,
            kept_from,
        });
        state.record_ids = state.record_ids.get(kept..).unwrap_or(&[]).to_vec();
    }
    while state.record_ids.len() < voice.history.len() {
        let record = record_from_message(&voice.history[state.record_ids.len()]);
        let _ = strand.append(record.clone());
        if let Some(id) = record.id() {
            state.record_ids.push(id.clone());
        } else {
            break;
        }
    }
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

    // Settling: unhandled steers become queued input so they are not lost.
    for steer in state.steers.drain(..) {
        state.queue.push_back(steer);
    }
    let est_out: u64 = chunks
        .iter()
        .map(|c: &String| c.len() as u64)
        .sum::<u64>()
        .div_ceil(4);
    history.push(ka_dialect::speaker::TurnMessage::assistant(chunks.concat()));
    if state.auto_commit {
        auto_commit(events, cwd, &text).await;
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

/// `[git] auto_commit`: stage the whole worktree (`git add -A` —
/// untracked files and anything already staged included) and commit
/// with a deterministic one-line message (`ka: <first prompt line>`, 72
/// chars max — no model roundtrip). Outside a repo: silent no-op. Any
/// git failure is an advisory note, never a turn failure.
async fn auto_commit(events: &mpsc::Sender<Event>, cwd: &std::path::Path, prompt: &str) {
    let git = |args: &[&str]| {
        tokio::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
    };
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
    let first_line: String = prompt
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(72)
        .collect();
    let msg: String = format!("ka: {first_line}").chars().take(72).collect();
    match git(&[
        "-c",
        "user.name=ka",
        "-c",
        "user.email=ka@local",
        "commit",
        "-m",
        &msg,
    ])
    .await
    {
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

    use ka_protocol::{Command, ErrorClass, Event, Stop};
    use std::time::Duration;
    use tokio::sync::mpsc;

    use crate::config::Config;
    use crate::engine::{EngineHandle, spawn};

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
        assert!(skills.contains(&"demo".to_string()), "skills: {skills:?}");
        assert!(skills.contains(&"second".to_string()), "skills: {skills:?}");
        assert_eq!(
            mcp,
            &[ka_protocol::McpSummary {
                name: "ghost".into(),
                ok: false,
                tools: 0,
            }]
        );
        // 7 built-ins + todo + jobs + delegate + web_fetch (search
        // registers only with a configured [[search]] provider)
        assert_eq!(tools.len(), 13, "tools: {tools:?}");
        assert!(tools.contains(&"delegate".to_string()), "tools: {tools:?}");
        assert!(tools.contains(&"todo".to_string()), "tools: {tools:?}");
        assert!(tools.contains(&"jobs".to_string()), "tools: {tools:?}");
        // and the ad-hoc notes stay gone
        assert!(seen.iter().all(
            |e| !matches!(e, Event::Note { message } if message.contains("agents available") || message.contains("tool(s)"))
        ));
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

    #[test]
    fn fork_records_parent_linkage() {
        let work = std::env::temp_dir().join(format!("ka-fork-parent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        let mut parent = ka_strand::StrandFile::create(&work, None).unwrap();
        parent
            .append(ka_strand::Record::Message {
                id: ka_strand::new_record_id(),
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
        let (history, _, _) = super::history_from_records(&[record]);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].thinking.as_deref(), Some("pondering"));
    }
}
