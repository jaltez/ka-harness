//! Opt-in Herdr integration ([tui] herdr = true): reports this agent's
//! state, resume command, and exit to the Herdr pane that hosts the
//! process.
//!
//! Off by default and inert unless `HERDR_ENV=1` with the pane
//! variables set, so running ka anywhere else spawns nothing. Reports
//! are fire-and-forget in a background thread: the newest state
//! replaces any unconsumed one, the child gets a short deadline and is
//! killed on timeout, and every failure is ignored — an integration
//! must never slow the agent down.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ka_term::tui::{ActivityReport, ActivitySink, AgentActivity, AgentReport};

/// How long one Herdr report may run before it is killed.
const REPORT_TIMEOUT: Duration = Duration::from_millis(300);
/// The `--source`/`--agent` name Herdr shows and de-duplicates on.
const NAME: &str = "ka";
/// Resume-command limits from the Herdr docs.
const MAX_ARGS: usize = 64;
const MAX_BYTES: usize = 8 * 1024;

/// Where to report: the Herdr binary and the pane hosting us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// `HERDR_BIN_PATH`.
    pub bin: PathBuf,
    /// `HERDR_PANE_ID`.
    pub pane: String,
}

/// Detect the pane from the environment when the integration is enabled.
/// `enabled` is the `[tui] herdr` opt-in; everything else follows the
/// Herdr contract (all variables present, `HERDR_ENV=1`).
pub fn detect(enabled: bool) -> Option<Endpoint> {
    detect_from(enabled, |key| std::env::var(key).ok())
}

/// [`detect`] against an injectable environment (tests).
fn detect_from(enabled: bool, get: impl Fn(&str) -> Option<String>) -> Option<Endpoint> {
    if !enabled || get("HERDR_ENV").as_deref() != Some("1") {
        return None;
    }
    let pane = get("HERDR_PANE_ID").filter(|p| !p.is_empty())?;
    let bin = get("HERDR_BIN_PATH").filter(|b| !b.is_empty())?;
    Some(Endpoint {
        bin: PathBuf::from(bin),
        pane,
    })
}

/// One argument Herdr accepts in a resume command: visible ASCII, no
/// apostrophes, no control characters (the docs' argv contract).
fn safe_arg(arg: &str) -> bool {
    !arg.is_empty() && !arg.contains('\'') && !arg.chars().any(char::is_control)
}

/// The resume argv Herdr will run after a server restart:
/// `ka --session <id>` plus every safe startup group. The session id is
/// the resume handle, so an unusable one yields no command at all —
/// state reports keep working.
pub fn resume_argv(session: &str, startup: &[Vec<String>]) -> Vec<String> {
    if !safe_arg(session) {
        return Vec::new();
    }
    let mut argv = vec![
        "ka".to_string(),
        "--session".to_string(),
        session.to_string(),
    ];
    let mut bytes = argv.iter().map(|a| a.len() + 1).sum::<usize>();
    for group in startup {
        if group.is_empty() || !group.iter().all(|a| safe_arg(a)) {
            continue;
        }
        let extra = group.iter().map(|a| a.len() + 1).sum::<usize>();
        if argv.len() + group.len() > MAX_ARGS || bytes + extra > MAX_BYTES {
            continue;
        }
        argv.extend(group.iter().cloned());
        bytes += extra;
    }
    argv
}

/// One `pane report-agent` invocation. The resume command rides every
/// state report that has a session, which keeps the pane restorable
/// without a second call.
fn report_args(
    endpoint: &Endpoint,
    activity: AgentActivity,
    seq: u64,
    session: &str,
    argv: &[String],
) -> Vec<String> {
    let (state, message) = match activity {
        AgentActivity::Idle => ("idle", None),
        AgentActivity::Working => ("working", None),
        AgentActivity::Blocked => ("blocked", Some("waiting for approval")),
    };
    let mut args = vec![
        "pane".to_string(),
        "report-agent".to_string(),
        endpoint.pane.clone(),
        "--source".to_string(),
        NAME.to_string(),
        "--agent".to_string(),
        NAME.to_string(),
        "--state".to_string(),
        state.to_string(),
        "--seq".to_string(),
        seq.to_string(),
    ];
    if let Some(m) = message {
        args.push("--message".to_string());
        args.push(m.to_string());
    }
    if !argv.is_empty() && !session.is_empty() {
        args.push("--agent-session-id".to_string());
        args.push(session.to_string());
        args.push("--".to_string());
        args.extend(argv.iter().cloned());
    }
    args
}

/// One `pane release-agent` invocation.
fn release_args(endpoint: &Endpoint, seq: u64) -> Vec<String> {
    vec![
        "pane".to_string(),
        "release-agent".to_string(),
        endpoint.pane.clone(),
        "--source".to_string(),
        NAME.to_string(),
        "--agent".to_string(),
        NAME.to_string(),
        "--seq".to_string(),
        seq.to_string(),
    ]
}

/// Strictly increasing report sequence: wall-clock nanoseconds, but
/// never behind the last value (late clocks and fast transitions).
fn next_seq(last: &mut u64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0);
    *last = (*last).max(now).saturating_add(1);
    *last
}

/// Run one report and ignore every outcome; kill it at the deadline.
fn run(bin: &std::path::Path, args: &[String]) {
    let spawned = std::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = spawned else {
        return;
    };
    let deadline = Instant::now() + REPORT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => return,
        }
    }
}

/// The background reporter: owns the consumer thread, hands the TUI its
/// sink, and releases the pane when the agent quits.
pub struct Reporter {
    sink: Arc<ActivitySink>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Reporter {
    /// Start reporting to `endpoint`. `startup` carries the session
    /// shaping flags (see `startup_args` in main) for resume commands.
    pub fn start(endpoint: Endpoint, startup: Vec<Vec<String>>) -> Self {
        let sink = Arc::new(ActivitySink::new());
        let consumer = Arc::clone(&sink);
        let join = std::thread::Builder::new()
            .name("ka-herdr".into())
            .spawn(move || {
                let mut seq = 0u64;
                while let Some(report) = consumer.wait() {
                    match report {
                        AgentReport::Report(ActivityReport { activity, session }) => {
                            let argv = resume_argv(&session, &startup);
                            let args = report_args(
                                &endpoint,
                                activity,
                                next_seq(&mut seq),
                                &session,
                                &argv,
                            );
                            run(&endpoint.bin, &args);
                        }
                        AgentReport::Release => {
                            run(&endpoint.bin, &release_args(&endpoint, next_seq(&mut seq)));
                            break;
                        }
                        AgentReport::Stop => break,
                    }
                }
            })
            .ok();
        Self { sink, join }
    }

    /// The sink the TUI publishes activity to.
    pub fn sink(&self) -> Arc<ActivitySink> {
        Arc::clone(&self.sink)
    }

    /// Release the pane and stop the reporter (bounded by one report
    /// deadline).
    pub fn finish(&mut self) {
        self.sink.publish(AgentReport::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Detect and start when opted in; `None` outside Herdr.
pub fn init(enabled: bool, startup: Vec<Vec<String>>) -> Option<Reporter> {
    detect(enabled).map(|endpoint| Reporter::start(endpoint, startup))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn endpoint() -> Endpoint {
        Endpoint {
            bin: PathBuf::from("/bin/true"),
            pane: "pane-7".into(),
        }
    }

    #[test]
    fn detection_requires_the_opt_in_and_the_full_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| (*v).to_string())
            }
        };
        let full = env(&[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "pane-7"),
            ("HERDR_BIN_PATH", "/usr/bin/herdr"),
        ]);
        assert_eq!(
            detect_from(true, full),
            Some(Endpoint {
                bin: PathBuf::from("/usr/bin/herdr"),
                pane: "pane-7".into()
            })
        );
        // config opt-out wins even inside Herdr
        assert!(
            detect_from(
                false,
                env(&[
                    ("HERDR_ENV", "1"),
                    ("HERDR_PANE_ID", "pane-7"),
                    ("HERDR_BIN_PATH", "/usr/bin/herdr"),
                ])
            )
            .is_none()
        );
        // outside Herdr, or with any piece missing: inert
        assert!(detect_from(true, env(&[("HERDR_ENV", "0")])).is_none());
        assert!(
            detect_from(
                true,
                env(&[("HERDR_PANE_ID", "p"), ("HERDR_BIN_PATH", "/b")])
            )
            .is_none()
        );
        assert!(detect_from(true, env(&[("HERDR_ENV", "1"), ("HERDR_BIN_PATH", "/b")])).is_none());
        assert!(detect_from(true, env(&[("HERDR_ENV", "1"), ("HERDR_PANE_ID", "p")])).is_none());
        assert!(
            detect_from(
                true,
                env(&[
                    ("HERDR_ENV", "1"),
                    ("HERDR_PANE_ID", ""),
                    ("HERDR_BIN_PATH", "/b")
                ])
            )
            .is_none()
        );
    }

    #[test]
    fn resume_argv_keeps_safe_groups_only() {
        let startup = vec![
            vec![
                "--model".to_string(),
                "anthropic/claude-sonnet-5".to_string(),
            ],
            vec!["--mode".to_string(), "free".to_string()],
            vec!["--config".to_string(), "/tmp/it's.toml".to_string()],
            vec!["--disable".to_string(), "mcp:github".to_string()],
        ];
        let argv = resume_argv("s1a2b3c4d5e6-abcdef", &startup);
        assert_eq!(
            argv,
            vec![
                "ka",
                "--session",
                "s1a2b3c4d5e6-abcdef",
                "--model",
                "anthropic/claude-sonnet-5",
                "--mode",
                "free",
                "--disable",
                "mcp:github",
            ]
        );
        // no session, no command (state reports still work)
        assert!(resume_argv("", &startup).is_empty());
        assert!(resume_argv("bad'id", &startup).is_empty());
        // a control character anywhere drops only that group
        let argv = resume_argv(
            "s1",
            &[vec!["--x".into(), "a\tb".into()], vec!["--y".into()]],
        );
        assert_eq!(argv, vec!["ka", "--session", "s1", "--y"]);
    }

    #[test]
    fn resume_argv_respects_argc_and_byte_limits() {
        let many: Vec<Vec<String>> = (0..100)
            .map(|i| vec!["--disable".to_string(), format!("tool:{i}")])
            .collect();
        let argv = resume_argv("s1", &many);
        assert!(argv.len() <= MAX_ARGS, "{} args", argv.len());
        let big = vec![vec!["--config".to_string(), "x".repeat(MAX_BYTES)]];
        assert_eq!(resume_argv("s1", &big), vec!["ka", "--session", "s1"]);
    }

    #[test]
    fn report_args_carry_state_session_and_argv() {
        let argv: Vec<String> = ["ka", "--session", "s1", "--mode", "free"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let args = report_args(&endpoint(), AgentActivity::Working, 42, "s1", &argv);
        assert_eq!(
            args,
            vec![
                "pane",
                "report-agent",
                "pane-7",
                "--source",
                "ka",
                "--agent",
                "ka",
                "--state",
                "working",
                "--seq",
                "42",
                "--agent-session-id",
                "s1",
                "--",
                "ka",
                "--session",
                "s1",
                "--mode",
                "free",
            ]
        );
        let blocked = report_args(&endpoint(), AgentActivity::Blocked, 43, "", &[]);
        assert!(blocked.contains(&"blocked".to_string()));
        assert!(
            blocked
                .windows(2)
                .any(|w| w == ["--message", "waiting for approval"])
        );
        assert!(!blocked.iter().any(|a| a == "--"));
        let idle = report_args(&endpoint(), AgentActivity::Idle, 44, "s1", &[]);
        assert!(idle.contains(&"idle".to_string()));
        assert!(!idle.iter().any(|a| a == "--agent-session-id"));
        assert_eq!(
            release_args(&endpoint(), 9),
            vec![
                "pane",
                "release-agent",
                "pane-7",
                "--source",
                "ka",
                "--agent",
                "ka",
                "--seq",
                "9",
            ]
        );
    }

    #[test]
    fn seq_is_strictly_increasing() {
        let mut seq = 0u64;
        let a = next_seq(&mut seq);
        let b = next_seq(&mut seq);
        assert!(b > a);
        // a clock behind the last value still moves forward
        seq = u64::MAX - 2;
        assert_eq!(next_seq(&mut seq), u64::MAX - 1);
        assert_eq!(next_seq(&mut seq), u64::MAX);
        assert_eq!(next_seq(&mut seq), u64::MAX, "saturates at the cap");
    }

    /// The reporter really invokes the Herdr binary, newest state wins,
    /// and finish releases the pane.
    #[test]
    fn reporter_invokes_the_binary_and_releases_on_finish() {
        let dir = std::env::temp_dir().join(format!("ka-herdr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("calls.log");
        let bin = dir.join("herdr");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\necho '--' >> {}\n",
                log.display(),
                log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut reporter = Reporter::start(
            Endpoint {
                bin: bin.clone(),
                pane: "pane-9".into(),
            },
            vec![vec!["--mode".to_string(), "free".to_string()]],
        );
        let sink = reporter.sink();
        sink.publish(AgentReport::Report(ActivityReport {
            activity: AgentActivity::Working,
            session: "s1a2".into(),
        }));
        let wait_for = |needle: &str| -> String {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let text = std::fs::read_to_string(&log).unwrap_or_default();
                if text.contains(needle) || Instant::now() >= deadline {
                    return text;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let text = wait_for("report-agent");
        assert!(text.contains("pane-9"), "{text}");
        assert!(text.contains("--state"), "{text}");
        assert!(text.contains("working"), "{text}");
        assert!(text.contains("s1a2"), "{text}");
        assert!(text.contains("--mode"), "{text}");
        reporter.finish();
        let text = wait_for("release-agent");
        assert!(text.contains("release-agent"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
