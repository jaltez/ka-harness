//! The delegate tool: run a user-defined markdown agent as a subagent.
//! Same machinery as pathfinder — a nested read-only voice with the
//! agent's markdown body as its system prompt — generalized over
//! `.ka/agents/*.md` definitions.

use std::pin::Pin;

use Future;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{Clearance, Hand, HandContext, HandDef, ToolOutput};
use crate::agents::AgentDef;
use crate::voice::{GuardRuntime, Voice};

/// The delegate tool: one hand over every discovered agent.
pub struct DelegateHand {
    agents: Vec<AgentDef>,
    /// Shared feature toggles: `agent:<name>` drops an agent from the
    /// roster (def + execute) without touching the others.
    features: crate::features::FeatureSlot,
    /// Shared catalog/model bootstrap (the engine-owned pathfinder slot —
    /// the single source of truth for the nested voices' speaker).
    source: Arc<parking_lot::RwLock<super::pathfinder::PathfinderSource>>,
    /// The parent session's permission mode (gates isolated agents).
    parent_mode: ka_protocol::Mode,
    /// Background-task registry (`background: true` delegates).
    tasks: Arc<super::tasks::AgentTaskTable>,
    /// Event sink for completion notes.
    events: mpsc::Sender<ka_protocol::Event>,
}

impl DelegateHand {
    /// New hand over discovered agents and the shared subagent source.
    pub fn new(
        agents: Vec<AgentDef>,
        source: Arc<parking_lot::RwLock<super::pathfinder::PathfinderSource>>,
        parent_mode: ka_protocol::Mode,
        tasks: Arc<super::tasks::AgentTaskTable>,
        events: mpsc::Sender<ka_protocol::Event>,
        features: crate::features::FeatureSlot,
    ) -> Self {
        Self {
            agents,
            features,
            source,
            parent_mode,
            tasks,
            events,
        }
    }

    /// Agents visible to the model right now (`agent:<name>` toggles).
    fn visible(&self) -> Vec<&AgentDef> {
        let features = self.features.read();
        self.agents
            .iter()
            .filter(|a| features.agent_enabled(&a.name))
            .collect()
    }

    fn find(&self, name: &str) -> Option<&AgentDef> {
        self.agents.iter().find(|a| a.name == name)
    }
}

impl Hand for DelegateHand {
    fn def(&self) -> HandDef {
        // the roster reflects live `agent:<name>` toggles: disabled
        // agents vanish from the listing and the enums alike
        let agents = self.visible();
        let mut listing = String::from(
            "Delegate a self-contained subtask to a named subagent. Available agents:\n",
        );
        for a in &agents {
            let desc = if a.description.is_empty() {
                "(no description)"
            } else {
                &a.description
            };
            let model_note = a
                .model
                .as_deref()
                .filter(|m| !m.is_empty())
                .map(|m| format!(" [model: {m}]"))
                .unwrap_or_default();
            listing.push_str(&format!("- {}: {desc}{model_note}\n", a.name));
        }
        listing.push_str(
            "The agent runs with read-only tools and returns a dense summary. \
Or pass `tasks` to run several agents concurrently (up to 16 per call, 4 at a time; results return in order). \
Pass `background: true` (single agent+task only) to start it detached and keep working — \
track it with the tasks tool.",
        );
        HandDef {
            name: "delegate".to_string(),
            description: listing,
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "enum": agents.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
                        "description": "Which agent to run (required unless `tasks` is given)"
                    },
                    "task": {
                        "type": "string",
                        "description": "The complete, self-contained task for the agent (required unless `tasks` is given)"
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Run detached and return immediately (single agent+task only); default false"
                    },
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "agent": {
                                    "type": "string",
                                    "enum": agents.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
                                    "description": "Which agent to run"
                                },
                                "task": {
                                    "type": "string",
                                    "description": "The complete, self-contained task for this agent"
                                }
                            },
                            "required": ["agent", "task"]
                        },
                        "description": "Fan out: run several agents concurrently (max 4 at a time). Mutually exclusive with agent+task."
                    }
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
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let agent_arg = args.get("agent").and_then(Value::as_str);
            let task_arg = args.get("task").and_then(Value::as_str);
            let tasks_arg = args.get("tasks").filter(|v| !v.is_null());

            if tasks_arg.is_some() && (agent_arg.is_some() || task_arg.is_some()) {
                return ToolOutput::err("delegate: pass either agent+task or tasks, not both");
            }
            if let Some(tasks) = tasks_arg {
                return self.fanout(tasks, ctx).await;
            }

            let Some(agent_name) = agent_arg else {
                return ToolOutput::err("delegate: missing required 'agent'");
            };
            let Some(task) = task_arg else {
                return ToolOutput::err("delegate: missing required 'task'");
            };
            let Some(def) = self.find(agent_name) else {
                let known: Vec<String> =
                    self.visible().into_iter().map(|a| a.name.clone()).collect();
                return ToolOutput::err(format!(
                    "delegate: unknown agent '{agent_name}' (known: {})",
                    known.join(", ")
                ));
            };
            if !self.features.read().agent_enabled(agent_name) {
                return ToolOutput::err(format!(
                    "delegate: agent '{agent_name}' is disabled by a session feature toggle \
                     (`agent:{agent_name}`); it is unavailable until the user re-enables it"
                ));
            }
            // background: register, spawn, return immediately — the
            // model tracks it via the tasks hand (single tasks only)
            if args.get("background").and_then(Value::as_bool) == Some(true) {
                let id = self.tasks.register(agent_name, task);
                let roster = Some(self.tasks.rows_except(Some(id)));
                let def = def.clone();
                let task = task.to_string();
                let cwd = ctx.cwd.clone();
                let source = self.source.read().clone();
                let parent_mode = self.parent_mode;
                let tasks = self.tasks.clone();
                let events = self.events.clone();
                let tasks_in = tasks.clone();
                let handle = tokio::spawn(async move {
                    let (outcome, branch, cost) =
                        run_agent(def, task.clone(), cwd, source, parent_mode, roster, |tx| {
                            tasks_in.attach_cmds(id, tx)
                        })
                        .await;
                    tasks_in.set_cost(id, cost);
                    if let Some(branch) = &branch {
                        tasks_in.set_branch(id, branch.clone());
                    }
                    tasks_in.finish(id, outcome.clone());
                    let (label, note) = match &outcome {
                        Ok(summary) => ("finished", truncate_for_note(summary, 200)),
                        Err(reason) => ("failed", truncate_for_note(reason, 200)),
                    };
                    events
                        .send(ka_protocol::Event::Note {
                            message: format!(
                                "background t-{id} {label} — full result via the tasks tool: {note}"
                            ),
                        })
                        .await
                        .ok();
                });
                tasks.attach(id, handle);
                return ToolOutput::ok(format!(
                    "background task t-{id} started (agent {agent_name}); continue other work \
                     and check the tasks tool for the result"
                ));
            }
            let roster = Some(self.tasks.rows_except(None));
            let source = self.source.read().clone();
            let (outcome, _branch, _cost) = run_agent(
                def.clone(),
                task.to_string(),
                ctx.cwd.clone(),
                source,
                self.parent_mode,
                roster,
                |_| {},
            )
            .await;
            match outcome {
                Ok(summary) => ToolOutput::ok(summary),
                Err(e) => ToolOutput::err(e),
            }
        })
    }
}

/// Cap a note to `cap` chars with an ellipsis.
fn truncate_for_note(text: &str, cap: usize) -> String {
    if text.chars().count() > cap {
        let cut: String = text.chars().take(cap).collect();
        format!("{cut}…")
    } else {
        text.to_string()
    }
}

impl DelegateHand {
    /// Fan out: run every task concurrently (semaphore cap 4) and
    /// report `## task N — <agent>` sections in input order under a
    /// wall-time header. Per-task failures stay inline; only
    /// structural problems (bad shape, empty list) fail the call.
    async fn fanout(&self, tasks: &Value, ctx: &HandContext) -> ToolOutput {
        let Some(list) = tasks.as_array() else {
            return ToolOutput::err("delegate: tasks must be an array of {agent, task}");
        };
        if list.is_empty() {
            return ToolOutput::err("delegate: tasks must not be empty");
        }
        if list.len() > 16 {
            return ToolOutput::err(format!(
                "delegate: tasks supports up to 16 entries (got {}); split the work across calls",
                list.len()
            ));
        }
        let mut picked: Vec<Result<FanoutJob, String>> = Vec::with_capacity(list.len());
        for (i, entry) in list.iter().enumerate() {
            let Some(name) = entry.get("agent").and_then(Value::as_str) else {
                return ToolOutput::err(format!("delegate: tasks[{i}] needs an 'agent'"));
            };
            let Some(task) = entry.get("task").and_then(Value::as_str) else {
                return ToolOutput::err(format!("delegate: tasks[{i}] needs a 'task'"));
            };
            match self.find(name) {
                Some(def) if self.features.read().agent_enabled(name) => {
                    picked.push(Ok(FanoutJob {
                        def: def.clone(),
                        task: task.to_string(),
                        cwd: ctx.cwd.clone(),
                        source: self.source.read().clone(),
                        parent_mode: self.parent_mode,
                    }))
                }
                Some(_) => picked.push(Err(format!(
                    "agent '{name}' is disabled by a session feature toggle"
                ))),
                None => picked.push(Err(format!("unknown agent '{name}'"))),
            }
        }

        let started = std::time::Instant::now();
        let sem = Arc::new(tokio::sync::Semaphore::new(4));
        let mut handles = Vec::with_capacity(picked.len());
        for entry in picked {
            let sem = sem.clone();
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire().await;
                match entry {
                    Ok(job) => {
                        let name = job.def.name.clone();
                        let out = run_agent(
                            job.def,
                            job.task,
                            job.cwd,
                            job.source,
                            job.parent_mode,
                            None,
                            |_| (),
                        )
                        .await
                        .0;
                        (Some(name), out)
                    }
                    Err(reason) => (None, Err(reason)),
                }
            }));
        }
        // Cancelling the fanout future (an engine abort drops it) must
        // abort still-running workers instead of leaving them detached:
        // dropping a worker's frame drops its command sender, which ends
        // the nested agent promptly — the same contract the single-agent
        // path honors.
        type FanoutOutcome = (Option<String>, Result<String, String>);
        struct FanoutGuard(Vec<tokio::task::JoinHandle<FanoutOutcome>>);
        impl Drop for FanoutGuard {
            fn drop(&mut self) {
                for h in self.0.drain(..) {
                    h.abort(); // no-op for already-finished workers
                }
            }
        }
        let mut handles = FanoutGuard(handles);
        let mut sections = Vec::with_capacity(handles.0.len());
        for (i, h) in handles.0.iter_mut().enumerate() {
            let idx = i + 1;
            match h.await {
                Ok((Some(name), Ok(summary))) => {
                    sections.push(format!("## task {idx} — {name}\n{summary}"));
                }
                Ok((Some(name), Err(reason))) => {
                    sections.push(format!("## task {idx} — {name}\n[failed: {reason}]"));
                }
                // unnamed entries are the pre-resolved inline failures
                Ok((None, outcome)) => {
                    let reason = outcome
                        .err()
                        .unwrap_or_else(|| "no summary produced".to_string());
                    sections.push(format!("## task {idx}\n[failed: {reason}]"));
                }
                Err(e) => {
                    sections.push(format!("## task {idx}\n[failed: {e}]"));
                }
            }
        }
        let mut report = format!(
            "{} tasks · {:.1}s\n",
            sections.len(),
            started.elapsed().as_secs_f64()
        );
        report.push_str(&sections.join("\n\n"));
        ToolOutput::ok(report)
    }
}

/// One fanout entry's shared source bootstrap (clone of the engine's
/// pathfinder slot plus the parent mode), moved into spawned tasks.
struct FanoutJob {
    def: AgentDef,
    task: String,
    cwd: std::path::PathBuf,
    source: super::pathfinder::PathfinderSource,
    parent_mode: ka_protocol::Mode,
}

/// Run one agent on one task: a nested voice turn (read-only, or
/// worktree-isolated when the agent opts in), 10-minute cap per
/// attempt. `roster` (sibling task rows) is injected into the prompt as
/// context; `on_channels` receives the steering channel per attempt so
/// `tasks send` can reach the live turn. An agent with an `output:`
/// schema gets one instructive retry when an attempt fails. Returns the
/// outcome plus the surviving worktree branch (isolated agents).
async fn run_agent(
    def: AgentDef,
    task: String,
    cwd: std::path::PathBuf,
    source: super::pathfinder::PathfinderSource,
    parent_mode: ka_protocol::Mode,
    roster: Option<Vec<String>>,
    mut on_channels: impl FnMut(mpsc::Sender<ka_protocol::Command>),
) -> (Result<String, String>, Option<String>, f64) {
    let attempts = if def.output.is_some() { 2 } else { 1 };
    let mut outcome: (Result<String, String>, Option<String>) =
        (Err("agent never ran".to_string()), None);
    let mut cost = 0.0f64;
    for attempt in 0..attempts {
        let task_text = if attempt > 0 {
            let reason = match &outcome.0 {
                Err(reason) => reason.clone(),
                Ok(_) => String::new(),
            };
            format!(
                "{task}\n\nYour previous reply could not be used ({reason}). Reply again \
                 with a single JSON value that matches the required output schema — \
                 no prose around it."
            )
        } else {
            task.clone()
        };
        let (res, branch, usage) = run_agent_once(
            &def,
            &task_text,
            &cwd,
            &source,
            parent_mode,
            roster.clone(),
            &mut on_channels,
        )
        .await;
        cost += usage.cost;
        outcome = (res, branch);
        if outcome.0.is_ok() {
            break;
        }
    }
    (outcome.0, outcome.1, cost)
}

/// One attempt of [`run_agent`].
async fn run_agent_once(
    def: &AgentDef,
    task: &str,
    cwd: &std::path::Path,
    source: &super::pathfinder::PathfinderSource,
    parent_mode: ka_protocol::Mode,
    roster: Option<Vec<String>>,
    on_channels: &mut impl FnMut(mpsc::Sender<ka_protocol::Command>),
) -> (Result<String, String>, Option<String>, ka_protocol::Usage) {
    let agent_name = def.name.as_str();
    let Some(parent_model) = source.model.clone() else {
        return (
            Err("delegate: no model configured for the parent session".to_string()),
            None,
            ka_protocol::Usage::default(),
        );
    };
    // per-agent model/effort (factory-droids style): the frontmatter
    // selector wins outright; an effort alone re-arms the parent's
    // selector (replacing any @effort it carried)
    let model = match (&def.model, &def.effort) {
        (Some(m), _) => m.clone(),
        (None, Some(e)) => {
            let base = parent_model.split('@').next().unwrap_or_default();
            format!("{base}@{}", effort_label(e))
        }
        (None, None) => parent_model,
    };
    let agent_tools = def.tools.clone();

    // isolated agents write in a throwaway git worktree on their
    // own branch: they need a repo and write-mode permission
    let mut worktree: Option<std::path::PathBuf> = None;
    if def.isolate {
        if !matches!(
            parent_mode,
            ka_protocol::Mode::AcceptEdits | ka_protocol::Mode::Free
        ) {
            return (
                Err(
                    "delegate: isolated agents need write access — switch to accept_edits or free mode first (/mode)"
                        .to_string(),
                ),
                None,
                ka_protocol::Usage::default(),
            );
        }
        match create_worktree(cwd, &format!("ka-{agent_name}")) {
            Ok(path) => worktree = Some(path),
            Err(e) => return (Err(e), None, ka_protocol::Usage::default()),
        }
    }
    let agent_cwd = worktree.clone().unwrap_or_else(|| cwd.to_path_buf());

    let mut prompt = format!("{}\n\nTask: {}", def.system, task);
    if let Some(rows) = roster.filter(|r| !r.is_empty()) {
        prompt.push_str(&format!(
            "\n\nSibling tasks also in flight (context only — do not wait on them):\n{}",
            rows.join("\n")
        ));
    }
    let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
    on_channels(cmd_tx.clone());
    let (evt_tx, mut evt_rx) = mpsc::channel(256);
    let isolated = worktree.is_some();
    let max_steps = def.max_steps;
    let schema = def.output.clone();
    let source = source.clone();
    let (usage_tx, usage_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        let mut steers = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let mut voice = if isolated {
            // isolated agents may write — inside the worktree only
            Voice::new(
                source.catalog,
                agent_cwd,
                ka_protocol::Mode::Free,
                max_steps,
            )
        } else {
            Voice::new_readonly(
                source.catalog,
                agent_cwd,
                ka_protocol::Mode::Free,
                max_steps,
            )
        };
        voice.set_model_selector(&model, 4.0);
        voice.set_session_id(source.session_id.clone());
        if let Some(tools) = &agent_tools {
            voice.restrict_tools(tools);
        }
        let usage = voice
            .turn(
                &model,
                prompt,
                &mut cmd_rx,
                &evt_tx,
                &mut steers,
                &mut queue,
                &mut GuardRuntime::default(),
                schema,
                Vec::new(),
            )
            .await;
        let _ = usage_tx.send(usage);
    });

    let mut summary = String::new();
    let mut thought = String::new();
    let mut failed: Option<String> = None;
    // 10-minute cap, same budget as pathfinder
    let deadline = tokio::time::timeout(std::time::Duration::from_secs(600), async {
        while let Some(evt) = evt_rx.recv().await {
            match evt {
                ka_protocol::Event::Delta {
                    kind: ka_protocol::DeltaKind::Text(t),
                } => summary.push_str(&t),
                ka_protocol::Event::Delta {
                    kind: ka_protocol::DeltaKind::Thought(t),
                } => thought.push_str(&t),
                ka_protocol::Event::Error { message, .. } => failed = Some(message),
                ka_protocol::Event::TurnFinished { .. } => break,
                _ => {}
            }
        }
    })
    .await;
    drop(cmd_tx);
    let _ = handle.await;

    if !matches!(deadline, Ok(())) {
        return (
            Err("delegate: agent timed out (10m)".to_string()),
            worktree.map(|_| String::new()),
            usage_rx.await.unwrap_or_default(),
        );
    }
    if summary.trim().is_empty() && !thought.trim().is_empty() {
        summary = thought; // thinking models: reason-only replies
    }
    if summary.trim().is_empty() {
        return (
            Err(format!(
                "agent {agent_name} failed: {}",
                failed.unwrap_or_else(|| "no summary produced".to_string())
            )),
            None,
            usage_rx.await.unwrap_or_default(),
        );
    }
    if let Some(wt) = &worktree {
        match finish_worktree(cwd, wt) {
            Ok(branch) => {
                summary.push_str(&format!(
                    "\n\n(isolated worktree: changes live on branch `{branch}`)"
                ));
                let usage = usage_rx.await.unwrap_or_default();
                return (Ok(summary), Some(branch), usage);
            }
            Err(e) => {
                let usage = usage_rx.await.unwrap_or_default();
                return (Err(e), None, usage);
            }
        }
    }
    let usage = usage_rx.await.unwrap_or_default();
    (Ok(summary), None, usage)
}
/// Reasoning-effort label for selector assembly.
fn effort_label(e: &ka_protocol::Effort) -> &'static str {
    match e {
        ka_protocol::Effort::Off => "off",
        ka_protocol::Effort::Low => "low",
        ka_protocol::Effort::Medium => "medium",
        ka_protocol::Effort::High => "high",
        ka_protocol::Effort::Max => "max",
    }
}

/// Create an isolated git worktree on its own branch under the state
/// dir. `Err` when the cwd is not a git repository.
fn create_worktree(cwd: &std::path::Path, name: &str) -> Result<std::path::PathBuf, String> {
    let git_ok = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output();
    match git_ok {
        Ok(o) if o.status.success() && o.stdout.starts_with(b"true") => {}
        Ok(o) if o.stdout.starts_with(b"false") => {
            return Err("delegate: isolate requires a git repository (cwd is inside one?)".into());
        }
        _ => {
            return Err("delegate: isolate requires a git repository".into());
        }
    }
    let state = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })
        .unwrap_or_else(|_| std::env::temp_dir());
    static WORKTREE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let uuid = format!(
        "{}-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        std::process::id(),
        // fanout can run the same isolated agent twice in one
        // millisecond; the sequence number keeps branch/path unique
        WORKTREE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    );
    let path = state.join("ka/worktrees").join(format!("{name}-{uuid}"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("worktree: {e}"))?;
    }
    let branch = format!("{name}-{uuid}");
    let status = std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            &branch,
            path.to_string_lossy().as_ref(),
            "HEAD",
        ])
        .current_dir(cwd)
        .status()
        .map_err(|e| format!("git worktree: {e}"))?;
    if !status.success() {
        return Err("git worktree add failed".to_string());
    }
    Ok(path)
}

/// Remove the worktree (force) and its directory; the branch keeps any
/// commits the agent made. Returns the branch name for the report.
fn finish_worktree(cwd: &std::path::Path, path: &std::path::Path) -> Result<String, String> {
    let branch_out = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(path)
        .output()
        .map_err(|e| format!("git branch: {e}"))?;
    let branch = String::from_utf8_lossy(&branch_out.stdout)
        .trim()
        .to_string();
    let status = std::process::Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .current_dir(cwd)
        .status()
        .map_err(|e| format!("git worktree remove: {e}"))?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(path);
    }
    Ok(branch)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::hands::pathfinder::PathfinderSource;

    fn hand() -> DelegateHand {
        let agents = vec![
            AgentDef {
                name: "reviewer".to_string(),
                description: "reviews diffs".to_string(),
                system: "You review.".to_string(),
                max_steps: 8,
                isolate: false,
                model: None,
                effort: None,
                tools: None,
                output: None,
            },
            AgentDef {
                name: "scout".to_string(),
                description: String::new(),
                system: "You scout.".to_string(),
                max_steps: 12,
                isolate: false,
                model: None,
                effort: None,
                tools: None,
                output: None,
            },
        ];
        let (events, _rx) = mpsc::channel(16);
        DelegateHand::new(
            agents,
            std::sync::Arc::new(parking_lot::RwLock::new(PathfinderSource::default())),
            ka_protocol::Mode::Free,
            crate::hands::tasks::AgentTaskTable::new(),
            events,
            crate::features::FeatureToggles::slot(Vec::new()),
        )
    }

    #[tokio::test]
    async fn agent_toggle_drops_one_agent_from_the_roster() {
        let h = hand();
        h.features
            .write()
            .set(&ka_protocol::FeatureSpec::Agent("reviewer".into()), false);
        let d = h.def();
        assert!(!d.description.contains("reviewer"), "{}", d.description);
        assert!(d.description.contains("scout"));
        let schema = serde_json::to_string(&d.parameters).unwrap();
        assert!(!schema.contains("reviewer"), "enum must drop it too");
        // execute rejects the disabled agent by name
        let out = h
            .execute(
                &serde_json::json!({"agent": "reviewer", "task": "x"}),
                &ctx_for(),
            )
            .await;
        assert!(
            out.content.contains("disabled by a session feature toggle"),
            "{}",
            out.content
        );
        // fanout reports it inline instead of running it
        let out = h
            .execute(
                &serde_json::json!({"tasks": [{"agent": "scout", "task": "x"}, {"agent": "reviewer", "task": "y"}]}),
                &ctx_for(),
            )
            .await;
        assert!(
            out.content.contains("disabled by a session feature toggle"),
            "{}",
            out.content
        );
    }

    fn ctx_for() -> HandContext {
        HandContext {
            cwd: std::env::temp_dir(),
            ledger: std::sync::Arc::new(parking_lot::Mutex::new(super::super::Ledger::default())),
            spill: std::sync::Arc::new(super::super::Spill::new()),
            snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
            jobs: std::sync::Arc::new(crate::hands::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
        }
    }

    #[test]
    fn def_lists_every_agent_with_descriptions() {
        let d = hand().def();
        assert_eq!(d.name, "delegate");
        assert_eq!(d.clearance, Clearance::Read);
        assert!(d.read_only);
        assert!(
            d.description.contains("reviewer: reviews diffs"),
            "{}",
            d.description
        );
        assert!(d.description.contains("scout: (no description)"));
        let enum_names = d.parameters["properties"]["agent"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(enum_names, vec!["reviewer", "scout"]);
    }

    #[tokio::test]
    async fn unknown_agent_and_missing_args_error_cleanly() {
        let h = hand();
        let ctx = ctx_for();
        let out = h
            .execute(&serde_json::json!({"agent": "nope", "task": "x"}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(
            out.content.contains("unknown agent 'nope'"),
            "{}",
            out.content
        );
        assert!(out.content.contains("reviewer, scout"));

        let out = h.execute(&serde_json::json!({"task": "x"}), &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("missing required 'agent'"));

        let out = h
            .execute(&serde_json::json!({"agent": "reviewer"}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("missing required 'task'"));
    }

    #[tokio::test]
    async fn delegate_without_model_reports_missing_configuration() {
        let h = hand();
        let ctx = ctx_for();
        let out = h
            .execute(
                &serde_json::json!({"agent": "reviewer", "task": "review x"}),
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(
            out.content.contains("no model configured"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn tasks_fanout_reports_both_unknown_agents_in_one_output() {
        let h = hand();
        let ctx = ctx_for();
        let out = h
            .execute(
                &serde_json::json!({"tasks": [
                    {"agent": "nope1", "task": "a"},
                    {"agent": "nope2", "task": "b"}
                ]}),
                &ctx,
            )
            .await;
        assert!(
            !out.is_error,
            "fanout succeeds with inline failures: {}",
            out.content
        );
        assert!(out.content.contains("2 tasks"), "{}", out.content);
        assert!(
            out.content.contains("[failed: unknown agent 'nope1']"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("[failed: unknown agent 'nope2']"),
            "{}",
            out.content
        );
        // input order preserved
        let n1 = out.content.find("nope1").unwrap();
        let n2 = out.content.find("nope2").unwrap();
        assert!(n1 < n2);
    }

    #[tokio::test]
    async fn tasks_arg_validation_rejects_mixed_and_empty() {
        let h = hand();
        let ctx = ctx_for();
        // agent+task together with tasks is refused
        let out = h
            .execute(
                &serde_json::json!({"agent": "reviewer", "task": "x", "tasks": []}),
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(
            out.content.contains("either agent+task or tasks"),
            "{}",
            out.content
        );
        // empty tasks array is refused
        let out = h.execute(&serde_json::json!({"tasks": []}), &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("must not be empty"), "{}", out.content);
        // known agents without a model: sections fail inline, in input order
        let out = h
            .execute(
                &serde_json::json!({"tasks": [
                    {"agent": "scout", "task": "a"},
                    {"agent": "reviewer", "task": "b"}
                ]}),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("## task 1 — scout"), "{}", out.content);
        assert!(
            out.content.contains("## task 2 — reviewer"),
            "{}",
            out.content
        );
        assert_eq!(
            out.content
                .matches("[failed: delegate: no model configured")
                .count(),
            2,
            "{}",
            out.content
        );
    }

    #[test]
    fn isolate_parse_flag() {
        let a = AgentDef::parse("---\nname: w\nisolate: true\n---\nbody", "w");
        assert!(a.isolate);
        let b = AgentDef::parse("plain body", "b");
        assert!(!b.isolate);
    }

    #[test]
    fn worktree_lifecycle_creates_own_branch_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("ka-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap()
        };
        assert!(run(&["init"]).status.success());
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "one\n").unwrap();
        run(&["add", "."]);
        assert!(run(&["commit", "-m", "init"]).status.success());

        let wt = create_worktree(&dir, "reviewer").expect("worktree created");
        assert!(wt.exists());
        assert!(wt.join(".git").exists());
        // branch check: HEAD of the worktree is on its own branch
        let branch = std::process::Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&wt)
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&branch.stdout).contains("reviewer"));

        let name = finish_worktree(&dir, &wt).unwrap();
        assert!(name.contains("reviewer"));
        assert!(!wt.exists(), "worktree dir removed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
