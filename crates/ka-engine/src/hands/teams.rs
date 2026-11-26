//! EXT-12 agent teams: persistent, named member sessions the parent
//! orchestrates — star topology, the parent is the router. Every member
//! is a full engine session (`crate::spawn_leaf`) on its own strand in
//! the data dir, named `<parent-stem>/team/<name>` so it shows up in
//! `ka sessions`; `team_send` injects a NEW prompt into the SAME
//! session, so member context accumulates across messages (the
//! difference from delegate's one-shot runs). Members are leaves: no
//! delegate hand, no nested teams, and asks auto-deny — the member
//! watcher answers every ask with the deny option, because nobody is
//! there to answer.
//!
//! Completion rides the transcript as one note per finished member
//! turn, `team · <name> — <first line>` (full result on the member's
//! strand; `team_read` fetches it, cap + spill like every result
//! surface). Gated only by safe mode — session-spawning automation is
//! a customization tier the bare surface pins off (cron/monitor
//! precedent); `[delegate] team_max` caps the table (0 disables teams
//! with an informative refusal).

use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{Clearance, Hand, HandContext, HandDef, Spill, ToolOutput};

/// Transcript prefix of every member completion note (twin of
/// `ka_protocol::TAN_NOTE_PREFIX`, engine-local because nothing parses
/// it — the note is transcript text, not a TUI counter feed).
pub const TEAM_NOTE_PREFIX: &str = "team · ";

/// Default live-member cap (`[delegate] team_max`).
pub const TEAM_MAX_DEFAULT: u32 = 8;

/// First-line cap for the completion note (chars — the delegate note
/// cap).
const NOTE_CAP: usize = 200;

/// Replies over this size park on the spill store; the note keeps the
/// first line and the `spill://` pointer (the tangential/delegate
/// convention).
const SPILL_AT: usize = 32 * 1024;

/// `team_read`'s result cap (chars — the tasks-read standard); the
/// full reply parks on the spill store with a pointer.
const READ_CAP: usize = 8_000;

/// Contract-test seam: a factory handing every spawned member its own
/// scripted speaker. Production passes None — members inherit the
/// session model through the parent config.
pub type MemberSpeakers =
    Arc<dyn Fn() -> (ka_dialect::Wire, Arc<dyn ka_dialect::speaker::Speaker>) + Send + Sync>;

/// A member's lifecycle state, surfaced by `team_status` and the
/// /tasks roster. `done` = a prompt cycle finished with an unread
/// reply; `team_read` moves it to `idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberState {
    /// Alive, reply delivered, waiting for the next `team_send`.
    Idle,
    /// A prompt cycle is in flight.
    Working,
    /// The cycle finished; the last reply is unread.
    Done,
    /// Dismissed, or the member engine ended (crash included).
    Dead,
}

impl MemberState {
    /// The row/status label.
    pub fn label(&self) -> &'static str {
        match self {
            MemberState::Idle => "idle",
            MemberState::Working => "working",
            MemberState::Done => "done",
            MemberState::Dead => "dead",
        }
    }
}

/// The parent-stem member names hang off: the parent's session name
/// (`Name` record) when one is set, else the slug of its first user
/// prompt — the natural session stem — else `session`.
fn parent_stem(strand: &ka_strand::StrandFile) -> String {
    let named = strand.records().iter().rev().find_map(|r| match r {
        ka_strand::Record::Name { name, .. } => Some(name.clone()),
        _ => None,
    });
    let raw = named.or_else(|| {
        strand.records().iter().find_map(|r| match r {
            ka_strand::Record::Message {
                role: ka_strand::Role::User,
                content,
                ..
            } => Some(content.lines().next().unwrap_or_default().to_string()),
            _ => None,
        })
    });
    slugify(&raw.unwrap_or_else(|| "session".to_string()))
}

/// Lowercase `[a-z0-9-]` stem: other characters fold to `-` (collapsed,
/// trimmed), the result capped at 24 chars, empty → `session`.
fn slugify(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars().take(64) {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let mut stem: String = out.trim_matches('-').chars().take(24).collect();
    while stem.ends_with('-') {
        stem.pop();
    }
    if stem.is_empty() {
        stem.push_str("session");
    }
    stem
}

/// The member-name grammar: 1–32 chars of `[a-z0-9-]`.
fn valid_member_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The first non-empty line of a text, whitespace-trimmed.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
}

/// One team member row (the table's live state).
struct Member {
    name: String,
    /// The member's session name in `ka sessions`
    /// (`<stem>/team/<name>`).
    session_name: String,
    state: MemberState,
    turns: u32,
    /// The last reply text (full — `team_read` caps and spills).
    last: Option<String>,
    /// Last error the member engine reported (failure-note fallback).
    error: Option<String>,
    /// The member engine's command sender. `None` once dismissed —
    /// dropping it closes the engine's channel, which is its clean
    /// shutdown (session_end path, the strand left resumable).
    cmds: Option<mpsc::Sender<ka_protocol::Command>>,
    /// The member engine task; aborted on dismiss of a live turn's
    /// parent drop (members die with the parent engine).
    engine: tokio::task::AbortHandle,
}

/// Shared table state behind [`TeamTable`] (watchers hold this, not
/// the outer handle — the outer handle's Drop is the parent-owned
/// kill switch and must not be kept alive by watchers).
struct TeamInner {
    members: parking_lot::Mutex<Vec<Member>>,
    max: u32,
    cwd: std::path::PathBuf,
    catalog: ka_dialect::Catalog,
    /// The parent's config as the member template (same model, rules,
    /// identity); leaf bootstrap strips MCP servers and the session
    /// lifecycle hooks at member start.
    member_template: crate::config::Config,
    /// The parent's live permission mode (shared cell — mid-session
    /// `SetMode`/strand restore reach members spawned afterwards).
    mode: crate::engine::ModeSlot,
    events: mpsc::Sender<ka_protocol::Event>,
    stem: String,
    speakers: Option<MemberSpeakers>,
}

/// The parent-owned team table: spawn, drive, read, dismiss. Cloned
/// cheaply (the hand and the engine Ctx each hold one); dropping the
/// LAST clone — the parent engine dropping — kills every member engine
/// and lets their watchers wind down.
#[derive(Clone)]
pub struct TeamTable {
    inner: Arc<TeamInner>,
}

impl TeamTable {
    /// New table for one parent session. `strand` is the parent's own
    /// strand (the member-name stem's source); `member_template` is the
    /// parent config captured before the engine consumed it.
    #[allow(clippy::too_many_arguments)] // one table, every arg a distinct engine handle
    pub fn new(
        max: u32,
        cwd: &std::path::Path,
        catalog: ka_dialect::Catalog,
        member_template: crate::config::Config,
        mode: crate::engine::ModeSlot,
        events: mpsc::Sender<ka_protocol::Event>,
        strand: &ka_strand::StrandFile,
        speakers: Option<MemberSpeakers>,
    ) -> Self {
        Self {
            inner: Arc::new(TeamInner {
                members: parking_lot::Mutex::new(Vec::new()),
                max,
                cwd: cwd.to_path_buf(),
                catalog,
                member_template,
                mode,
                events,
                stem: parent_stem(strand),
                speakers,
            }),
        }
    }

    /// `team_spawn`: validate, cap, arm a leaf member on `prompt`, and
    /// name its strand `<stem>/team/<name>` (a `Name` record via the
    /// member's own `SetName` path, queued before the opening prompt so
    /// it lands while the member is still idle). The opening prompt
    /// runs immediately — the member is born working.
    pub async fn spawn(&self, name: &str, prompt: &str) -> Result<String, String> {
        if crate::conventions::bare_mode() {
            return Err("team: unavailable in safe mode".to_string());
        }
        if self.inner.max == 0 {
            return Err(
                "team: teams are disabled ([delegate] team_max = 0) — raise the cap to spawn \
                 members"
                    .to_string(),
            );
        }
        let name = name.trim();
        if !valid_member_name(name) {
            return Err(format!(
                "team: invalid name '{name}' — use 1-32 chars of [a-z0-9-]"
            ));
        }
        {
            let mut members = self.inner.members.lock();
            // dismissed rows keep their last reply readable but hold no
            // name or slot: a dismissed name is respawneable and the cap
            // counts live members only
            members.retain(|m| !(m.name == name && m.state == MemberState::Dead));
            if members.iter().any(|m| m.name == name) {
                return Err(format!("team: member '{name}' already exists"));
            }
            let live = members
                .iter()
                .filter(|m| m.state != MemberState::Dead)
                .count();
            if live as u32 >= self.inner.max {
                return Err(format!(
                    "team: the team is full ({live}/{} members) — dismiss one first \
                     ([delegate] team_max)",
                    self.inner.max
                ));
            }
        }
        let session_name = format!("{}/team/{name}", self.inner.stem);
        // the member template with the parent's live cwd + mode pinned
        // (config-level mode would miss strand-restored overrides; the
        // slot keeps later SetMode/steering changes live for spawns too)
        let mut member_cfg = self.inner.member_template.clone();
        member_cfg.cwd = Some(self.inner.cwd.to_string_lossy().into_owned());
        member_cfg.mode = Some(*self.inner.mode.read());
        let speaker = self.inner.speakers.as_ref().map(|f| f());
        let (handle, task) =
            crate::engine::spawn_leaf(member_cfg, self.inner.catalog.clone(), speaker);
        // order matters: the strand name lands first (the member is
        // idle during bootstrap), the opening prompt second. Both
        // buffer in the engine's command channel.
        let _ = handle
            .commands
            .send(ka_protocol::Command::SetName {
                name: session_name.clone(),
            })
            .await;
        let _ = handle
            .commands
            .send(ka_protocol::Command::Prompt {
                text: prompt.to_string(),
                schema: None,
                images: Vec::new(),
                allowed_tools: None,
                model: None,
                skills: Vec::new(),
            })
            .await;
        {
            let mut members = self.inner.members.lock();
            // Re-check under lock after spawning to close the TOCTOU window
            // if two concurrent spawns raced on the same name or the cap.
            members.retain(|m| !(m.name == name && m.state == MemberState::Dead));
            if members.iter().any(|m| m.name == name) {
                task.abort();
                drop(handle.commands);
                return Err(format!("team: member '{name}' already exists"));
            }
            let live = members
                .iter()
                .filter(|m| m.state != MemberState::Dead)
                .count();
            if live as u32 >= self.inner.max {
                task.abort();
                drop(handle.commands);
                return Err(format!(
                    "team: the team is full ({live}/{} members) — dismiss one first \
                     ([delegate] team_max)",
                    self.inner.max
                ));
            }
            members.push(Member {
                name: name.to_string(),
                session_name: session_name.clone(),
                state: MemberState::Working,
                turns: 0,
                last: None,
                error: None,
                cmds: Some(handle.commands.clone()),
                engine: task.abort_handle(),
            });
        }
        // the watcher consumes the member's event stream: reply
        // capture, ask auto-deny, turn accounting, completion notes.
        // Weak ref breaks the Arc<TeamInner> cycle so dropping TeamTable
        // runs TeamInner::drop and aborts all member engines.
        tokio::spawn(watch_member(
            Arc::downgrade(&self.inner),
            name.to_string(),
            handle.events,
        ));
        Ok(format!(
            "team: member {name} spawned ({session_name}) — opening prompt running; the reply \
             arrives as a `{}{name} — …` note (team_read fetches it, team_status tracks it)",
            TEAM_NOTE_PREFIX
        ))
    }

    /// `team_send`: inject a new prompt into the member's SAME session.
    /// Working members get their input queued (it runs as a follow-on
    /// turn when the cycle settles); idle/done members get a prompt.
    /// Dismissed members refuse — the session is gone.
    pub async fn send(&self, name: &str, message: &str) -> Result<String, String> {
        if crate::conventions::bare_mode() {
            return Err("team: unavailable in safe mode".to_string());
        }
        // state read and Working-marking share one lock critical
        // section: two concurrent `team send` calls must not both take
        // the Prompt path (the member engine silently discards a second
        // Prompt mid-turn — the loser has to land on Queue instead).
        // Known residual window: a Queue aimed at a member that just
        // went idle (watcher not yet scheduled) parks in the engine's
        // main-loop queue until the member's next turn — it replays
        // then, out of order but never lost. v1 accepts this; a fix
        // needs an engine-side idle-Queue dispatch.
        let (cmds, cmd) = {
            let mut members = self.inner.members.lock();
            let m = members
                .iter_mut()
                .find(|m| m.name == name)
                .ok_or_else(|| self.no_such_member(name))?;
            let cmds = m.cmds.clone().ok_or_else(|| {
                format!("team: member '{name}' is dismissed — team_spawn creates a new one")
            })?;
            match m.state {
                MemberState::Working => (
                    cmds,
                    ka_protocol::Command::Queue {
                        text: message.to_string(),
                    },
                ),
                MemberState::Dead => {
                    return Err(format!(
                        "team: member '{name}' is dismissed — team_spawn creates a new one"
                    ));
                }
                _ => {
                    m.state = MemberState::Working;
                    (
                        cmds,
                        ka_protocol::Command::Prompt {
                            text: message.to_string(),
                            schema: None,
                            images: Vec::new(),
                            allowed_tools: None,
                            model: None,
                            skills: Vec::new(),
                        },
                    )
                }
            }
        };
        if cmds.send(cmd).await.is_err() {
            self.set_state(name, MemberState::Dead);
            return Err(format!(
                "team: member '{name}' is gone (its engine ended) — team_spawn creates a new one"
            ));
        }
        Ok(format!(
            "team: message sent to {name} — the reply arrives as a `{}{name} — …` note",
            TEAM_NOTE_PREFIX
        ))
    }

    /// `team_read`: the member's last reply, capped like every result
    /// surface; the full text parks on the spill store. Reading also
    /// settles a `done` member back to `idle` (the reply is delivered).
    pub fn read(&self, name: &str, spill: &Spill) -> Result<String, String> {
        let text = {
            let mut members = self.inner.members.lock();
            let m = members
                .iter_mut()
                .find(|m| m.name == name)
                .ok_or_else(|| self.no_such_member(name))?;
            match m.last.clone() {
                Some(text) => {
                    if m.state == MemberState::Done {
                        m.state = MemberState::Idle;
                    }
                    text
                }
                None => {
                    return Err(format!(
                        "team: member '{name}' has no reply yet ({})",
                        m.state.label()
                    ));
                }
            }
        };
        let mut out: String = text.chars().take(READ_CAP).collect();
        if text.chars().count() > READ_CAP {
            match spill.park(&text) {
                Ok(ptr) => out.push_str(&format!("\n[full reply: {ptr}]")),
                Err(_) => out.push_str("\n[full reply: spilling failed — re-ask the member]"),
            }
        }
        Ok(out)
    }

    /// The team table rows: `name  state  turns N  <last first line>`.
    /// Shared by `team_status` (model-facing) and the /tasks roster
    /// (`team ` prefix — the TUI pill ignores it).
    pub fn rows(&self) -> Vec<String> {
        let members = self.inner.members.lock();
        members
            .iter()
            .map(|m| {
                let head = m
                    .last
                    .as_deref()
                    .map(first_line)
                    .filter(|l| !l.is_empty())
                    .map(|l| super::delegate::truncate_for_note(l, 60))
                    .unwrap_or_else(|| "-".to_string());
                format!(
                    "team {}  {:7}  turns {:<3} {}",
                    m.name,
                    m.state.label(),
                    m.turns,
                    head
                )
            })
            .collect()
    }

    /// `team_dismiss`: end the member's session (dropping its command
    /// sender is the engine's clean shutdown — the strand stays
    /// resumable), mark the row dead. The member stays listed until the
    /// parent engine drops.
    pub fn dismiss(&self, name: &str) -> Result<String, String> {
        let session_name;
        {
            let mut members = self.inner.members.lock();
            let m = members
                .iter_mut()
                .find(|m| m.name == name)
                .ok_or_else(|| self.no_such_member(name))?;
            if m.state == MemberState::Dead {
                return Err(format!("team: member '{name}' is already dismissed"));
            }
            m.cmds = None; // channel close = the engine's clean exit
            m.state = MemberState::Dead;
            session_name = m.session_name.clone();
        }
        Ok(format!(
            "team: member {name} dismissed — its session remains in `ka sessions` \
             ({session_name})"
        ))
    }

    fn no_such_member(&self, name: &str) -> String {
        format!("team: no such member '{name}' (team_status lists the table)")
    }

    fn set_state(&self, name: &str, state: MemberState) {
        if let Some(m) = self
            .inner
            .members
            .lock()
            .iter_mut()
            .find(|m| m.name == name)
        {
            m.state = state;
        }
    }

    /// One finished member turn: account it, store the reply, emit the
    /// completion note (suppressed for dismissed members).
    async fn finish_turn(inner: &TeamInner, name: &str, reply: String, failed: Option<String>) {
        let mut note = {
            let mut members = inner.members.lock();
            let Some(m) = members.iter_mut().find(|m| m.name == name) else {
                return;
            };
            if m.state == MemberState::Dead {
                return; // dismissed — the transcript already closed it out
            }
            m.turns += 1;
            if !reply.trim().is_empty() {
                m.last = Some(reply.clone());
            }
            if failed.is_some() {
                m.error = failed.clone();
            }
            let head = if !reply.trim().is_empty() {
                first_line(&reply).to_string()
            } else {
                failed
                    .as_deref()
                    .map(first_line)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| "no reply".to_string())
            };
            format!(
                "{}{} — {}",
                TEAM_NOTE_PREFIX,
                name,
                super::delegate::truncate_for_note(&head, NOTE_CAP)
            )
        };
        if reply.len() > SPILL_AT {
            match Spill::new().park(&reply) {
                Ok(ptr) => note.push_str(&format!(" (full: {ptr})")),
                Err(_) => note.push_str(" (full reply via team_read — spilling failed)"),
            }
        }
        inner
            .events
            .send(ka_protocol::Event::Note { message: note })
            .await
            .ok();
    }
}

impl Drop for TeamInner {
    /// The parent engine is dropping its table: kill every member
    /// engine (the members die with the parent — nothing cross-process
    /// survives) and drop their command senders. Watchers need no
    /// aborts: every one exits when its member's event channel closes,
    /// which the aborts cause. Implemented on the shared inner so the
    /// cleanup fires when the LAST clone dies, not on any clone.
    fn drop(&mut self) {
        let mut members = self.members.lock();
        for m in members.drain(..) {
            m.engine.abort();
            drop(m.cmds);
        }
    }
}

/// Pick the deny/decline option index from an Ask prompt's questions,
/// falling back to the last option (never assuming index 2 blindly).
pub(super) fn deny_choice(questions: &[ka_protocol::AskQuestion]) -> usize {
    let Some(q) = questions.first() else {
        return 2;
    };
    if let Some(idx) = q.options.iter().position(|o| {
        let l = o.to_ascii_lowercase();
        l.contains("deny") || l.contains("decline") || l.contains("no") || l.contains("cancel")
    }) {
        return idx;
    }
    q.options.len().saturating_sub(1)
}

/// One member's event watcher: reply capture, ask auto-deny (nobody is
/// there to answer — the deny option is answered on the member's
/// behalf), turn accounting, and the state machine. Exits when the
/// member engine ends or the parent table drops.
async fn watch_member(
    inner: std::sync::Weak<TeamInner>,
    name: String,
    mut events: mpsc::Receiver<ka_protocol::Event>,
) {
    let mut reply = String::new();
    let mut thought = String::new();
    let mut last_error: Option<String> = None;
    // `Idle` only marks the cycle done once a turn actually ran: the
    // member's bootstrap (`SetName` before the opening prompt) emits a
    // spurious idle that must not flip a freshly spawned member to done
    let mut turns_seen = 0u32;
    while let Some(evt) = events.recv().await {
        let Some(inner) = inner.upgrade() else {
            break;
        };
        match evt {
            ka_protocol::Event::Delta {
                kind: ka_protocol::DeltaKind::Text(t),
            } => reply.push_str(&t),
            ka_protocol::Event::Delta {
                kind: ka_protocol::DeltaKind::Thought(t),
            } => thought.push_str(&t),
            ka_protocol::Event::Ask { id, questions } => {
                let choice = deny_choice(&questions);
                let cmds = inner
                    .members
                    .lock()
                    .iter()
                    .find(|m| m.name == name)
                    .and_then(|m| m.cmds.clone());
                if let Some(cmds) = cmds {
                    tokio::spawn(async move {
                        let _ = cmds
                            .send(ka_protocol::Command::Answer {
                                question: id,
                                choice,
                            })
                            .await;
                    });
                }
            }
            ka_protocol::Event::Error { message, .. } => last_error = Some(message),
            ka_protocol::Event::TurnFinished { .. } => {
                turns_seen += 1;
                let mut text = std::mem::take(&mut reply);
                if text.trim().is_empty() {
                    text = std::mem::take(&mut thought);
                } else {
                    thought.clear();
                }
                let failed = match &last_error {
                    // a turn that ended in an error with no text keeps
                    // the reason for its note; the error clears so the
                    // next turn's note reflects only fresh failures
                    Some(e) if text.trim().is_empty() => Some(e.clone()),
                    _ => None,
                };
                if failed.is_some() {
                    last_error = None;
                }
                TeamTable::finish_turn(&inner, &name, text, failed).await;
            }
            ka_protocol::Event::Idle => {
                // a cycle end only settles a member that actually ran a
                // turn: the bootstrap `SetName` emits a spurious idle
                // before the opening prompt and must not flip the fresh
                // member to done
                if turns_seen > 0 {
                    if let Some(m) = inner
                        .members
                        .lock()
                        .iter_mut()
                        .find(|m| m.name == name && m.state != MemberState::Dead)
                    {
                        m.state = MemberState::Done;
                    }
                }
            }
            _ => {}
        }
    }
    // the member engine ended (dismiss close, parent drop, or crash):
    // the row reads dead — but never resurrect a dismissed member's
    // state change, and never note (the dismissal already did)
    if let Some(inner) = inner.upgrade() {
        if let Some(m) = inner
            .members
            .lock()
            .iter_mut()
            .find(|m| m.name == name && m.state != MemberState::Dead)
        {
            m.state = MemberState::Dead;
        }
    }
}

/// The `team` hand: five actions over the shared table — the model's
/// window into its member sessions. Spawn/send/dismiss run at exec
/// tier (they create, drive, and end sessions); status/read are read
/// tier via [`Hand::clearance_for`].
pub struct TeamsHand {
    table: TeamTable,
}

impl TeamsHand {
    /// Hand over the shared table.
    pub fn new(table: TeamTable) -> Self {
        Self { table }
    }
}

impl Hand for TeamsHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "team".to_string(),
            description: "Orchestrate persistent team members: named child sessions that \
                KEEP their context across messages (unlike delegate's one-shot agents). \
                {\"action\": \"spawn\", \"name\": \"scout\", \"prompt\": \"...\"} starts one \
                (1-32 chars of [a-z0-9-]); its opening prompt runs immediately. \
                {\"action\": \"send\", \"name\": \"scout\", \"message\": \"...\"} injects the \
                next prompt into the SAME session — context accumulates between messages. \
                Each finished member turn lands as a `team · <name> — <first line>` note. \
                {\"action\": \"status\"} lists the table (state, turns, last line); \
                {\"action\": \"read\", \"name\": \"scout\"} returns a member's last reply; \
                {\"action\": \"dismiss\", \"name\": \"scout\"} ends its session. \
                Members cannot spawn sub-agents and their asks auto-deny — brief them \
                self-contained."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["spawn", "send", "status", "read", "dismiss"],
                        "description": "spawn, send, status, read, or dismiss"
                    },
                    "name": {
                        "type": "string",
                        "description": "Member name, 1-32 chars of [a-z0-9-] (spawn/send/read/dismiss)"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The opening prompt (spawn)"
                    },
                    "message": {
                        "type": "string",
                        "description": "The next prompt to inject (send)"
                    }
                }
            }),
            clearance: Clearance::Exec,
            read_only: false,
        }
    }

    fn clearance_for(&self, args: &Value) -> Clearance {
        match args.get("action").and_then(Value::as_str) {
            Some("status") | Some("read") => Clearance::Read,
            _ => Clearance::Exec,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("status");
            let name =
                |what: &str| ToolOutput::err(format!("team {what}: missing required 'name'"));
            match action {
                "spawn" => {
                    let Some(name) = args.get("name").and_then(Value::as_str) else {
                        return name("spawn");
                    };
                    let Some(prompt) = args.get("prompt").and_then(Value::as_str) else {
                        return ToolOutput::err("team spawn: missing required 'prompt'");
                    };
                    match self.table.spawn(name, prompt).await {
                        Ok(msg) => ToolOutput::ok(msg),
                        Err(e) => ToolOutput::err(e),
                    }
                }
                "send" => {
                    let Some(name) = args.get("name").and_then(Value::as_str) else {
                        return name("send");
                    };
                    let Some(message) = args.get("message").and_then(Value::as_str) else {
                        return ToolOutput::err("team send: missing required 'message'");
                    };
                    match self.table.send(name, message).await {
                        Ok(msg) => ToolOutput::ok(msg),
                        Err(e) => ToolOutput::err(e),
                    }
                }
                "status" => {
                    let rows = self.table.rows();
                    if rows.is_empty() {
                        return ToolOutput::ok("no team members — team_spawn adds one");
                    }
                    ToolOutput::ok(rows.join("\n"))
                }
                "read" => {
                    let Some(name) = args.get("name").and_then(Value::as_str) else {
                        return name("read");
                    };
                    match self.table.read(name, &ctx.spill) {
                        Ok(text) => ToolOutput::ok(text),
                        Err(e) => ToolOutput::err(e),
                    }
                }
                "dismiss" => {
                    let Some(name) = args.get("name").and_then(Value::as_str) else {
                        return name("dismiss");
                    };
                    match self.table.dismiss(name) {
                        Ok(msg) => ToolOutput::ok(msg),
                        Err(e) => ToolOutput::err(e),
                    }
                }
                other => ToolOutput::err(format!(
                    "team: unknown action {other:?} (expected \"spawn\", \"send\", \"status\", \
                     \"read\", or \"dismiss\")"
                )),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn member_name_grammar() {
        assert!(valid_member_name("scout"));
        assert!(valid_member_name("a"));
        assert!(valid_member_name("re-run-2"));
        assert!(!valid_member_name(""));
        assert!(!valid_member_name("Scout"));
        assert!(!valid_member_name("with space"));
        assert!(!valid_member_name("under_score"));
        assert!(!valid_member_name(&"x".repeat(33)));
        assert!(valid_member_name(&"x".repeat(32)));
    }

    #[test]
    fn slug_folds_and_caps() {
        assert_eq!(slugify("Fix the Auth Bug!"), "fix-the-auth-bug");
        assert_eq!(slugify("  --  "), "session");
        assert_eq!(slugify("Árbol"), "rbol");
        let long = slugify(&"a".repeat(80));
        assert_eq!(long.len(), 24);
        assert_eq!(slugify("a___b"), "a-b");
    }

    #[test]
    fn parent_stem_prefers_the_session_name_then_the_first_prompt() {
        let data = std::env::temp_dir().join(format!("ka-team-stem-data-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        ka_strand::set_data_dir_for_tests(data.clone());
        let dir = std::env::temp_dir().join(format!("ka-team-stem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut strand = ka_strand::StrandFile::create(&dir, None).unwrap();
        // bare strand: falls back to `session`
        assert_eq!(parent_stem(&strand), "session");
        strand
            .append(ka_strand::Record::Message {
                id: ka_strand::new_record_id(),
                role: ka_strand::Role::User,
                content: "Fix the pump leak\nplease".to_string(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
                images: Vec::new(),
            })
            .unwrap();
        assert_eq!(parent_stem(&strand), "fix-the-pump-leak");
        // the user-set name wins over the prompt-derived stem
        strand
            .append(ka_strand::Record::Name {
                id: ka_strand::new_record_id(),
                name: "main".to_string(),
            })
            .unwrap();
        assert_eq!(parent_stem(&strand), "main");
        ka_strand::set_data_dir_for_tests(std::env::temp_dir());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// A one-row table with a live command channel: `(table, cmds rx,
    /// parent-note rx)`. `watch_member` runs against the same private
    /// shape the engine builds, so the watcher contract is tested
    /// without a member engine.
    fn one_row_table(
        tag: &str,
    ) -> (
        TeamTable,
        mpsc::Receiver<ka_protocol::Command>,
        mpsc::Receiver<ka_protocol::Event>,
    ) {
        let dir = std::env::temp_dir().join(format!("ka-team-watch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let strand = ka_strand::StrandFile::create(&dir, None).unwrap();
        let (events, note_rx) = mpsc::channel(64);
        let table = TeamTable::new(
            8,
            &dir,
            ka_dialect::Catalog::parse(
                "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
            )
            .unwrap(),
            crate::config::Config::default(),
            std::sync::Arc::new(parking_lot::RwLock::new(ka_protocol::Mode::Allow)),
            events,
            &strand,
            None,
        );
        let (cmds, cmds_rx) = mpsc::channel(8);
        let engine = tokio::spawn(async {});
        table.inner.members.lock().push(Member {
            name: "scout".to_string(),
            session_name: "session/team/scout".to_string(),
            state: MemberState::Working,
            turns: 0,
            last: None,
            error: None,
            cmds: Some(cmds),
            engine: engine.abort_handle(),
        });
        (table, cmds_rx, note_rx)
    }

    /// The watcher auto-denies every ask (choice 2 = the deny option)
    /// — nobody is there to answer for a member.
    #[tokio::test]
    async fn watcher_auto_denies_asks() {
        let (table, mut cmds_rx, _notes) = one_row_table("ask");
        let inner = Arc::downgrade(&table.inner);
        let (evt_tx, evt_rx) = mpsc::channel(8);
        let watcher = tokio::spawn(watch_member(inner, "scout".to_string(), evt_rx));
        evt_tx
            .send(ka_protocol::Event::Ask {
                id: ka_protocol::AskId("ask-1".into()),
                questions: Vec::new(),
            })
            .await
            .unwrap();
        drop(evt_tx); // let the watcher wind down after the answer
        let answer = tokio::time::timeout(std::time::Duration::from_secs(5), cmds_rx.recv())
            .await
            .expect("deny answer arrives")
            .expect("channel alive");
        assert!(
            matches!(answer, ka_protocol::Command::Answer { choice: 2, .. }),
            "asks auto-deny: {answer:?}"
        );
        let _ = watcher.await;
    }

    /// Turn accounting: text deltas accumulate into the member's last
    /// reply, each TurnFinished emits exactly one `team · ` note, and a
    /// closed event channel (engine ended) marks the row dead.
    #[tokio::test]
    async fn watcher_notes_turns_and_marks_engine_death() {
        let (table, _cmds, mut notes) = one_row_table("notes");
        let inner = Arc::downgrade(&table.inner);
        let (evt_tx, evt_rx) = mpsc::channel(8);
        let watcher = tokio::spawn(watch_member(inner, "scout".to_string(), evt_rx));
        // the bootstrap idle (before any turn) must not flip the row
        evt_tx.send(ka_protocol::Event::Idle).await.unwrap();
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(
            table.rows()[0].contains("working"),
            "pre-turn idle is ignored: {}",
            table.rows()[0]
        );
        evt_tx
            .send(ka_protocol::Event::Delta {
                kind: ka_protocol::DeltaKind::Text("All done: 3 files.\nextra line".into()),
            })
            .await
            .unwrap();
        evt_tx
            .send(ka_protocol::Event::TurnFinished {
                stop: ka_protocol::Stop::Done,
                usage: ka_protocol::Usage::default(),
            })
            .await
            .unwrap();
        // the cycle idle settles the row (a turn actually ran)
        evt_tx.send(ka_protocol::Event::Idle).await.unwrap();
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(
            table.rows()[0].contains("done"),
            "post-turn idle marks done: {}",
            table.rows()[0]
        );
        drop(evt_tx); // the member engine "ends"
        let _ = watcher.await;
        let note = tokio::time::timeout(std::time::Duration::from_secs(5), notes.recv())
            .await
            .expect("note arrives")
            .expect("channel alive");
        let ka_protocol::Event::Note { message } = note else {
            panic!("expected a note, got {note:?}")
        };
        assert_eq!(message, "team · scout — All done: 3 files.");
        let row = table.rows()[0].clone();
        assert!(row.contains("dead"), "engine death marks dead: {row}");
        assert!(row.contains("turns 1"), "the turn was accounted: {row}");
    }
}
