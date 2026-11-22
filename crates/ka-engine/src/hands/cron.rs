//! The cron hand: session-scoped scheduled self-prompts. `create`
//! arms an entry (recurring interval or one-shot); when the engine's
//! idle tick finds entries due, it sends the stored prompt through the
//! command channel as a full self-prompt (a complete turn, tools
//! included) preceded by a `cron fired: <id>` note. `list` shows the
//! armed entries; `delete` disarms one.
//!
//! Cron is SESSION-VIVA by design: the table lives in memory, nothing
//! is persisted, and closing the session disarms everything — an
//! unattended executor that outlives its session is the loop tier's
//! job, not cron's. `--safe-mode` never arms cron: `create` refuses
//! and the firing path is inert, so no entry can exist or fire.

use std::time::Instant;

use serde_json::{Value, json};

use super::{Clearance, Hand, HandContext, HandDef, ToolOutput};

/// One armed schedule.
#[derive(Debug, Clone)]
pub struct CronEntry {
    /// Stable id (`cron-1`, `cron-2`, …).
    pub id: String,
    /// Freeform human-readable schedule text (`"every 5m"`) — carried
    /// verbatim for `list`; the timing truth is `interval_secs`.
    pub schedule_text: String,
    /// Prompt sent through the command channel when due.
    pub prompt: String,
    /// Interval between fires in seconds.
    pub interval_secs: u64,
    /// Fire once, then disarm (None next fire).
    pub one_shot: bool,
    /// Next due instant (monotonic clock).
    pub next_due: Instant,
}

/// An immutable snapshot row for `list`.
#[derive(Debug, Clone)]
pub struct CronView {
    pub id: String,
    pub schedule_text: String,
    pub prompt: String,
    pub interval_secs: u64,
    pub one_shot: bool,
    /// Seconds until the next fire (0 when already due).
    pub due_in_secs: u64,
}

/// Shared cron table: the hand creates/deletes, the engine tick fires.
/// One per voice; entries are session-scoped and never persisted.
#[derive(Default)]
pub struct CronTable {
    inner: parking_lot::Mutex<TableInner>,
}

#[derive(Default)]
struct TableInner {
    next_id: u64,
    entries: Vec<CronEntry>,
}

impl CronTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Arm a schedule; returns its id. The first fire lands one full
    /// `interval_secs` from now (an immediate fire would race the
    /// turn that created it).
    pub fn create(
        &self,
        schedule_text: String,
        prompt: String,
        interval_secs: u64,
        one_shot: bool,
    ) -> String {
        let mut t = self.inner.lock();
        t.next_id += 1;
        let id = format!("cron-{}", t.next_id);
        t.entries.push(CronEntry {
            id: id.clone(),
            schedule_text,
            prompt,
            interval_secs,
            one_shot,
            next_due: Instant::now() + std::time::Duration::from_secs(interval_secs),
        });
        id
    }

    /// Disarm one schedule by id; Err when unknown.
    pub fn delete(&self, id: &str) -> Result<(), String> {
        let mut t = self.inner.lock();
        let before = t.entries.len();
        t.entries.retain(|e| e.id != id);
        if t.entries.len() == before {
            return Err(format!("no such cron entry: {id}"));
        }
        Ok(())
    }

    /// Snapshot of every armed entry.
    pub fn snapshot(&self) -> Vec<CronView> {
        let t = self.inner.lock();
        let now = Instant::now();
        t.entries
            .iter()
            .map(|e| CronView {
                id: e.id.clone(),
                schedule_text: e.schedule_text.clone(),
                prompt: e.prompt.clone(),
                interval_secs: e.interval_secs,
                one_shot: e.one_shot,
                due_in_secs: e.next_due.saturating_duration_since(now).as_secs(),
            })
            .collect()
    }

    /// Collect every entry due at `now`: recurring entries reschedule
    /// (`next_due += interval`, overdue ones fire immediately again on
    /// the next tick), one-shots disarm. Returns (id, prompt) pairs in
    /// creation order. Pure bookkeeping — the caller sends the
    /// prompts, so a turn never runs under the table lock.
    pub fn take_due(&self, now: Instant) -> Vec<(String, String)> {
        let mut t = self.inner.lock();
        let mut due = Vec::new();
        let mut kept = Vec::new();
        for e in t.entries.drain(..) {
            if e.next_due > now {
                kept.push(e);
                continue;
            }
            due.push((e.id.clone(), e.prompt.clone()));
            if !e.one_shot {
                // schedule the next fire from NOW (a starved tick
                // catches up on the following tick, not N times over)
                kept.push(CronEntry {
                    next_due: now + std::time::Duration::from_secs(e.interval_secs),
                    ..e
                });
            }
        }
        t.entries = kept;
        due
    }
}

/// The cron tool.
pub struct CronHand {
    table: std::sync::Arc<CronTable>,
}

impl CronHand {
    /// Hand over the shared table (must be the voice's table so the
    /// engine tick fires what this hand arms).
    pub fn new(table: std::sync::Arc<CronTable>) -> Self {
        Self { table }
    }
}

impl Hand for CronHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "cron".to_string(),
            description: "Schedule self-prompts for THIS session (in-memory; disarmed when the \
                session ends). Arguments: {\"action\": \"create\" | \"list\" | \"delete\", \
                \"schedule_text\": \"every 5m\", \"prompt\": \"check the deploy\", \
                \"interval_secs\": 300, \"one_shot\": false, \"id\": \"cron-1\"}. create needs \
                schedule_text + prompt + interval_secs (one_shot defaults to false); the first \
                fire lands one interval after creation. When due, the prompt runs as a full \
                turn of this session and a `cron fired: <id>` note marks it. Not available in \
                safe mode. list takes no other args; delete needs id."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["create", "list", "delete"],
                        "description": "create (default), list or delete"
                    },
                    "schedule_text": {
                        "type": "string",
                        "description": "Human-readable schedule (create)"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Prompt to run when due (create)"
                    },
                    "interval_secs": {
                        "type": "integer",
                        "description": "Seconds between fires (create, >= 1)"
                    },
                    "one_shot": {
                        "type": "boolean",
                        "description": "Fire once then disarm (create, default false)"
                    },
                    "id": {
                        "type": "string",
                        "description": "Entry id (delete)"
                    }
                }
            }),
            clearance: Clearance::Exec,
            read_only: false,
        }
    }

    /// Listing is read-only; arming or disarming an unattended executor
    /// is arbitrary execution and gates at exec tier.
    fn clearance_for(&self, args: &Value) -> Clearance {
        if args.get("action").and_then(Value::as_str) == Some("list") {
            Clearance::Read
        } else {
            Clearance::Exec
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _ctx: &'a HandContext,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            if crate::conventions::bare_mode() {
                return ToolOutput::err("cron is unavailable in safe mode");
            }
            match args
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("create")
            {
                "list" => ToolOutput::ok(self.render_list()),
                "create" => {
                    let Some(prompt) = args
                        .get("prompt")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                    else {
                        return ToolOutput::err("cron create: missing required 'prompt'");
                    };
                    let interval_secs = args.get("interval_secs").and_then(Value::as_u64);
                    match interval_secs {
                        Some(secs) if secs >= 1 => {
                            let schedule_text = args
                                .get("schedule_text")
                                .and_then(Value::as_str)
                                .unwrap_or("every interval")
                                .to_string();
                            let one_shot = args
                                .get("one_shot")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            let id = self.table.create(schedule_text, prompt, secs, one_shot);
                            let kind = if one_shot { " once" } else { "" };
                            ToolOutput::ok(format!(
                                "cron {id} armed: fires{kind} in {secs}s (`{}`)",
                                self.table
                                    .snapshot()
                                    .iter()
                                    .find(|v| v.id == id)
                                    .map(|v| v.schedule_text.as_str())
                                    .unwrap_or("")
                            ))
                        }
                        Some(_) => ToolOutput::err("cron create: 'interval_secs' must be >= 1"),
                        None => ToolOutput::err("cron create: missing required 'interval_secs'"),
                    }
                }
                "delete" => {
                    let Some(id) = args.get("id").and_then(Value::as_str) else {
                        return ToolOutput::err("cron delete: missing required 'id'");
                    };
                    match self.table.delete(id) {
                        Ok(()) => ToolOutput::ok(format!("cron {id} disarmed")),
                        Err(e) => ToolOutput::err(e),
                    }
                }
                other => ToolOutput::err(format!(
                    "cron: unknown action {other:?} (expected \"create\", \"list\" or \"delete\")"
                )),
            }
        })
    }
}

impl CronHand {
    fn render_list(&self) -> String {
        let entries = self.table.snapshot();
        if entries.is_empty() {
            return "no cron entries armed".to_string();
        }
        let mut out = String::new();
        for e in entries {
            let kind = if e.one_shot {
                "one-shot".to_string()
            } else {
                format!("every {}s", e.interval_secs)
            };
            let prompt_head: String = e.prompt.chars().take(80).collect();
            out.push_str(&format!(
                "{}  {kind}  due in {}s  `{schedule}`  prompt: {prompt_head}\n",
                e.id,
                e.due_in_secs,
                schedule = e.schedule_text,
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn ctx() -> HandContext {
        HandContext {
            cwd: std::env::temp_dir(),
            ledger: std::sync::Arc::new(parking_lot::Mutex::new(super::super::Ledger::default())),
            spill: std::sync::Arc::new(super::super::Spill::new()),
            snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                super::super::snapshots::Snapshots::inert(),
            )),
            jobs: std::sync::Arc::new(super::super::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
        }
    }

    #[tokio::test]
    async fn create_then_take_due_fires_and_reschedules() {
        let table = std::sync::Arc::new(CronTable::new());
        let hand = CronHand::new(table.clone());
        let out = hand
            .execute(
                &json!({
                    "action": "create",
                    "schedule_text": "every 1s",
                    "prompt": "check the build",
                    "interval_secs": 1
                }),
                &ctx(),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("cron-1"), "{}", out.content);

        // not due yet (first fire lands one interval out)
        let now = std::time::Instant::now();
        assert!(table.take_due(now).is_empty());
        // due one second later: fires and reschedules
        let later = now + std::time::Duration::from_secs(2);
        let due = table.take_due(later);
        assert_eq!(due, vec![("cron-1".into(), "check the build".into())]);
        // rescheduled: not immediately due again, but due after one more interval
        assert!(table.take_due(later).is_empty());
        assert_eq!(
            table
                .take_due(later + std::time::Duration::from_secs(2))
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn one_shot_disarms_after_firing() {
        let table = std::sync::Arc::new(CronTable::new());
        let hand = CronHand::new(table.clone());
        hand.execute(
            &json!({
                "prompt": "remind me",
                "interval_secs": 1,
                "one_shot": true
            }),
            &ctx(),
        )
        .await;
        let later = std::time::Instant::now() + std::time::Duration::from_secs(2);
        assert_eq!(table.take_due(later).len(), 1);
        assert!(table.take_due(later).is_empty(), "one-shot must disarm");
        assert!(table.snapshot().is_empty());
    }

    #[tokio::test]
    async fn delete_and_list_round_trip() {
        let table = std::sync::Arc::new(CronTable::new());
        let hand = CronHand::new(table.clone());
        hand.execute(&json!({"prompt": "a", "interval_secs": 60}), &ctx())
            .await;
        hand.execute(&json!({"prompt": "b", "interval_secs": 60}), &ctx())
            .await;
        let listed = hand.execute(&json!({"action": "list"}), &ctx()).await;
        assert!(!listed.is_error);
        assert!(listed.content.contains("cron-1"));
        assert!(listed.content.contains("cron-2"));

        let gone = hand
            .execute(&json!({"action": "delete", "id": "cron-1"}), &ctx())
            .await;
        assert!(!gone.is_error, "{}", gone.content);
        assert!(
            hand.execute(&json!({"action": "delete", "id": "cron-1"}), &ctx())
                .await
                .is_error
        );
        assert_eq!(table.snapshot().len(), 1);
    }

    #[tokio::test]
    async fn create_gates_exec_and_list_gates_read() {
        let hand = CronHand::new(std::sync::Arc::new(CronTable::new()));
        assert_eq!(
            hand.clearance_for(&json!({"action": "list"})),
            Clearance::Read
        );
        assert_eq!(
            hand.clearance_for(&json!({"action": "create", "prompt": "p", "interval_secs": 5})),
            Clearance::Exec
        );
        assert_eq!(
            hand.clearance_for(&json!({"action": "delete", "id": "cron-1"})),
            Clearance::Exec
        );
    }

    #[tokio::test]
    async fn create_validates_arguments() {
        let hand = CronHand::new(std::sync::Arc::new(CronTable::new()));
        assert!(
            hand.execute(&json!({"interval_secs": 5}), &ctx())
                .await
                .is_error
        );
        assert!(hand.execute(&json!({"prompt": "p"}), &ctx()).await.is_error);
        assert!(
            hand.execute(&json!({"prompt": "p", "interval_secs": 0}), &ctx())
                .await
                .is_error
        );
        assert!(
            hand.execute(&json!({"action": "delete"}), &ctx())
                .await
                .is_error
        );
        assert!(
            hand.execute(&json!({"action": "explode"}), &ctx())
                .await
                .is_error
        );
    }
}
