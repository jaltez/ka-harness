//! The jobs hand: visibility and control over bash commands the engine
//! auto-backgrounded after the `background_after_ms` threshold.
//!
//! Backgrounded commands run FULLY DETACHED: `sh -c '( cmd ) > spill
//! 2>&1; echo $? > spill.done'` in its own process group, with no
//! in-session watcher. The [`JobTable`] persists entries to
//! `jobs.jsonl` (state dir), liveness is derived from the `.done`
//! marker plus pid liveness, and a later session adopts surviving
//! entries — killing ka never kills detached work.

use std::io::Seek;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};

use super::{Clearance, Hand, HandContext, HandDef, ToolOutput};

/// Lifecycle state of one auto-backgrounded bash job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Child still running; output streams into the spill file.
    Running,
    /// Child finished with this exit code (-1 when killed by signal).
    Exited(i32),
}

/// Shared registry of detached background jobs. One per voice; the bash
/// hand registers promotions, this hand lists and kills.
pub struct JobTable {
    inner: parking_lot::Mutex<TableInner>,
    /// `jobs.jsonl` path; None = in-memory only (tests).
    path: parking_lot::Mutex<Option<PathBuf>>,
}

#[derive(Default)]
struct TableInner {
    next_id: u64,
    jobs: Vec<JobEntry>,
}

/// One persisted/backgrounded job. State is DERIVED (see
/// [`derived_state`]), never stored.
struct JobEntry {
    id: u64,
    cmd: String,
    started_epoch_ms: u64,
    spill: PathBuf,
    pid: Option<u32>,
}

/// Immutable snapshot row handed to the jobs hand.
#[derive(Debug)]
pub struct JobView {
    pub id: u64,
    pub cmd: String,
    pub started: std::time::SystemTime,
    pub state: JobState,
    pub spill: PathBuf,
}

/// The persistent job registry file (`jobs.jsonl` under the state dir).
pub fn default_jobs_file() -> Option<PathBuf> {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("ka/jobs.jsonl")
        .into()
}

/// `<spill>.done`: the detached shell writes the exit code there.
pub(crate) fn done_path(spill: &Path) -> PathBuf {
    PathBuf::from(format!("{}.done", spill.display()))
}

/// Pid liveness (`/proc/<pid>` — the Linux `kill -0` equivalent).
/// Zombies count as dead: unreaped children of this very process must
/// not keep a killed job looking alive.
pub(crate) fn pid_alive(pid: Option<u32>) -> bool {
    let Some(p) = pid else {
        return false;
    };
    match std::fs::read_to_string(format!("/proc/{p}/stat")) {
        Ok(stat) => match stat.rsplit_once(')') {
            Some((_, rest)) => !rest.split_whitespace().next().is_some_and(|s| s == "Z"),
            None => true,
        },
        Err(_) => false,
    }
}

/// Derive a job's lifecycle state from disk: `.done` present → exited
/// with the recorded code; pid live → running; otherwise died unknown.
fn derived_state(spill: &Path, pid: Option<u32>) -> JobState {
    if let Ok(text) = std::fs::read_to_string(done_path(spill)) {
        return JobState::Exited(text.trim().parse::<i32>().unwrap_or(-1));
    }
    if pid_alive(pid) {
        JobState::Running
    } else {
        JobState::Exited(-1)
    }
}

impl Default for JobTable {
    fn default() -> Self {
        Self {
            inner: parking_lot::Mutex::default(),
            path: parking_lot::Mutex::new(None),
        }
    }
}

impl JobTable {
    /// An empty, in-memory table (tests).
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the persistence file: adopt surviving jobs (dead entries
    /// are compacted away) and rewrite the file. Must run before the
    /// first registration.
    pub fn set_path(&self, path: PathBuf) {
        let mut t = self.inner.lock();
        let mut adopted: Vec<JobEntry> = Vec::new();
        let mut next_id = 0u64;
        for line in std::fs::read_to_string(&path).unwrap_or_default().lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let entry = JobEntry {
                id: v["id"].as_u64().unwrap_or(0),
                cmd: v["cmd"].as_str().unwrap_or_default().to_string(),
                started_epoch_ms: v["started_ms"].as_u64().unwrap_or(0),
                spill: PathBuf::from(v["spill"].as_str().unwrap_or_default()),
                pid: v["pid"].as_u64().map(|p| p as u32),
            };
            next_id = next_id.max(entry.id);
            // adopt only survivors; dead entries silently compact away
            if derived_state(&entry.spill, entry.pid) == JobState::Running {
                adopted.push(entry);
            }
        }
        t.next_id = next_id;
        t.jobs = adopted;
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
        self.persist_locked(&t, &path);
        *self.path.lock() = Some(path);
    }

    /// Register a promoted detached command; returns its job id
    /// (1-based) and persists the entry.
    pub fn register(
        &self,
        cmd: String,
        started_epoch_ms: u64,
        spill: PathBuf,
        pid: Option<u32>,
    ) -> u64 {
        let mut t = self.inner.lock();
        t.next_id += 1;
        let id = t.next_id;
        let entry = JobEntry {
            id,
            cmd,
            started_epoch_ms,
            spill,
            pid,
        };
        t.jobs.push(entry);
        if let Some(path) = self.path.lock().clone() {
            let Some(e) = t.jobs.last() else {
                return id;
            };
            let v = serde_json::json!({
                "id": id,
                "cmd": e.cmd,
                "started_ms": e.started_epoch_ms,
                "spill": e.spill.to_string_lossy(),
                "pid": e.pid,
            });
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = writeln!(f, "{v}");
            }
        }
        id
    }

    /// Kill a running job's process group. Returns a human status; Err
    /// when there is nothing to kill.
    pub fn kill(&self, id: u64) -> Result<String, String> {
        let t = self.inner.lock();
        let Some(e) = t.jobs.iter().find(|e| e.id == id) else {
            return Err(format!("no such job: {id}"));
        };
        if let JobState::Exited(code) = derived_state(&e.spill, e.pid) {
            return Err(format!("job {id} already exited ({code})"));
        }
        if let Some(pid) = e.pid {
            #[cfg(unix)]
            super::bash::kill_tree(pid);
        }
        Ok(format!("killing job {id} (`{}`)", e.cmd))
    }

    /// Snapshot of every job in registration order, state derived from
    /// disk.
    pub fn snapshot(&self) -> Vec<JobView> {
        self.inner
            .lock()
            .jobs
            .iter()
            .map(|e| JobView {
                id: e.id,
                cmd: e.cmd.clone(),
                started: std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_millis(e.started_epoch_ms),
                state: derived_state(&e.spill, e.pid),
                spill: e.spill.clone(),
            })
            .collect()
    }

    fn persist_locked(&self, t: &TableInner, path: &Path) {
        let mut out = String::new();
        for e in &t.jobs {
            let v = serde_json::json!({
                "id": e.id,
                "cmd": e.cmd,
                "started_ms": e.started_epoch_ms,
                "spill": e.spill.to_string_lossy(),
                "pid": e.pid,
            });
            out.push_str(&v.to_string());
            out.push('\n');
        }
        let _ = std::fs::write(path, out);
    }
}

/// The jobs tool.
pub struct JobsHand {
    table: Arc<JobTable>,
}

impl JobsHand {
    /// Hand over the shared job table (must be the voice's `HandContext`
    /// table so listings include promoted bash calls).
    pub fn new(table: Arc<JobTable>) -> Self {
        Self { table }
    }
}

impl Hand for JobsHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "jobs".to_string(),
            description: "List or kill bash commands that were auto-backgrounded after \
                running longer than the background threshold. Arguments: \
                {\"action\": \"list\" | \"kill\", \"id\": <job id>}; action defaults to \
                list, id is required for kill. Each listing shows the job's output tail."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "kill"],
                        "description": "list (default) or kill"
                    },
                    "id": {
                        "type": "integer",
                        "description": "Job id to kill (required for kill)"
                    }
                }
            }),
            clearance: Clearance::Read,
            read_only: false,
        }
    }

    /// Listing is read-only; killing a job is arbitrary execution and
    /// gates at exec tier.
    fn clearance_for(&self, args: &Value) -> Clearance {
        if args.get("action").and_then(Value::as_str) == Some("kill") {
            Clearance::Exec
        } else {
            Clearance::Read
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            match args.get("action").and_then(Value::as_str).unwrap_or("list") {
                "list" => ToolOutput::ok(self.render_list()),
                "kill" => {
                    let Some(id) = args.get("id").and_then(Value::as_u64) else {
                        return ToolOutput::err("jobs kill: missing required 'id'");
                    };
                    match self.table.kill(id) {
                        Ok(msg) => ToolOutput::ok(msg),
                        Err(e) => ToolOutput::err(e),
                    }
                }
                other => ToolOutput::err(format!(
                    "jobs: unknown action {other:?} (expected \"list\" or \"kill\")"
                )),
            }
        })
    }
}

impl JobsHand {
    fn render_list(&self) -> String {
        let jobs = self.table.snapshot();
        if jobs.is_empty() {
            return "no background jobs".to_string();
        }
        let mut out = String::new();
        for j in jobs {
            let state = match j.state {
                JobState::Running => "running".to_string(),
                JobState::Exited(c) => format!("exited {c}"),
            };
            let cmd_head: String = j.cmd.chars().take(60).collect();
            out.push_str(&format!(
                "job {}  {}  {}  `{}`\n",
                j.id,
                state,
                human_elapsed(j.started.elapsed().unwrap_or(std::time::Duration::ZERO),),
                cmd_head
            ));
            let tail = tail_of(&j.spill);
            if !tail.is_empty() {
                for line in tail.lines() {
                    out.push_str("  ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// Seconds/minutes elapsed, compact ("42s", "3m07s").
fn human_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// Last few hundred bytes of a streamed output file, tail lines kept.
/// Empty when the file is missing or has no content yet.
pub(crate) fn tail_of(path: &Path) -> String {
    const MAX_BYTES: u64 = 512;
    const MAX_LINES: usize = 5;
    let Ok(len) = path.metadata().map(|m| m.len()) else {
        return String::new();
    };
    if len == 0 {
        return String::new();
    }
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    if f.seek(std::io::SeekFrom::Start(len.saturating_sub(MAX_BYTES)))
        .is_err()
    {
        return String::new();
    }
    let mut bytes = Vec::new();
    if std::io::Read::read_to_end(&mut f, &mut bytes).is_err() {
        return String::new();
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if len > MAX_BYTES {
        // drop the (probably partial) first line
        if let Some(i) = text.find('\n') {
            text.drain(..=i);
        }
    }
    let mut lines: Vec<&str> = text.lines().collect();
    if lines.len() > MAX_LINES {
        lines.drain(..lines.len() - MAX_LINES);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn ctx_for(dir: &std::path::Path) -> HandContext {
        HandContext {
            cwd: dir.to_path_buf(),
            ledger: Arc::new(parking_lot::Mutex::new(super::super::Ledger::default())),
            spill: Arc::new(super::super::Spill::new()),
            snapshots: Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
            jobs: Arc::new(JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
        }
    }

    /// Spawn a REAL detached-style command writing into `spill`, like
    /// the bash hand does; returns its pid.
    fn spawn_detached(script_body: &str, spill: &std::path::Path) -> u32 {
        use std::process::{Command, Stdio};
        let done = super::done_path(spill);
        let script = format!(
            "( {script_body} ) > {} 2>&1; echo $? > {}",
            spill.to_string_lossy(),
            done.to_string_lossy(),
        );
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&script).stdout(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        cmd.spawn().unwrap().id()
    }

    fn wait_for(pred: impl Fn() -> bool, secs: u64) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if pred() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        pred()
    }

    #[test]
    fn list_reports_state_and_tail() {
        let table = Arc::new(JobTable::new());
        let hand = JobsHand::new(table.clone());
        let dir = std::env::temp_dir().join(format!("ka-jobs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let spill = dir.join("out-1");
        let pid = spawn_detached("echo progress line; sleep 30", &spill);
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let id = table.register("sleep 30".into(), started_ms, spill.clone(), Some(pid));
        assert_eq!(id, 1, "ids are 1-based");
        assert!(wait_for(
            || std::fs::read_to_string(&spill)
                .map(|s| s.contains("progress line"))
                .unwrap_or(false),
            5
        ));

        let ctx = ctx_for(&dir);
        let out = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(hand.execute(&json!({}), &ctx));
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("job 1"), "{}", out.content);
        assert!(out.content.contains("running"), "{}", out.content);
        assert!(out.content.contains("sleep 30"), "{}", out.content);
        assert!(out.content.contains("progress line"), "{}", out.content);

        // exit lands in the listing (via the .done marker)
        std::fs::write(super::done_path(&spill), "0\n").unwrap();
        let out = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(hand.execute(&json!({}), &ctx));
        assert!(out.content.contains("exited 0"), "{}", out.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn kill_terminates_detached_job() {
        let table = Arc::new(JobTable::new());
        let hand = JobsHand::new(table.clone());
        let dir = std::env::temp_dir();
        let ctx = ctx_for(&dir);

        // unknown id
        let out = hand
            .execute(&json!({"action": "kill", "id": 9}), &ctx)
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("no such job"), "{}", out.content);

        // running job: kill terminates the detached pid
        let spill = dir.join(format!("ka-jobs-kill-{}.spill", std::process::id()));
        let pid = spawn_detached("sleep 30", &spill);
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        table.register("sleep 99".into(), started_ms, spill.clone(), Some(pid));
        let out = hand
            .execute(&json!({"action": "kill", "id": 1}), &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("killing job 1"), "{}", out.content);
        assert!(
            wait_for(
                || matches!(
                    table.snapshot().first().map(|v| v.state),
                    Some(JobState::Exited(_))
                ),
                5
            ),
            "detached pid must be dead after kill"
        );

        // kill arg validation
        let out = hand.execute(&json!({"action": "kill"}), &ctx).await;
        assert!(out.is_error, "{}", out.content);
        let out = hand.execute(&json!({"action": "explode"}), &ctx).await;
        assert!(out.is_error, "{}", out.content);
    }

    #[test]
    fn persistence_adopts_survivors_and_compacts_dead() {
        let dir = std::env::temp_dir().join(format!("ka-jobs-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let jsonl = dir.join("jobs.jsonl");

        let spill = dir.join("out-live");
        let pid = spawn_detached("sleep 30", &spill);
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // session A: one live job, one already-dead (pid slot empty)
        let a = Arc::new(JobTable::new());
        a.set_path(jsonl.clone());
        a.register("live job".into(), started_ms, spill.clone(), Some(pid));
        a.register("dead job".into(), started_ms, dir.join("out-dead"), None);

        // session B: adopts the survivor, compacts the dead entry
        let b = Arc::new(JobTable::new());
        b.set_path(jsonl.clone());
        let snap = b.snapshot();
        assert_eq!(snap.len(), 1, "dead entries compact away: {snap:?}");
        assert_eq!(snap[0].cmd, "live job");
        assert_eq!(snap[0].state, JobState::Running);

        // and the adopted job is killable
        let res = b.kill(snap[0].id);
        assert!(res.is_ok(), "{res:?}");
        assert!(
            wait_for(
                || matches!(
                    b.snapshot().first().map(|v| v.state),
                    Some(JobState::Exited(_))
                ),
                5
            ),
            "adopted job must die"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn register_after_open_persists_when_path_set_first() {
        let dir = std::env::temp_dir().join(format!("ka-jobs-openfirst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let jsonl = dir.join("jobs.jsonl");

        // corrected integration order: attach the file BEFORE any
        // registration — every subsequent register must persist
        let spill = dir.join("out-live");
        let pid = spawn_detached("sleep 30", &spill);
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let table = Arc::new(JobTable::new());
        table.set_path(jsonl.clone());
        table.register("persisted job".into(), started_ms, spill, Some(pid));

        let snap = table.snapshot();
        assert_eq!(snap.len(), 1, "{snap:?}");
        assert_eq!(snap[0].cmd, "persisted job");
        let disk = std::fs::read_to_string(&jsonl).unwrap();
        assert!(disk.contains("persisted job"), "{disk}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_clearance_depends_on_action() {
        let hand = JobsHand::new(Arc::new(JobTable::new()));
        assert_eq!(
            hand.clearance_for(&json!({})),
            Clearance::Read,
            "list is read-only"
        );
        assert_eq!(
            hand.clearance_for(&json!({"action": "kill", "id": 1})),
            Clearance::Exec,
            "kill is exec-tier"
        );
    }

    #[test]
    fn tail_reads_last_lines_only() {
        let dir = std::env::temp_dir().join(format!("ka-jobs-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big");
        let body: String = (1..=2_000).map(|i| format!("line-{i}\n")).collect();
        std::fs::write(&path, &body).unwrap();
        let tail = tail_of(&path);
        assert!(tail.contains("line-2000"), "{tail:?}");
        assert!(!tail.contains("line-1\n"), "head must be dropped: {tail:?}");
        assert_eq!(tail.lines().count(), 5);
        assert_eq!(tail_of(&dir.join("missing")), "");
        let empty = dir.join("empty");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(tail_of(&empty), "");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
