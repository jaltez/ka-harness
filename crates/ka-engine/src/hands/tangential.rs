//! The tangential spawn (`/tan`, TUI-16): a lateral question or small
//! job that runs detached on its own nested voice while the main
//! conversation continues untouched. The child is built exactly like a
//! `delegate` child — [`super::delegate::arm_background`] over
//! [`super::tasks::AgentTaskTable`], read-only tools, the same turn
//! cap, and auto-denied asks (nobody is there to answer) — so the
//! preset tells it to work only with what is already allowed. The
//! result never interrupts: it lands as one `Note` prefixed
//! [`ka_protocol::TAN_NOTE_PREFIX`] (`tan · <subject> — <first
//! line>`), full answers over 32 KiB parked on the spill store.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::agents::AgentDef;

/// The child's system preset (literal): autonomous, read-only, one
/// self-contained answer of at most 40 lines.
const TAN_PRESET: &str = "You are a tangential worker (`/tan`): you answer one lateral \
question or do one small piece of background work while the main conversation continues \
elsewhere.\n\
\n\
Work autonomously: there is no user to interact with — asks auto-deny in this nested \
session, so never ask for permission or clarification; reason from what you can already \
see and what your (read-only) tools surface.\n\
\n\
Your sandbox is read-only: do not edit, write, or create files.\n\
\n\
Reply with a self-contained answer of at most 40 lines: the answer and the few facts it \
rests on, no process narration, no offers to do more.";

/// Subject cap for the `tan · ` note (chars).
const SUBJECT_CAP: usize = 40;

/// First-line cap for the answer line (chars — the delegate note cap).
const NOTE_CAP: usize = 200;

/// Answers over this size park on the spill store; the note keeps the
/// first line and the `spill://` pointer (the delegate result pattern).
const SPILL_AT: usize = 32 * 1024;

/// The nested voice's step budget — the delegate child default (no
/// frontmatter means 12), the same turn cap every delegate run gets.
const TAN_MAX_STEPS: u32 = 12;

/// Maximum concurrent live tangential jobs.
const MAX_LIVE_TAN: usize = 8;

/// One parsed `Command::Tan` text.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TanAction {
    /// Spawn a child on the question (verbatim, trimmed).
    Ask(String),
    /// List live jobs (ids + first line).
    List,
    /// Cancel the only live job (`None`) or one by id.
    Cancel(Option<u64>),
    /// Unparseable — refuse with the usage line.
    Usage,
}

/// `/tan` grammar: the question verbatim; bare/blank = listing;
/// `cancel` = the only live job; `cancel <id>` = one job (the `t-`
/// prefix tolerated). Anything else after `cancel` is refused, not
/// read as a question — `/tan cancel the pending deployment` is a
/// mistyped cancel (a question can rephrase around the leading word).
fn parse_tan(text: &str) -> TanAction {
    let text = text.trim();
    if text.is_empty() {
        return TanAction::List;
    }
    if let Some(rest) = text.strip_prefix("cancel") {
        if rest.is_empty() {
            return TanAction::Cancel(None);
        }
        if rest.starts_with(char::is_whitespace) {
            let rest = rest.trim();
            let digits = rest.strip_prefix("t-").unwrap_or(rest);
            return match digits.parse::<u64>() {
                Ok(id) => TanAction::Cancel(Some(id)),
                Err(_) => TanAction::Usage,
            };
        }
    }
    TanAction::Ask(text.to_string())
}

/// The first non-empty line of a text, whitespace-trimmed.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
}

/// The completion/cancel note for one tangential job:
/// `tan · <subject> — <first line>`, plus the spill pointer once the
/// full answer exceeds the spill threshold.
fn tan_answer_note(subject: &str, answer: &str) -> String {
    let head = super::delegate::truncate_for_note(first_line(answer), NOTE_CAP);
    let mut message = format!("{}{subject} — {head}", ka_protocol::TAN_NOTE_PREFIX);
    if answer.len() > SPILL_AT {
        match super::Spill::new().park(answer) {
            Ok(ptr) => message.push_str(&format!(" (full: {ptr})")),
            Err(_) => message.push_str(" (full result via the tasks tool — spilling failed)"),
        }
    }
    message
}

/// One live tangential job (for `/tan` listing and cancellation).
struct LiveTan {
    id: u64,
    subject: String,
}

/// Shared state behind [`Tangential`] (cheap to clone into watchers).
struct TanInner {
    tasks: Arc<super::tasks::AgentTaskTable>,
    events: mpsc::Sender<ka_protocol::Event>,
    hooks: Arc<Vec<crate::config::Hook>>,
    features: crate::features::FeatureSlot,
    /// The engine's nested-voice bootstrap (catalog + model + session
    /// id): the child's speaker source, shared with delegate.
    source: Arc<parking_lot::RwLock<super::pathfinder::PathfinderSource>>,
    parent_mode: ka_protocol::Mode,
    live: parking_lot::Mutex<Vec<LiveTan>>,
}

/// Tangential spawns (`Command::Tan`): arms background children via
/// the delegate spawn path and turns their outcomes into transcript
/// notes. The engine owns one; `Command::Tan` dispatches into it and
/// [`Tangential::handle`] answers with notes (the engine sends its
/// usual `Idle` after the dispatch).
#[derive(Clone)]
pub struct Tangential {
    inner: Arc<TanInner>,
}

impl Tangential {
    /// New tangential spawner over the engine's shared pieces (the
    /// same handles [`super::delegate::DelegateHand`] runs with).
    pub fn new(
        source: Arc<parking_lot::RwLock<super::pathfinder::PathfinderSource>>,
        parent_mode: ka_protocol::Mode,
        tasks: Arc<super::tasks::AgentTaskTable>,
        events: mpsc::Sender<ka_protocol::Event>,
        features: crate::features::FeatureSlot,
        hooks: Arc<Vec<crate::config::Hook>>,
    ) -> Self {
        Self {
            inner: Arc::new(TanInner {
                tasks,
                events,
                hooks,
                features,
                source,
                parent_mode,
                live: parking_lot::Mutex::new(Vec::new()),
            }),
        }
    }

    /// Handle one `Command::Tan` text: spawn, list, or cancel — the
    /// answer always arrives as a `Note`, never as a turn.
    pub async fn handle(&self, text: &str, cwd: &std::path::Path) {
        match parse_tan(text) {
            TanAction::Ask(question) => self.spawn(&question, cwd).await,
            TanAction::List => {
                send_note(&self.inner, self.list_message()).await;
            }
            TanAction::Cancel(id) => self.cancel(id).await,
            TanAction::Usage => {
                send_note(
                    &self.inner,
                    "tan: usage — /tan <question> | /tan cancel [id] (a question cannot start \
                     with `cancel`; rephrase)"
                        .to_string(),
                )
                .await;
            }
        }
    }

    /// Arm one child on the question and remember it as live. No ack
    /// note: the dispatching surface confirms locally, the transcript
    /// stays quiet until the answer itself arrives.
    async fn spawn(&self, question: &str, cwd: &std::path::Path) {
        if crate::conventions::bare_mode() {
            send_note(&self.inner, "tan: unavailable in safe mode".to_string()).await;
            return;
        }
        if self.inner.live.lock().len() >= MAX_LIVE_TAN {
            send_note(
                &self.inner,
                format!(
                    "tan: too many concurrent tangential jobs (max {MAX_LIVE_TAN}) — wait or /tan cancel"
                ),
            )
            .await;
            return;
        }
        let subject =
            super::delegate::truncate_for_note(first_line(question), SUBJECT_CAP).to_string();
        let def = AgentDef {
            name: "tan".to_string(),
            description: String::new(),
            system: TAN_PRESET.to_string(),
            max_steps: TAN_MAX_STEPS,
            isolate: false,
            model: None,
            effort: None,
            tools: None,
            output: None,
        };
        // bound ahead of the await: the read guard must not live
        // across the spawn (it is not Send)
        let source = self.inner.source.read().clone();
        let run = super::delegate::arm_background(
            def,
            question,
            cwd,
            source,
            self.inner.parent_mode,
            self.inner.tasks.clone(),
            &self.inner.events,
            self.inner.hooks.clone(),
            self.inner.features.clone(),
        )
        .await;
        self.inner.live.lock().push(LiveTan {
            id: run.id,
            subject: subject.clone(),
        });
        let inner = self.inner.clone();
        tokio::spawn(watch_tan(inner, run.id, subject, run.completion));
    }

    /// Cancel one job (`Some(id)`) or the only live one (`None`).
    /// Every ending emits its `tan · ` note so the surface pill counts
    /// down exactly once per job.
    async fn cancel(&self, id: Option<u64>) {
        let target = match id {
            Some(id) => Some(id),
            None => {
                let live = self.inner.live.lock();
                (live.len() == 1).then_some(live[0].id)
            }
        };
        let Some(id) = target else {
            // zero or several live jobs: say which (or how to name one)
            let rows = self.live_rows();
            let message = if rows.is_empty() {
                "tan: nothing to cancel — no tangential jobs running (usage: /tan <question>)"
                    .to_string()
            } else {
                format!(
                    "tan: {} jobs running — /tan cancel <id>: {}",
                    rows.len(),
                    rows.join(", ")
                )
            };
            send_note(&self.inner, message).await;
            return;
        };
        let subject = self
            .inner
            .live
            .lock()
            .iter()
            .find(|j| j.id == id)
            .map(|j| j.subject.clone());
        let Some(subject) = subject else {
            // a delegate task id, or a job that already finished
            let message = match self.inner.tasks.result(id) {
                Some(status) => format!("tan: t-{id} is not a live tangential job ({status})"),
                None => format!("tan: no such job t-{id}"),
            };
            send_note(&self.inner, message).await;
            return;
        };
        match self.inner.tasks.cancel(id) {
            Ok(_msg) => {
                forget(&self.inner, id);
                send_note(
                    &self.inner,
                    format!("{}{subject} — cancelled", ka_protocol::TAN_NOTE_PREFIX),
                )
                .await;
            }
            Err(reason) => {
                // finished between the lookup and the cancel — forget
                // the stale entry either way
                forget(&self.inner, id);
                send_note(&self.inner, format!("tan: {reason}")).await;
            }
        }
    }

    /// `t-<id> "<subject>"` rows for the live jobs, id order.
    fn live_rows(&self) -> Vec<String> {
        self.inner
            .live
            .lock()
            .iter()
            .map(|j| format!("t-{} \u{201c}{}\u{201d}", j.id, j.subject))
            .collect()
    }

    /// The `/tan` listing note: ids + first lines when jobs are alive,
    /// the usage line when not (an empty `/tan` with nothing to list is
    /// a mistyped question — teach the grammar).
    fn list_message(&self) -> String {
        let rows = self.live_rows();
        if rows.is_empty() {
            return "tan: no tangential jobs running — usage: /tan <question>".to_string();
        }
        let mut message = format!("tan: {} running", rows.len());
        for row in rows {
            message.push_str(&format!("\n  {row}"));
        }
        message
    }
}

/// One transcript note out (failures are fine to drop — the surface
/// may be gone).
async fn send_note(inner: &TanInner, message: String) {
    inner
        .events
        .send(ka_protocol::Event::Note { message })
        .await
        .ok();
}

/// Drop a job from the live registry; `true` when it was there.
fn forget(inner: &TanInner, id: u64) -> bool {
    let mut live = inner.live.lock();
    let before = live.len();
    live.retain(|j| j.id != id);
    live.len() != before
}

/// The tangential completion watcher: await the runner (the handle's
/// completion channel) and end the job with exactly one `tan · ` note.
/// A job cancelled through the `tasks` hand (not `/tan cancel`) is
/// ended here too, so the pill and the listing stay honest.
async fn watch_tan(
    inner: Arc<TanInner>,
    id: u64,
    subject: String,
    runner: tokio::task::JoinHandle<Result<String, String>>,
) {
    match runner.await {
        Ok(Ok(answer)) => {
            forget(&inner, id);
            // settle the shared task-table row like the delegate watcher
            // does; `finish` is first-write-wins, so a cancel's state
            // stands and its note is the only one (never double-note)
            if inner.tasks.finish(id, Ok(answer.clone())) {
                send_note(&inner, tan_answer_note(&subject, &answer)).await;
            }
        }
        Ok(Err(reason)) => {
            forget(&inner, id);
            if inner.tasks.finish(id, Err(reason.clone())) {
                send_note(
                    &inner,
                    format!(
                        "{}{subject} — failed: {}",
                        ka_protocol::TAN_NOTE_PREFIX,
                        super::delegate::truncate_for_note(&reason, NOTE_CAP)
                    ),
                )
                .await;
            }
        }
        // `/tan cancel` already noted the end and forgot the job; a
        // cancel through the `tasks` hand did neither — end it here
        Err(e) if e.is_cancelled() => {
            if forget(&inner, id) {
                send_note(
                    &inner,
                    format!("{}{subject} — cancelled", ka_protocol::TAN_NOTE_PREFIX),
                )
                .await;
            }
        }
        // a panicked runner must not wedge the pill: fail the task and
        // end the job loudly (same contract as the delegate watcher)
        Err(_) => {
            if forget(&inner, id) {
                let _ = inner
                    .tasks
                    .finish(id, Err("runner task panicked".to_string()));
                send_note(
                    &inner,
                    format!(
                        "{}{subject} — failed: runner task panicked",
                        ka_protocol::TAN_NOTE_PREFIX
                    ),
                )
                .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A spawner wired to a test channel: `(tangential, note receiver)`.
    fn tangential() -> (Tangential, mpsc::Receiver<ka_protocol::Event>) {
        let (events, rx) = mpsc::channel(64);
        let t = Tangential::new(
            Arc::new(parking_lot::RwLock::new(
                super::super::pathfinder::PathfinderSource::default(),
            )),
            ka_protocol::Mode::Allow,
            super::super::tasks::AgentTaskTable::new(),
            events,
            crate::features::FeatureToggles::slot(Vec::new()),
            Arc::new(Vec::new()),
        );
        (t, rx)
    }

    fn notes(rx: &mut mpsc::Receiver<ka_protocol::Event>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            if let ka_protocol::Event::Note { message } = evt {
                out.push(message);
            }
        }
        out
    }

    #[test]
    fn parse_tan_shapes() {
        assert_eq!(
            parse_tan("  why is the sky blue \n"),
            TanAction::Ask("why is the sky blue".to_string())
        );
        assert_eq!(parse_tan(""), TanAction::List);
        assert_eq!(parse_tan("   "), TanAction::List);
        assert_eq!(parse_tan("cancel"), TanAction::Cancel(None));
        assert_eq!(parse_tan("cancel 3"), TanAction::Cancel(Some(3)));
        assert_eq!(parse_tan("cancel t-12"), TanAction::Cancel(Some(12)));
        assert_eq!(parse_tan("cancel t-"), TanAction::Usage);
        assert_eq!(parse_tan("cancel why did it break"), TanAction::Usage);
    }

    #[tokio::test]
    async fn list_note_is_usage_when_nothing_runs() {
        let (t, mut rx) = tangential();
        t.handle("", std::env::temp_dir().as_path()).await;
        assert_eq!(
            notes(&mut rx),
            vec!["tan: no tangential jobs running — usage: /tan <question>".to_string()]
        );
    }

    #[tokio::test]
    async fn cancel_resolves_the_single_live_job_without_an_id() {
        let (t, mut rx) = tangential();
        // seed the registry + table as a spawned child would
        t.inner.tasks.register("tan", "how does the spill GC work");
        t.inner.live.lock().push(LiveTan {
            id: 1,
            subject: "how does the spill GC work".to_string(),
        });
        t.handle("cancel", std::env::temp_dir().as_path()).await;
        assert_eq!(
            notes(&mut rx),
            vec![format!(
                "{}how does the spill GC work — cancelled",
                ka_protocol::TAN_NOTE_PREFIX
            )],
            "the note is the one `tan · ` line the pill counts down"
        );
        assert!(t.inner.live.lock().is_empty(), "the job left the registry");
        assert!(
            t.inner
                .tasks
                .result(1)
                .is_some_and(|r| r.contains("cancelled")),
            "{}",
            t.inner.tasks.result(1).unwrap_or_default()
        );
    }

    #[tokio::test]
    async fn cancel_with_several_jobs_names_the_ids() {
        let (t, mut rx) = tangential();
        t.inner.live.lock().push(LiveTan {
            id: 1,
            subject: "first".to_string(),
        });
        t.inner.live.lock().push(LiveTan {
            id: 2,
            subject: "second".to_string(),
        });
        t.handle("cancel", std::env::temp_dir().as_path()).await;
        let got = notes(&mut rx);
        assert_eq!(got.len(), 1);
        assert!(
            got[0].starts_with("tan: 2 jobs running — /tan cancel <id>:"),
            "{}",
            got[0]
        );
        assert!(got[0].contains("t-1 \u{201c}first\u{201d}"), "{}", got[0]);
        assert!(got[0].contains("t-2 \u{201c}second\u{201d}"), "{}", got[0]);
    }

    #[tokio::test]
    async fn cancel_refuses_non_tan_and_unknown_ids() {
        let (t, mut rx) = tangential();
        // a delegate task id: registered in the table, not a tan job
        t.inner.tasks.register("reviewer", "review the diff");
        t.handle("cancel 1", std::env::temp_dir().as_path()).await;
        let got = notes(&mut rx);
        assert_eq!(got.len(), 1);
        assert!(
            got[0].starts_with("tan: t-1 is not a live tangential job"),
            "{}",
            got[0]
        );
        // an id nobody registered
        t.handle("cancel t-99", std::env::temp_dir().as_path())
            .await;
        let got = notes(&mut rx);
        assert_eq!(got, vec!["tan: no such job t-99".to_string()]);
    }

    #[tokio::test]
    async fn unparseable_cancel_refuses_with_usage() {
        let (t, mut rx) = tangential();
        t.handle(
            "cancel the pending deployment",
            std::env::temp_dir().as_path(),
        )
        .await;
        let got = notes(&mut rx);
        assert_eq!(got.len(), 1);
        assert!(got[0].starts_with("tan: usage —"), "{}", got[0]);
    }

    #[tokio::test]
    async fn spawn_without_model_fails_into_a_tan_note() {
        let (t, mut rx) = tangential();
        t.handle(
            "why does the spill GC keep files",
            std::env::temp_dir().as_path(),
        )
        .await;
        // registered as a `tan` child in the shared table
        let rows = t.inner.tasks.rows();
        assert_eq!(rows.len(), 1, "{}", rows.join("\n"));
        assert!(rows[0].contains("tan"), "{}", rows[0]);
        assert_eq!(t.inner.live.lock().len(), 1, "the job is live");
        // no model: the runner settles failed and the watcher ends the
        // job with exactly one `tan · ` note
        let mut got = Vec::new();
        for _ in 0..100 {
            got = notes(&mut rx);
            if !got.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            got[0].starts_with("tan · why does the spill GC keep files — failed:"),
            "{}",
            got[0]
        );
        assert!(t.inner.live.lock().is_empty(), "the job left the registry");
        // the shared row settled too (not wedged `Running`): /tasks read,
        // send and cancel keep behaving for the id after a natural end
        let rows = t.inner.tasks.rows();
        assert_eq!(rows.len(), 1, "{}", rows.join("\n"));
        assert!(
            rows[0].contains("failed") && !rows[0].contains("running"),
            "the row must not stay running: {}",
            rows[0]
        );
    }

    #[test]
    fn answer_note_format_and_spill_parking() {
        // the plain format: `tan · <subject> — <first line>`
        let plain = tan_answer_note(
            "why is the sky blue",
            "Rayleigh scattering.\n\nMore detail.",
        );
        assert_eq!(
            plain,
            format!(
                "{}why is the sky blue — Rayleigh scattering.",
                ka_protocol::TAN_NOTE_PREFIX
            )
        );
        // oversized answers park on the spill store, note keeps the head
        let big = format!("Big answer.\n{}", "x".repeat(SPILL_AT + 1));
        let note = tan_answer_note("big", &big);
        assert!(
            note.starts_with(&format!(
                "{}big — Big answer.",
                ka_protocol::TAN_NOTE_PREFIX
            )),
            "{}",
            note
        );
        assert!(note.contains("(full: spill://"), "{}", note);
    }

    #[test]
    fn subject_caps_at_the_first_line() {
        let subject = super::super::delegate::truncate_for_note(
            first_line("a very long question that keeps going and going and going\nsecond line"),
            SUBJECT_CAP,
        );
        assert!(subject.chars().count() <= SUBJECT_CAP + 1, "{subject}");
        assert!(!subject.contains("second"), "{subject}");
    }

    #[test]
    fn preset_is_self_contained_and_read_only() {
        assert!(
            TAN_PRESET.contains("no user to interact with"),
            "{TAN_PRESET}"
        );
        assert!(TAN_PRESET.contains("asks auto-deny"), "{TAN_PRESET}");
        assert!(
            TAN_PRESET.contains("do not edit, write, or create files"),
            "{TAN_PRESET}"
        );
        assert!(TAN_PRESET.contains("at most 40 lines"), "{TAN_PRESET}");
    }
}
