//! The monitor hand: spawn a command DETACHED with its combined
//! stdout+stderr streaming into a shared in-memory buffer. `read`
//! returns new lines since a caller-held byte offset (capped per
//! call), `kill` terminates the process tree. At every engine settle
//! tick the table notes new-line activity and one-time process exits,
//! so the model learns a monitor progressed without polling blind.
//!
//! Monitors are session-viva like cron: the buffer lives in memory and
//! dies with the session (the detached JOBS tier is the persistent
//! variant). Killing ka orphans the processes, not the contract.

use std::io::Read;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use super::{Clearance, Hand, HandContext, HandDef, ToolOutput};

/// Per-read caps: at most this many lines or bytes come back per
/// `read` call (whichever bites first) — a chatty monitor must not
/// flood one tool result; the caller iterates via `next_offset`.
const READ_LINE_CAP: usize = 200;
const READ_BYTE_CAP: usize = 32 * 1024;
/// Per-monitor in-memory output cap (2 MiB) so chatty processes cannot OOM ka.
const MAX_BUFFER_BYTES: usize = 2 * 1024 * 1024;
/// Maximum number of concurrently running monitors per session.
const MAX_MONITORS: usize = 16;

/// One monitored process. Shared between the spawning hand and the
/// engine's settle tick.
pub struct MonitorEntry {
    /// Stable id (`mon-1`, …).
    pub id: String,
    /// The `sh -c` command line.
    pub cmd: String,
    /// Child pid (still meaningful after exit until reaped — the
    /// waiter thread reaps promptly).
    pub pid: u32,
    /// Combined stdout+stderr, appended by reader threads.
    pub buffer: Arc<Mutex<String>>,
    /// Set by the waiter thread after reaping, with the exit code.
    pub exit: Arc<Mutex<Option<i32>>>,
    /// Bookkeeping for the settle tick: buffer bytes already noted.
    pub noted_bytes: usize,
    /// The settle tick closes the story exactly once.
    pub close_noted: bool,
}

/// Shared monitor registry: the hand spawns/reads/kills, the engine
/// tick drains notes. One per voice.
#[derive(Default)]
pub struct MonitorTable {
    inner: Mutex<TableInner>,
}

#[derive(Default)]
struct TableInner {
    next_id: u64,
    entries: Vec<MonitorEntry>,
}

/// One `read` result.
pub struct MonitorRead {
    /// New text (≤ 200 lines / 32 KiB), whole lines only.
    pub text: String,
    /// Byte offset to pass as the next `read` offset.
    pub next_offset: usize,
    /// Whether the process has exited (with its code when reaped).
    pub exit_code: Option<i32>,
    /// Whether `text` was capped (more is waiting past `next_offset`).
    pub truncated: bool,
}

impl MonitorTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn `cmd` under `sh -c` with combined output captured by
    /// reader threads; returns (id, pid). The caller's [shell] env
    /// policy applies (bash hand parity).
    pub fn spawn_with_env(
        &self,
        cmd: &str,
        env: &crate::config::ShellEnvPolicy,
    ) -> Result<(String, u32), String> {
        self.spawn_in(cmd, None, env, &ka_sandbox::Policy::Off)
    }

    /// Spawn `cmd` honoring `cwd`, `[shell]` env policy, and `[sandbox]`
    /// policy, enforcing [`MAX_MONITORS`] live monitors and placing the
    /// child in its own process group on Unix.
    pub(crate) fn spawn_in(
        &self,
        cmd: &str,
        cwd: Option<&std::path::Path>,
        env: &crate::config::ShellEnvPolicy,
        sandbox: &ka_sandbox::Policy,
    ) -> Result<(String, u32), String> {
        {
            let t = self.inner.lock();
            let active = t.entries.iter().filter(|e| e.exit.lock().is_none()).count();
            if active >= MAX_MONITORS {
                return Err(format!(
                    "monitor spawn: limit of {MAX_MONITORS} concurrent monitors reached (kill one first)"
                ));
            }
        }
        let work_dir = cwd
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()));
        let mut command = if matches!(sandbox, ka_sandbox::Policy::Off) {
            let mut c = std::process::Command::new("sh");
            c.arg("-c").arg(cmd);
            c
        } else {
            let argv = ka_sandbox::wrap_command(sandbox, cmd, &work_dir)?;
            let mut c = std::process::Command::new(&argv[0]);
            c.args(&argv[1..]);
            c
        };
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        env.apply_to_std(&mut command);
        match ka_sandbox::proxy_env(sandbox) {
            Ok(Some(proxy)) => {
                for (name, value) in proxy.vars() {
                    command.env(name, value);
                }
            }
            Ok(None) => {}
            Err(e) => return Err(format!("monitor spawn: sandbox proxy: {e}")),
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|e| format!("monitor spawn: {e}"))?;
        let pid = child.id();
        let stdout = child.stdout.take().ok_or("monitor spawn: no stdout")?;
        let stderr = child.stderr.take().ok_or("monitor spawn: no stderr")?;

        let mut t = self.inner.lock();
        t.next_id += 1;
        let id = format!("mon-{}", t.next_id);
        let buffer = Arc::new(Mutex::new(String::new()));
        let exit: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));

        // stdout + stderr reader threads append decoded text; the
        // waiter thread reaps the child and records the exit code
        let out_buf = buffer.clone();
        std::thread::spawn(move || pipe_into(stdout, out_buf));
        let err_buf = buffer.clone();
        std::thread::spawn(move || pipe_into(stderr, err_buf));
        {
            let exit = exit.clone();
            std::thread::spawn(move || {
                let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                *exit.lock() = Some(code);
            });
        }

        t.entries.push(MonitorEntry {
            id: id.clone(),
            cmd: cmd.to_string(),
            pid,
            buffer,
            exit,
            noted_bytes: 0,
            close_noted: false,
        });
        Ok((id, pid))
    }

    fn entry(&self, id: &str) -> Option<MonitorEntrySnapshot> {
        let t = self.inner.lock();
        t.entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| MonitorEntrySnapshot {
                buffer: e.buffer.clone(),
                exit: e.exit.clone(),
            })
    }

    /// New text past `offset` for monitor `id`, whole lines only,
    /// capped at [`READ_LINE_CAP`] lines / [`READ_BYTE_CAP`] bytes.
    pub fn read(&self, id: &str, offset: usize) -> Result<MonitorRead, String> {
        let Some(snap) = self.entry(id) else {
            return Err(format!("no such monitor: {id}"));
        };
        let buf = snap.buffer.lock();
        let Some(start) = safe_offset(&buf, offset) else {
            return Err(format!(
                "monitor {id}: offset {offset} is past the buffer ({} bytes); use offset 0",
                buf.len()
            ));
        };
        let rest = &buf[start.min(buf.len())..];
        // cap at whole lines, then bytes (re-cutting at the last newline)
        let mut end = rest
            .char_indices()
            .filter(|(_, c)| *c == '\n')
            .nth(READ_LINE_CAP.saturating_sub(1))
            .map(|(i, _)| i + 1)
            .unwrap_or(rest.len());
        if end > READ_BYTE_CAP {
            let cap = floor_char_boundary(rest, READ_BYTE_CAP);
            end = rest[..cap].rfind('\n').map(|i| i + 1).unwrap_or(cap);
        }
        let text = rest[..end].to_string();
        Ok(MonitorRead {
            next_offset: start + end,
            exit_code: *snap.exit.lock(),
            truncated: start + end < buf.len(),
            text,
        })
    }

    /// Kill a monitor's process tree. Err when there is nothing to
    /// kill (unknown id or already exited and reaped). Holds the table
    /// and exit locks across `kill_tree` to prevent PID-reuse races.
    pub fn kill(&self, id: &str) -> Result<String, String> {
        let t = self.inner.lock();
        let Some(e) = t.entries.iter().find(|e| e.id == id) else {
            return Err(format!("no such monitor: {id}"));
        };
        let exit = e.exit.lock();
        if let Some(code) = *exit {
            return Err(format!("monitor {id} already exited ({code})"));
        }
        #[cfg(unix)]
        super::bash::kill_tree(e.pid);
        Ok(format!("killing monitor {id} (`{}`)", e.cmd))
    }

    /// Settle-tick notes: one `monitor <id>: N new lines` per active
    /// entry with unread output, plus a one-time close note when the
    /// process has exited. Returns the rendered notes; the caller
    /// surfaces them.
    pub fn drain_notes(&self) -> Vec<String> {
        let mut t = self.inner.lock();
        let mut notes = Vec::new();
        for e in &mut t.entries {
            let buf = e.buffer.lock();
            let new_bytes = buf.len().saturating_sub(e.noted_bytes);
            if new_bytes > 0 {
                e.noted_bytes = buf.len();
                let new_lines = buf[e.noted_bytes - new_bytes..].lines().count();
                notes.push(format!("monitor {}: {new_lines} new lines", e.id));
            }
            if !e.close_noted {
                if let Some(code) = *e.exit.lock() {
                    e.close_noted = true;
                    notes.push(format!("monitor {}: exited ({code})", e.id));
                }
            }
        }
        notes
    }

    /// Snapshot for `list`-style rendering.
    pub fn snapshot(&self) -> Vec<(String, String, Option<i32>)> {
        let t = self.inner.lock();
        t.entries
            .iter()
            .map(|e| (e.id.clone(), e.cmd.clone(), *e.exit.lock()))
            .collect()
    }
}

impl Drop for MonitorTable {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let t = self.inner.lock();
            for e in &t.entries {
                if e.exit.lock().is_none() {
                    super::bash::kill_tree(e.pid);
                }
            }
        }
    }
}

/// Largest valid UTF-8 char boundary `<= max`.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    let mut i = max.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Cheap cloneable view of one entry (the table lock stays with the
/// table; these are the shared Arcs the readers touch).
struct MonitorEntrySnapshot {
    buffer: Arc<Mutex<String>>,
    exit: Arc<Mutex<Option<i32>>>,
}

/// Resolve a caller offset to a char-boundary start: `None` when it is
/// past the buffer (the buffer never truncates, so the offset is
/// simply stale/wrong). Zero always reads from the start.
fn safe_offset(buf: &str, offset: usize) -> Option<usize> {
    if offset == 0 {
        return Some(0);
    }
    if offset > buf.len() {
        return None;
    }
    // land on a char boundary even if the caller resumed mid-char
    let mut start = offset;
    while start < buf.len() && !buf.is_char_boundary(start) {
        start += 1;
    }
    Some(start)
}

/// Read a pipe to EOF, appending decoded text to the shared buffer up
/// to [`MAX_BUFFER_BYTES`]; excess output is drained and discarded so
/// chatty children neither block nor OOM ka.
fn pipe_into(mut pipe: impl Read, buffer: Arc<Mutex<String>>) {
    let mut chunk = [0u8; 4096];
    let mut capped = false;
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if capped {
                    continue;
                }
                let text = String::from_utf8_lossy(&chunk[..n]);
                let mut buf = buffer.lock();
                if buf.len() >= MAX_BUFFER_BYTES {
                    // exact-fill chunks land here on the next read: record
                    // the drop instead of going silent
                    buf.push_str("\n[...monitor buffer cap reached; further output dropped]\n");
                    capped = true;
                    continue;
                }
                let room = MAX_BUFFER_BYTES - buf.len();
                if text.len() <= room {
                    buf.push_str(&text);
                } else {
                    let cut = floor_char_boundary(&text, room);
                    buf.push_str(&text[..cut]);
                    buf.push_str("\n[...monitor buffer cap reached; further output dropped]\n");
                    capped = true;
                }
            }
        }
    }
}

/// The monitor tool: spawn / read / kill in one hand, jobs-style.
pub struct MonitorHand {
    table: Arc<MonitorTable>,
}

impl MonitorHand {
    /// Hand over the shared table (must be the voice's table so the
    /// settle tick notes what this hand spawns).
    pub fn new(table: Arc<MonitorTable>) -> Self {
        Self { table }
    }
}

impl Hand for MonitorHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "monitor".to_string(),
            description: "Run a command in the background and watch its combined output \
                stream. Arguments: {\"action\": \"spawn\" | \"read\" | \"kill\", \
                \"command\": \"tail -f x.log\", \"id\": \"mon-1\", \"offset\": 0}. spawn needs \
                command and returns the monitor id + pid; read needs id (offset = byte offset \
                from the previous read, defaults to 0) and returns new lines since it plus the \
                next offset; kill needs id. New output and process exits are also reported as \
                notes between turns, so you never have to poll blind. Output lives in memory \
                for this session."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["spawn", "read", "kill"],
                        "description": "spawn (default), read or kill"
                    },
                    "command": {
                        "type": "string",
                        "description": "Command line for sh -c (spawn)"
                    },
                    "id": {
                        "type": "string",
                        "description": "Monitor id (read, kill)"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Byte offset from the previous read (read, default 0)"
                    }
                }
            }),
            clearance: Clearance::Exec,
            read_only: false,
        }
    }

    /// Reading a stream is read-only; spawning or killing a process is
    /// arbitrary execution and gates at exec tier.
    fn clearance_for(&self, args: &Value) -> Clearance {
        if args.get("action").and_then(Value::as_str) == Some("read") {
            Clearance::Read
        } else {
            Clearance::Exec
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            match args
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("spawn")
            {
                "spawn" => {
                    if crate::conventions::bare_mode() {
                        return ToolOutput::err("monitor is unavailable in safe mode");
                    }
                    let Some(cmd) = args.get("command").and_then(Value::as_str) else {
                        return ToolOutput::err("monitor spawn: missing required 'command'");
                    };
                    match self
                        .table
                        .spawn_in(cmd, Some(&ctx.cwd), &ctx.shell_env, &ctx.sandbox)
                    {
                        Ok((id, pid)) => ToolOutput::ok(format!(
                            "monitor {id} started (pid {pid}): `{cmd}` — read it with \
                             monitor {{\"action\": \"read\", \"id\": \"{id}\"}}"
                        )),
                        Err(e) => ToolOutput::err(e),
                    }
                }
                "read" => {
                    let Some(id) = args.get("id").and_then(Value::as_str) else {
                        return ToolOutput::err("monitor read: missing required 'id'");
                    };
                    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
                    match self.table.read(id, offset) {
                        Ok(r) => {
                            let mut out = r.text;
                            if r.truncated {
                                out.push_str(&format!(
                                    "\n… more output waiting — read again with offset {}",
                                    r.next_offset
                                ));
                            }
                            if let Some(code) = r.exit_code {
                                out.push_str(&format!("\n(process exited, code {code})"));
                            }
                            if out.is_empty() {
                                out = format!(
                                    "no new output since offset {offset} (next offset {})",
                                    r.next_offset
                                );
                            }
                            ToolOutput::ok(out)
                        }
                        Err(e) => ToolOutput::err(e),
                    }
                }
                "kill" => {
                    let Some(id) = args.get("id").and_then(Value::as_str) else {
                        return ToolOutput::err("monitor kill: missing required 'id'");
                    };
                    match self.table.kill(id) {
                        Ok(msg) => ToolOutput::ok(msg),
                        Err(e) => ToolOutput::err(e),
                    }
                }
                other => ToolOutput::err(format!(
                    "monitor: unknown action {other:?} (expected \"spawn\", \"read\" or \"kill\")"
                )),
            }
        })
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
            shell_env: Default::default(),
        }
    }

    /// Wait until the predicate holds (reader threads are async to the
    /// test) or fail after ~2s.
    fn wait_for(mut pred: impl FnMut() -> bool) {
        for _ in 0..200 {
            if pred() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("condition never held");
    }

    #[tokio::test]
    async fn spawn_captures_output_and_exit() {
        let table = Arc::new(MonitorTable::new());
        let hand = MonitorHand::new(table.clone());
        let out = hand
            .execute(
                &json!({"command": "echo one; echo two >&2; echo three"}),
                &ctx(),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let id = out
            .content
            .split(' ')
            .nth(1)
            .unwrap_or_default()
            .to_string();
        wait_for(|| {
            table
                .read(&id, 0)
                .map(|r| r.exit_code.is_some())
                .unwrap_or(false)
        });
        let r = table.read(&id, 0).unwrap();
        assert!(r.text.contains("one") && r.text.contains("two") && r.text.contains("three"));
        assert_eq!(r.exit_code, Some(0));
        assert!(!r.truncated);
    }

    #[tokio::test]
    async fn read_offset_increments() {
        let table = Arc::new(MonitorTable::new());
        let hand = MonitorHand::new(table.clone());
        let out = hand
            .execute(&json!({"command": "printf 'l1\\nl2\\nl3\\n'"}), &ctx())
            .await;
        let id = out
            .content
            .split(' ')
            .nth(1)
            .unwrap_or_default()
            .to_string();
        wait_for(|| {
            table
                .snapshot()
                .iter()
                .any(|(i, _, e)| *i == id && e.is_some())
        });
        // first read: capped at 2 lines (READ_LINE_CAP is 200 — here the
        // whole 3-line buffer fits, so slice manually for the offset
        // mechanics: read twice with explicit offsets instead)
        let first = table.read(&id, 0).unwrap();
        assert!(first.text.contains("l1\nl2\nl3"));
        assert!(!first.truncated);
        assert_eq!(first.next_offset, first.text.len());
        // resuming at the end yields nothing new
        let again = table.read(&id, first.next_offset).unwrap();
        assert_eq!(again.text.len(), 0);
        // a mid-buffer offset resumes at whole lines: offset 6 = after
        // "l1\nl2\n" → the third line
        let half = table.read(&id, 6).unwrap();
        assert!(half.text.starts_with("l3"), "{:?}", half.text);
    }

    #[tokio::test]
    async fn read_caps_lines_and_bytes() {
        let table = Arc::new(MonitorTable::new());
        table
            .spawn_with_env("yes a | head -300", &Default::default())
            .unwrap();
        let id = table.snapshot()[0].0.clone();
        wait_for(|| {
            let snap = table.snapshot();
            snap.iter().any(|(i, _, e)| *i == id && e.is_some())
        });
        let r = table.read(&id, 0).unwrap();
        assert_eq!(r.text.lines().count(), READ_LINE_CAP, "{}", r.text.len());
        assert!(r.truncated);
        // byte cap bites with one huge line
        let (id2, _) = table
            .spawn_with_env(
                "head -c 40000 /dev/zero | tr '\\0' 'a'",
                &Default::default(),
            )
            .unwrap();
        wait_for(|| {
            table
                .read(&id2, 0)
                .map(|r| r.exit_code.is_some())
                .unwrap_or(false)
        });
        let r2 = table.read(&id2, 0).unwrap();
        assert!(r2.text.len() <= READ_BYTE_CAP + 16, "{}", r2.text.len());
        assert!(r2.truncated);
    }

    /// The [shell] env policy applies to monitor spawns: env_set rides
    /// through, a stripped mode hides parent vars (bash hand parity).
    #[tokio::test]
    async fn spawn_applies_the_shell_env_policy() {
        let table = Arc::new(MonitorTable::new());
        let policy = crate::config::ShellEnvPolicy {
            mode: crate::config::ShellEnvMode::None,
            keep: Vec::new(),
            set: [("KA_PERM_PROBE".to_string(), "mon-set".to_string())]
                .into_iter()
                .collect(),
        };
        table
            .spawn_with_env("echo probe=[$KA_PERM_PROBE] home=[$HOME]", &policy)
            .unwrap();
        let id = table.snapshot()[0].0.clone();
        wait_for(|| {
            table
                .read(&id, 0)
                .map(|r| r.exit_code.is_some())
                .unwrap_or(false)
        });
        let r = table.read(&id, 0).unwrap();
        assert!(
            r.text.contains("probe=[mon-set]"),
            "env_set injects: {}",
            r.text
        );
        assert!(
            r.text.contains("home=[]"),
            "none strips the parent env: {}",
            r.text
        );
    }

    #[tokio::test]
    async fn kill_stops_the_process() {
        let table = Arc::new(MonitorTable::new());
        let hand = MonitorHand::new(table.clone());
        let out = hand.execute(&json!({"command": "sleep 30"}), &ctx()).await;
        let id = out
            .content
            .split(' ')
            .nth(1)
            .unwrap_or_default()
            .to_string();
        let killed = hand
            .execute(&json!({"action": "kill", "id": id}), &ctx())
            .await;
        assert!(!killed.is_error, "{}", killed.content);
        wait_for(|| {
            table
                .read(&id, 0)
                .map(|r| r.exit_code.is_some())
                .unwrap_or(false)
        });
        // killing a dead monitor says so
        let again = hand
            .execute(&json!({"action": "kill", "id": id}), &ctx())
            .await;
        assert!(again.is_error);
    }

    #[tokio::test]
    async fn drain_notes_report_new_lines_and_exit_once() {
        let table = Arc::new(MonitorTable::new());
        table
            .spawn_with_env("printf 'x\\n'", &Default::default())
            .unwrap();
        let id = table.snapshot()[0].0.clone();
        wait_for(|| {
            table
                .read(&id, 0)
                .map(|r| r.exit_code.is_some())
                .unwrap_or(false)
        });
        let notes = table.drain_notes();
        assert!(
            notes
                .iter()
                .any(|n| n.contains(&format!("monitor {id}: 1 new lines"))),
            "{notes:?}"
        );
        assert!(notes.iter().any(|n| n.contains("exited (0)")), "{notes:?}");
        // a second drain repeats nothing: new-line bookkeeping and the
        // close note are once-per-settle / once-ever
        let notes2: Vec<String> = table.drain_notes();
        assert!(
            !notes2.iter().any(|n| n.contains("new lines")),
            "{notes2:?}"
        );
        assert!(!notes2.iter().any(|n| n.contains("exited")), "{notes2:?}");
    }

    #[tokio::test]
    async fn unknown_ids_error() {
        let table = Arc::new(MonitorTable::new());
        let hand = MonitorHand::new(table);
        assert!(
            hand.execute(&json!({"action": "read", "id": "mon-9"}), &ctx())
                .await
                .is_error
        );
        assert!(
            hand.execute(&json!({"action": "kill", "id": "mon-9"}), &ctx())
                .await
                .is_error
        );
        assert!(
            hand.execute(&json!({"action": "spawn"}), &ctx())
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn read_gates_read_and_spawn_kill_gate_exec() {
        let hand = MonitorHand::new(Arc::new(MonitorTable::new()));
        assert_eq!(
            hand.clearance_for(&json!({"action": "read", "id": "m"})),
            Clearance::Read
        );
        assert_eq!(
            hand.clearance_for(&json!({"command": "ls"})),
            Clearance::Exec
        );
        assert_eq!(
            hand.clearance_for(&json!({"action": "kill", "id": "m"})),
            Clearance::Exec
        );
    }
}
