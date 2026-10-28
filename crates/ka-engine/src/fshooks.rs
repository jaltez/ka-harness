//! File-convention hooks: executable shell scripts under `.ka/hooks/`.
//!
//! Three hook points, each addressed by file name (`.sh` optional):
//! `pre-turn`, `post-turn`, and `pre-tool`. Scripts run with the session
//! working directory as cwd and `KA_HOOK=<name>` in the environment
//! (`KA_TOOL=<tool>` additionally for `pre-tool`). A non-zero exit is
//! advisory for turns (surfaced as a note with the stderr tail) and a
//! veto for `pre-tool` (the tool call is refused). Everything is
//! best-effort: missing hooks are a no-op, each hook gets 10 seconds,
//! and failures never panic the engine.
//!
//! This complements the ka.toml `[[hooks]]` entries (config-file hooks,
//! see [`crate::config::Hook`]); both may coexist.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The three conventional hook points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPoint {
    /// Before a turn starts.
    PreTurn,
    /// After a turn finished.
    PostTurn,
    /// Before an approved tool call executes (non-zero = veto).
    PreTool,
}

impl HookPoint {
    /// The hook file stem (and `KA_HOOK` value).
    pub fn name(self) -> &'static str {
        match self {
            HookPoint::PreTurn => "pre-turn",
            HookPoint::PostTurn => "post-turn",
            HookPoint::PreTool => "pre-tool",
        }
    }
}

const HOOK_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap on the stderr tail carried in veto/note messages.
const TAIL_CHARS: usize = 400;
/// Cap on steering note length (chars). Also caps hook-controlled key
/// lists surfaced in transcript notes elsewhere in the engine.
pub(crate) const NOTE_CHARS: usize = 200;
/// Cap on post-tool feedback length (chars) — tool-result-sized, not
/// note-sized: a linter's findings must fit.
const FEEDBACK_CHARS: usize = 2000;

/// Minimal hook-stdout steering: a JSON object may switch the
/// permission mode, surface a short note, patch the upcoming tool
/// arguments (`updated_input`, shallow-merged top-level keys), or
/// pre-approve the call (`decision: "allow"` — matched
/// case-insensitively). A pre-approve may only skip a permission ASK:
/// hardstops, protected paths, and deny-class gate verdicts (deny
/// rules, plan-mode writes outside .ka/plans/) still apply. A
/// `post_tool_use` hook may additionally return `feedback`: the string
/// is appended to that call's tool result for the model to read
/// (formatters, linters, policy echoes) — pre_tool_use feedback has no
/// result to ride and is ignored. Anything else — invalid JSON,
/// unknown keys, non-objects — is ignored silently; hook output never
/// fails a turn. This is deliberately the whole action language (no
/// rule mutation, no directory adds).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Steering {
    pub mode: Option<ka_protocol::Mode>,
    pub note: Option<String>,
    /// Tool-argument patch for the call being admitted (pre_tool_use
    /// only): top-level keys shallow-merge over the call's arguments.
    pub updated_input: Option<serde_json::Map<String, serde_json::Value>>,
    /// Hook verdict: `decision: "allow"` (any ASCII case) pre-approves
    /// the call — the permission ask is skipped; hardstops, protected
    /// paths, and deny-class verdicts still refuse.
    pub allow: bool,
    /// Post-tool feedback (post_tool_use only): appended to the tool
    /// result the model reads, capped at [`FEEDBACK_CHARS`] chars.
    pub feedback: Option<String>,
}

/// Parse steering off a successful hook's trimmed stdout.
pub(crate) fn parse_steering(stdout: &str) -> Steering {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
        return Steering::default();
    };
    if !v.is_object() {
        return Steering::default();
    }
    let mode = v
        .get("mode")
        .and_then(|m| m.as_str())
        .and_then(|s| match s {
            "ask" | "guarded" => Some(ka_protocol::Mode::Ask),
            "accept_edits" | "accept-edits" => Some(ka_protocol::Mode::AcceptEdits),
            "allow" | "free" => Some(ka_protocol::Mode::Allow),
            "readonly" | "plan" => Some(ka_protocol::Mode::Readonly),
            _ => None,
        });
    let note = v
        .get("note")
        .and_then(|n| n.as_str())
        .map(|s| s.chars().take(NOTE_CHARS).collect::<String>())
        .filter(|s| !s.is_empty());
    let updated_input = v
        .get("updated_input")
        .and_then(|u| u.as_object())
        .cloned()
        .filter(|m| !m.is_empty());
    // case-insensitive by design: hooks are shell scripts, and "ALLOW"
    // carries the same intent as "allow"
    let allow = v
        .get("decision")
        .and_then(|d| d.as_str())
        .is_some_and(|d| d.eq_ignore_ascii_case("allow"));
    let feedback = v
        .get("feedback")
        .and_then(|f| f.as_str())
        .map(|s| s.chars().take(FEEDBACK_CHARS).collect::<String>())
        .filter(|s| !s.is_empty());
    Steering {
        mode,
        note,
        updated_input,
        allow,
        feedback,
    }
}

impl Steering {
    /// `None` when nothing parsed (callers skip empty steering).
    pub(crate) fn into_nonempty(self) -> Option<Steering> {
        (self != Steering::default()).then_some(self)
    }
}

/// Run the convention hook for `event` if one exists. `Ok(None)` means
/// proceed without steering (hook missing, or exited zero with no
/// parseable steering stdout); `Ok(Some(..))` carries stdout steering;
/// `Err(reason)` means the hook failed or timed out, with the stderr
/// tail as the reason.
///
/// Convention hooks are project-scope `.ka/` content: they only run in
/// a trusted project (see [`crate::trust`]). An untrusted project's
/// hook is silently skipped.
pub async fn run(
    event: HookPoint,
    cwd: &Path,
    tool: Option<&str>,
) -> Result<Option<Steering>, String> {
    if crate::conventions::bare_mode() {
        return Ok(None);
    }
    run_trusted(event, cwd, tool, crate::trust::project_trusted(cwd)).await
}

/// [`run`] with an explicit trust decision (tests).
pub async fn run_trusted(
    event: HookPoint,
    cwd: &Path,
    tool: Option<&str>,
    project_trusted: bool,
) -> Result<Option<Steering>, String> {
    if !project_trusted {
        return Ok(None);
    }
    let Some(script) = locate(event, cwd) else {
        return Ok(None);
    };
    let name = event.name();
    let tool = tool.map(str::to_string);
    let attempt = {
        let script = script.clone();
        let cwd = cwd.to_path_buf();
        tokio::task::spawn_blocking(move || execute(&script, &cwd, name, tool.as_deref()))
    };
    // belt-and-braces around the blocking pool join; execute() enforces
    // the real 10s budget itself and kills the child on overrun
    match tokio::time::timeout(HOOK_TIMEOUT + Duration::from_secs(5), attempt).await {
        Ok(Ok(result)) => result,
        Ok(Err(join)) => Err(format!("hook task failed: {join}")),
        Err(_) => Err(format!("{name} hook did not finish in 10s")),
    }
}

/// Find the hook script: `<project root>/.ka/hooks/<name>.sh` first,
/// then `<name>` — root-anchored so a session launched from a
/// subdirectory runs the same project hooks.
fn locate(event: HookPoint, cwd: &Path) -> Option<PathBuf> {
    let base = crate::project_root(cwd).join(".ka").join("hooks");
    [format!("{}.sh", event.name()), event.name().to_string()]
        .into_iter()
        .map(|name| base.join(name))
        .find(|path| path.is_file())
}

/// Execute one hook script synchronously (runs on the blocking pool).
/// `Ok(Some(..))` = exited zero with parseable steering stdout.
fn execute(
    script: &Path,
    cwd: &Path,
    name: &str,
    tool: Option<&str>,
) -> Result<Option<Steering>, String> {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg(script)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("KA_HOOK", name);
    if let Some(tool) = tool {
        cmd.env("KA_TOOL", tool);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return Err(format!("{name} hook failed to spawn: {e}")),
    };
    // drain both pipes off-thread so a chatty hook cannot fill the
    // pipe buffer and deadlock against our wait loop
    let stdout = read_pipe(child.stdout.take());
    let stderr = read_pipe(child.stderr.take());
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= HOOK_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{name} hook timed out after 10s"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("{name} hook wait failed: {e}")),
        }
    };
    if status.success() {
        let out = stdout.join().unwrap_or_default();
        return Ok(parse_steering(&String::from_utf8_lossy(&out)).into_nonempty());
    }
    let err = stderr.join().unwrap_or_default();
    let code = status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_string());
    Err(format!("{name} hook exited {code}: {}", tail(&err)))
}

/// Read one pipe to end on a plain thread → bytes, capped at
/// [`STEERING_BUDGET`]. Steering payloads are tiny JSON objects; the
/// budget exists so a chatty hook cannot grow unbounded memory while
/// the wait loop holds off the 10s timeout.
const STEERING_BUDGET: usize = 64 * 1024;

fn read_pipe<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; STEERING_BUDGET];
        let mut used = 0;
        if let Some(mut pipe) = pipe {
            while used < STEERING_BUDGET {
                match pipe.read(&mut buf[used..]) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => used += n,
                }
            }
            // drain the rest so the child never blocks on a full pipe
            let mut sink = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut sink) {
                if n == 0 {
                    break;
                }
            }
        }
        buf.truncate(used);
        buf
    })
}

/// The last `TAIL_CHARS` characters of a stream, trimmed.
fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim_end();
    let tail: String = trimmed.chars().rev().take(TAIL_CHARS).collect();
    tail.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn steering_parses_mode_and_note() {
        let s = parse_steering(r#"  {"mode":"plan","note":"switching"}  "#);
        assert_eq!(s.mode, Some(ka_protocol::Mode::Readonly));
        assert_eq!(s.note.as_deref(), Some("switching"));
        // both vocabularies parse: the new words first, older spellings kept
        for (word, want) in [
            ("ask", ka_protocol::Mode::Ask),
            ("guarded", ka_protocol::Mode::Ask),
            ("accept_edits", ka_protocol::Mode::AcceptEdits),
            ("accept-edits", ka_protocol::Mode::AcceptEdits),
            ("allow", ka_protocol::Mode::Allow),
            ("free", ka_protocol::Mode::Allow),
            ("readonly", ka_protocol::Mode::Readonly),
            ("plan", ka_protocol::Mode::Readonly),
        ] {
            assert_eq!(
                parse_steering(&format!(r#"{{"mode":"{word}"}}"#)).mode,
                Some(want),
                "{word}"
            );
        }
        assert_eq!(parse_steering(r#"{"mode":"yolo"}"#).mode, None);
        assert_eq!(parse_steering("not json"), Steering::default());
        assert_eq!(parse_steering(r#"["array"]"#), Steering::default());
        assert_eq!(parse_steering(r#"{"unknown":1}"#), Steering::default());
        // note capped at 200 chars
        let long = "x".repeat(500);
        let s = parse_steering(&format!(r#"{{"note":"{long}"}}"#));
        assert_eq!(s.note.as_deref().map(str::len), Some(200));
    }

    #[test]
    fn steering_parses_feedback_capped() {
        let s = parse_steering(r#"{"feedback":"rustfmt: 3 diffs"}"#);
        assert_eq!(s.feedback.as_deref(), Some("rustfmt: 3 diffs"));
        // non-strings and empties are ignored
        assert!(parse_steering(r#"{"feedback":7}"#).feedback.is_none());
        assert!(parse_steering(r#"{"feedback":""}"#).feedback.is_none());
        assert!(parse_steering(r#"{"feedback":null}"#).feedback.is_none());
        // capped at FEEDBACK_CHARS chars
        let long = "y".repeat(FEEDBACK_CHARS + 50);
        let s = parse_steering(&format!(r#"{{"feedback":"{long}"}}"#));
        assert_eq!(s.feedback.as_deref().map(str::len), Some(FEEDBACK_CHARS));
    }

    #[test]
    fn steering_parses_updated_input_and_allow() {
        // updated_input: object, shallow-merged top-level keys
        let s = parse_steering(r#"{"updated_input":{"command":"echo patched"}}"#);
        assert_eq!(
            s.updated_input
                .as_ref()
                .and_then(|m| m.get("command"))
                .and_then(|v| v.as_str()),
            Some("echo patched")
        );
        // non-object / empty-object patches are ignored
        assert!(
            parse_steering(r#"{"updated_input":"echo x"}"#)
                .updated_input
                .is_none()
        );
        assert!(
            parse_steering(r#"{"updated_input":{}}"#.trim())
                .updated_input
                .is_none()
        );
        // decision: "allow" pre-approves; anything else does not
        assert!(parse_steering(r#"{"decision":"allow"}"#).allow);
        assert!(parse_steering(r#"{"decision":"ALLOW"}"#).allow);
        assert!(!parse_steering(r#"{"decision":"deny"}"#).allow);
        let both = parse_steering(r#"{"decision":"allow","updated_input":{}}"#);
        assert!(both.allow);
        assert!(both.updated_input.is_none(), "empty patch ignored");
        // the combined form parses both
        let s = parse_steering(
            r#"{"decision":"allow","updated_input":{"command":"echo ok"},"note":"auto"}"#,
        );
        assert!(s.allow);
        assert!(s.updated_input.is_some());
        assert_eq!(s.note.as_deref(), Some("auto"));
    }

    fn hook_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-fshooks-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ka/hooks")).unwrap();
        dir
    }

    #[tokio::test]
    async fn missing_hooks_are_a_noop() {
        let dir = hook_dir("missing");
        assert!(
            run_trusted(HookPoint::PreTurn, &dir, None, true)
                .await
                .is_ok()
        );
        assert!(
            run_trusted(HookPoint::PreTool, &dir, Some("bash"), true)
                .await
                .is_ok()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn failing_pre_turn_reports_stderr_tail() {
        let dir = hook_dir("pre-turn-fail");
        std::fs::write(dir.join(".ka/hooks/pre-turn.sh"), "echo boom >&2; exit 3\n").unwrap();
        let err = run_trusted(HookPoint::PreTurn, &dir, None, true)
            .await
            .unwrap_err();
        assert!(err.contains("exited 3"), "{err}");
        assert!(err.contains("boom"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn successful_hook_and_bare_name_resolution() {
        let dir = hook_dir("ok");
        std::fs::write(dir.join(".ka/hooks/post-turn"), "exit 0\n").unwrap();
        assert!(
            run_trusted(HookPoint::PostTurn, &dir, None, true)
                .await
                .is_ok()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pre_tool_veto_carries_tool_env() {
        let dir = hook_dir("pre-tool");
        std::fs::write(
            dir.join(".ka/hooks/pre-tool.sh"),
            "test \"$KA_TOOL\" = bash -a \"$KA_HOOK\" = pre-tool || exit 1\nexit 5\n",
        )
        .unwrap();
        let err = run_trusted(HookPoint::PreTool, &dir, Some("bash"), true)
            .await
            .unwrap_err();
        assert!(err.contains("exited 5"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hanging_hook_times_out() {
        let dir = hook_dir("hang");
        std::fs::write(dir.join(".ka/hooks/pre-turn.sh"), "sleep 30\n").unwrap();
        let err = run_trusted(HookPoint::PreTurn, &dir, None, true)
            .await
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn untrusted_project_hook_is_skipped() {
        let dir = hook_dir("untrusted");
        std::fs::write(dir.join(".ka/hooks/pre-turn.sh"), "echo boom >&2; exit 3\n").unwrap();
        assert!(
            run_trusted(HookPoint::PreTurn, &dir, None, false)
                .await
                .is_ok(),
            "untrusted project hook must not run"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn trust_store_approval_unlocks_hooks() {
        let (_file, _guard) = crate::trust::test_support::trust_guard();
        let dir = hook_dir("store-trusted");
        std::fs::write(dir.join(".ka/hooks/pre-turn.sh"), "exit 3\n").unwrap();
        // still untrusted: the hook is skipped even though it exists
        assert!(run(HookPoint::PreTurn, &dir, None).await.is_ok());
        // approve through the shared trust path (writes the store file)
        crate::trust::approve(&dir);
        let err = run(HookPoint::PreTurn, &dir, None).await.unwrap_err();
        assert!(err.contains("exited 3"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_caps_long_output() {
        let long = "x".repeat(2_000);
        assert_eq!(tail(long.as_bytes()).chars().count(), TAIL_CHARS);
    }
}
