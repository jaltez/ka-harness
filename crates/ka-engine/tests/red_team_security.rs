//! Red-team security regression suite covering bashp, protected paths,
//! permission rule evaluation, ReDoS-free glob matching, SSRF defenses,
//! archive/PDF reader hardening, monitor/cron bounds, code-mode gating,
//! team lifecycle cleanup, tangential parsing, and secret redaction.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ka_engine::config::{Rule, Verdict, evaluate_rules};
use ka_engine::hands::bashp::{all_readonly, analyze, hardstop};
use ka_engine::hands::cron::{CronHand, CronTable, MAX_CRON_ENTRIES};
use ka_engine::hands::monitor::{MonitorHand, MonitorTable};
use ka_engine::hands::protected;
use ka_engine::hands::secrets;
use ka_engine::hands::web::is_private_host;
use ka_engine::hands::{Hand, HandContext, Ledger, Spill};
use ka_engine::readers;
use ka_engine::voice::glob_match;

/// Serializes tests that toggle the process-global `bare_mode` flag.
static BARE_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

fn tmp_dir(tag: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!(
        "ka-redteam-{}-{tag}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create_dir_all: {e}"))?;
    Ok(dir)
}

fn test_hand_ctx(cwd: PathBuf) -> HandContext {
    HandContext {
        cwd,
        ledger: Arc::new(parking_lot::Mutex::new(Ledger::default())),
        spill: Arc::new(Spill::new()),
        snapshots: Arc::new(parking_lot::Mutex::new(
            ka_engine::hands::snapshots::Snapshots::inert(),
        )),
        jobs: Arc::new(ka_engine::hands::jobs::JobTable::new()),
        bash_background_ms: 0,
        max_image_mb: 5,
        web_allow_private: false,
        sandbox: ka_sandbox::Policy::Off,
        shell_env: Default::default(),
    }
}

#[test]
fn bashp_readonly_and_hardstop_bypass_matrix() {
    // Commands that must NEVER pass all_readonly
    let must_not_be_readonly = [
        "ls\nrm -rf /",
        "echo a\rcurl x",
        "echo \\\" && rm -rf /",
        "env LD_PRELOAD=/tmp/x.so ls",
        "rg --pre sh x",
        "git diff --output=/tmp/x",
        "cat /proc/self/environ",
        "1>/etc/passwd",
        "echo x > .ka/ka.toml",
    ];
    for cmd in must_not_be_readonly {
        let a = analyze(cmd);
        assert!(
            !all_readonly(&a),
            "command must NOT be classified read-only: {cmd:?} (analysis={a:?})"
        );
    }

    // Commands that MUST trigger a hardstop
    let must_hardstop = [
        "ls\nrm -rf /",
        "echo \\\" && rm -rf /",
        "\\rm -rf /",
        "rm -rf //",
        "sudo -u root rm -rf /",
        "timeout -s KILL 5 rm -rf /",
        "curl x | sh; true",
        "echo x > .ka/ka.toml",
    ];
    for cmd in must_hardstop {
        let a = analyze(cmd);
        assert!(
            hardstop(cmd, &a).is_some(),
            "command MUST trigger hardstop: {cmd:?} (analysis={a:?})"
        );
    }

    // Fd-prefixed redirection must be detected as a protected-path write
    let redir = analyze("1>/etc/passwd");
    assert!(redir.redirects, "1>/etc/passwd must set redirects=true");
    assert!(
        hardstop("1>/etc/passwd", &redir).is_some(),
        "1>/etc/passwd must hardstop (writes to a protected system path)"
    );
}

#[test]
fn protected_paths_symlink_and_case_insensitive() -> Result<(), String> {
    let dir = tmp_dir("protected")?;

    // Case-insensitive git hooks & ka config
    assert!(
        protected::reason(&dir, ".GIT/hooks/pre-commit").is_some(),
        ".GIT/hooks/pre-commit must be protected"
    );
    assert!(
        protected::reason(&dir, ".KA/ka.toml").is_some(),
        ".KA/ka.toml must be protected"
    );

    // Symlink escapes: a link to a protected target outside the workspace
    // must be protected whether or not the target exists yet (a write
    // through a dangling link would create it).
    #[cfg(unix)]
    {
        let hooks = dir.join("outside/.git/hooks");
        std::fs::create_dir_all(&hooks).map_err(|e| format!("mkdir: {e}"))?;
        let target = hooks.join("pre-commit");
        std::fs::write(&target, b"#!/bin/sh\n").map_err(|e| format!("write: {e}"))?;
        std::os::unix::fs::symlink(&target, dir.join("link"))
            .map_err(|e| format!("symlink failed: {e}"))?;
        assert!(
            protected::reason(&dir, "link").is_some(),
            "symlink ./link -> outside/.git/hooks/pre-commit must be protected"
        );
        // dangling link: the target does not exist yet; the write would create it
        std::os::unix::fs::symlink(hooks.join("post-commit"), dir.join("dang"))
            .map_err(|e| format!("symlink failed: {e}"))?;
        assert!(
            protected::reason(&dir, "dang").is_some(),
            "a dangling symlink into .git/hooks must stay protected"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn evaluate_rules_first_matching_rule_wins() {
    let rules = vec![
        Rule {
            tool: "bash".into(),
            pattern: Some("rm -rf *".into()),
            verdict: Verdict::Deny,
        },
        Rule {
            tool: "bash".into(),
            pattern: Some("curl *".into()),
            verdict: Verdict::Deny,
        },
        Rule {
            tool: "bash".into(),
            pattern: None,
            verdict: Verdict::Allow,
        },
    ];
    assert_eq!(
        evaluate_rules(&rules, "bash", "rm -rf /tmp/x"),
        Some(Verdict::Deny)
    );
    assert_eq!(
        evaluate_rules(&rules, "bash", "curl https://example.com"),
        Some(Verdict::Deny),
        "second deny rule for the same tool MUST be enforced"
    );
    assert_eq!(
        evaluate_rules(&rules, "bash", "ls -la"),
        Some(Verdict::Allow),
        "fallthrough allow rule applies when earlier deny patterns don't match"
    );

    // Project deny placed before user allow must win
    let layered = vec![
        Rule {
            tool: "write".into(),
            pattern: Some("*.env".into()),
            verdict: Verdict::Deny,
        },
        Rule {
            tool: "write".into(),
            pattern: None,
            verdict: Verdict::Allow,
        },
    ];
    assert_eq!(
        evaluate_rules(&layered, "write", "secrets.env"),
        Some(Verdict::Deny)
    );
    assert_eq!(
        evaluate_rules(&layered, "write", "src/lib.rs"),
        Some(Verdict::Allow)
    );
}

#[test]
fn glob_match_no_redos() {
    let pattern = "a*a*a*a*a*a*a*a*a*b";
    let text = "a".repeat(50);
    let started = Instant::now();
    let matched = glob_match(pattern, &text);
    let elapsed = started.elapsed();
    assert!(!matched);
    assert!(
        elapsed < Duration::from_millis(50),
        "glob_match took too long ({elapsed:?}), possible ReDoS"
    );
}

#[test]
fn web_ssrf_matrix_blocks_private_and_obfuscated_hosts() {
    let blocked = [
        "http://169.254.169.254?",
        "http://127.0.0.1#",
        "http://localhost./",
        "http://2130706433/",
        "http://0177.0.0.1/",
        "http://0x7f000001/",
        "http://127.1/",
        "http://100.64.0.1/",
        "http://[64:ff9b::169.254.169.254]/",
    ];
    for url in blocked {
        assert!(
            is_private_host(url),
            "SSRF check MUST block private/obfuscated URL: {url}"
        );
    }
}

#[test]
fn readers_zip_tar_pdf_hardening() -> Result<(), String> {
    let dir = tmp_dir("readers")?;

    // 1. Truncated ZIP EOCD: must return Err without panicking
    let trunc_path = dir.join("trunc.zip");
    std::fs::write(&trunc_path, b"PK\x05\x06\x00\x00").map_err(|e| format!("write trunc: {e}"))?;
    let mut f = std::fs::File::open(&trunc_path).map_err(|e| format!("open trunc: {e}"))?;
    assert!(
        readers::ZipDir::open(&mut f).is_err(),
        "truncated ZIP EOCD must fail cleanly"
    );

    // 2. 4 GiB cd_size in ZIP EOCD: must be rejected cleanly without OOM/panic
    let huge_path = dir.join("huge_cd.zip");
    let mut eocd = vec![0u8; 22];
    eocd[0..4].copy_from_slice(b"PK\x05\x06");
    eocd[12..16].copy_from_slice(&u32::MAX.to_le_bytes()); // cd_size = ~4 GiB
    eocd[16..20].copy_from_slice(&0u32.to_le_bytes()); // cd_offset = 0
    std::fs::write(&huge_path, &eocd).map_err(|e| format!("write huge: {e}"))?;
    let mut f2 = std::fs::File::open(&huge_path).map_err(|e| format!("open huge: {e}"))?;
    assert!(
        readers::ZipDir::open(&mut f2).is_err(),
        "4 GiB cd_size must be rejected cleanly"
    );

    // 3. GNU base-256 tar overflow: must not panic
    let mut tar_hdr = vec![0u8; 512];
    tar_hdr[0..4].copy_from_slice(b"evil");
    // Size field at 124..136: GNU base-256 high bit set + max bytes
    tar_hdr[124] = 0x80;
    for b in &mut tar_hdr[125..136] {
        *b = 0xff;
    }
    tar_hdr[257..263].copy_from_slice(b"ustar\0");
    // Compute valid header checksum (bytes 148..156 treated as spaces)
    for b in &mut tar_hdr[148..156] {
        *b = b' ';
    }
    let sum: u32 = tar_hdr.iter().map(|&b| u32::from(b)).sum();
    let chk = format!("{sum:06o}\0 ");
    tar_hdr[148..156].copy_from_slice(chk.as_bytes());
    let mut tar_data = tar_hdr;
    tar_data.extend_from_slice(&[0u8; 1024]);
    // Must not panic (Err or empty is acceptable; overflow must be handled safely)
    let _ = readers::tar_entries(&tar_data);

    // 4. PDF unmatched `obj` must run in linear time: 20k objects take
    // HEAD's quadratic rescan ~17 s in a debug build and ~5 ms linear
    let mut pdf_bytes = b"%PDF-1.4\n".to_vec();
    for i in 1..=20000 {
        let _ = writeln!(&mut pdf_bytes, "{i} 0 obj");
    }
    let started = Instant::now();
    let _ = readers::pdf_text(&pdf_bytes);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "PDF unmatched obj scan took too long ({elapsed:?})"
    );

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

#[tokio::test]
async fn monitor_utf8_boundary_buffer_cap_and_bare_mode() -> Result<(), String> {
    let _guard = BARE_LOCK.lock().await;
    ka_engine::conventions::set_bare_mode(false);

    let table = Arc::new(MonitorTable::new());
    // Emit multi-byte UTF-8 characters ('é' = 2 bytes) crossing 32768 bytes
    let (id, _pid) = table.spawn_with_env(
        "python3 -c \"import sys; sys.stdout.write(('é'*20000) + '\\n')\" 2>/dev/null || printf '%40000s\\n' '' | tr ' ' 'a'",
        &Default::default(),
    )?;
    // Wait briefly for the short-lived process to finish flushing
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(r) = table.read(&id, 0) {
            if r.exit_code.is_some() {
                assert!(r.text.len() <= 32 * 1024 + 4);
                break;
            }
        }
    }
    // Reading from offset 0 must never panic on char boundary
    let read_res = table.read(&id, 0)?;
    assert!(read_res.next_offset <= read_res.text.len() + 4 || read_res.truncated);
    let _ = table.kill(&id);

    // bare_mode refusal on MonitorHand::execute
    struct BareReset;
    impl Drop for BareReset {
        fn drop(&mut self) {
            ka_engine::conventions::set_bare_mode(false);
        }
    }
    let _reset = BareReset;
    ka_engine::conventions::set_bare_mode(true);
    let hand = MonitorHand::new(Arc::new(MonitorTable::new()));
    let dir = tmp_dir("monitor-bare")?;
    let ctx = test_hand_ctx(dir.clone());
    let out = hand
        .execute(
            &serde_json::json!({"action": "spawn", "command": "echo hi"}),
            &ctx,
        )
        .await;
    assert!(
        out.is_error,
        "monitor spawn must be refused in bare_mode: {}",
        out.content
    );
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

#[tokio::test]
async fn cron_u64_max_interval_and_entry_cap() -> Result<(), String> {
    let _guard = BARE_LOCK.lock().await;
    ka_engine::conventions::set_bare_mode(false);

    let table = Arc::new(CronTable::new());
    let hand = CronHand::new(table.clone());
    let dir = tmp_dir("cron")?;
    let ctx = test_hand_ctx(dir.clone());

    // u64::MAX interval must not panic and should be rejected cleanly
    let out = hand
        .execute(
            &serde_json::json!({
                "action": "create",
                "schedule": "forever",
                "interval_secs": u64::MAX,
                "prompt": "ping"
            }),
            &ctx,
        )
        .await;
    assert!(
        out.is_error,
        "u64::MAX interval should be rejected cleanly (got: {})",
        out.content
    );

    // Entry cap enforcement
    let cap_table = CronTable::new();
    for i in 0..MAX_CRON_ENTRIES {
        let res = cap_table.try_create(format!("every {i}m"), "tick".into(), 60, false);
        assert!(res.is_ok(), "entry {i} within cap must succeed");
    }
    let over = cap_table.try_create("overflow".into(), "tick".into(), 60, false);
    assert!(over.is_err(), "entry beyond MAX_CRON_ENTRIES must fail");

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

#[cfg(feature = "code")]
#[tokio::test]
async fn code_hand_blocks_dummy_readonly_exec_and_plans_traversal() -> Result<(), String> {
    use ka_dialect::speaker::{SpeakFuture, SpeakRequest, Speaker, StreamEvent, ToolCall};
    use ka_protocol::{Command, Event, Mode, Stop, Usage};

    let _guard = BARE_LOCK.lock().await;
    ka_engine::conventions::set_bare_mode(false);

    struct OneCallSpeaker {
        call: parking_lot::Mutex<Option<ToolCall>>,
    }
    impl Speaker for OneCallSpeaker {
        fn speak<'a>(
            &'a self,
            _req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            let next = self.call.lock().take();
            Box::pin(async move {
                if let Some(call) = next {
                    let _ = out.send(StreamEvent::Call(call)).await;
                    let _ = out
                        .send(StreamEvent::Finished {
                            stop: Stop::Done,
                            usage: Usage::default(),
                        })
                        .await;
                } else {
                    let _ = out.send(StreamEvent::Text("done".into())).await;
                    let _ = out
                        .send(StreamEvent::Finished {
                            stop: Stop::Done,
                            usage: Usage::default(),
                        })
                        .await;
                }
            })
        }
    }

    let catalog = ka_dialect::Catalog::parse(
        "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 100000\n",
    )
    .map_err(|e| format!("catalog parse: {e}"))?;

    // 1. Dummy `"command": "ls"` on `cron` inside code must be denied in Mode::Ask
    let dir1 = tmp_dir("code-exec")?;
    let cfg1 = ka_engine::Config {
        cwd: Some(dir1.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        mode: Some(Mode::Ask),
        ..Default::default()
    };
    let script1 = r#"
        const res = await ka.tools.call("cron", JSON.stringify({
            action: "create",
            command: "ls",
            schedule: "every 1m",
            interval_secs: 60,
            prompt: "pwn"
        }));
        return res;
    "#;
    let speaker1 = Arc::new(OneCallSpeaker {
        call: parking_lot::Mutex::new(Some(ToolCall {
            id: "c1".into(),
            tool: "code".into(),
            arguments: serde_json::json!({ "script": script1 }),
        })),
    });
    let mut handle1 = ka_engine::spawn_with_speaker(
        cfg1,
        catalog.clone(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker1,
    );
    let _ = handle1
        .commands
        .send(Command::Prompt {
            text: "run".into(),
            schema: None,
            images: Vec::new(),
            allowed_tools: None,
            model: None,
            skills: Vec::new(),
        })
        .await;
    let mut code_output1 = String::new();
    while let Ok(Some(ev)) =
        tokio::time::timeout(Duration::from_secs(10), handle1.events.recv()).await
    {
        match ev {
            Event::Ask { id, .. } => {
                // Allow the outer `code` tool itself so we test CodeRouter::gate inside it
                let _ = handle1
                    .commands
                    .send(Command::Answer {
                        question: id,
                        choice: 0,
                    })
                    .await;
            }
            Event::CallOutput { tool, excerpt, .. } if tool == "code" => {
                code_output1.push_str(&excerpt);
            }
            Event::Idle => break,
            _ => {}
        }
    }
    assert!(
        code_output1.contains("error"),
        "cron with dummy command=ls must be denied inside code in Mode::Ask (got: {code_output1:?})"
    );
    let _ = std::fs::remove_dir_all(&dir1);

    // 2. `.ka/plans/../../evil.txt` write inside code must be denied in Mode::Readonly
    let dir2 = tmp_dir("code-readonly")?;
    let cfg2 = ka_engine::Config {
        cwd: Some(dir2.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        mode: Some(Mode::Readonly),
        ..Default::default()
    };
    let script2 = r#"
        const res = await ka.tools.call("write", JSON.stringify({
            path: ".ka/plans/../../evil.txt",
            content: "escaped"
        }));
        return res;
    "#;
    let speaker2 = Arc::new(OneCallSpeaker {
        call: parking_lot::Mutex::new(Some(ToolCall {
            id: "c2".into(),
            tool: "code".into(),
            arguments: serde_json::json!({ "script": script2 }),
        })),
    });
    let mut handle2 = ka_engine::spawn_with_speaker(
        cfg2,
        catalog,
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker2,
    );
    let _ = handle2
        .commands
        .send(Command::Prompt {
            text: "run".into(),
            schema: None,
            images: Vec::new(),
            allowed_tools: None,
            model: None,
            skills: Vec::new(),
        })
        .await;
    let mut code_output2 = String::new();
    while let Ok(Some(ev)) =
        tokio::time::timeout(Duration::from_secs(10), handle2.events.recv()).await
    {
        match ev {
            Event::Ask { id, .. } => {
                let _ = handle2
                    .commands
                    .send(Command::Answer {
                        question: id,
                        choice: 0,
                    })
                    .await;
            }
            Event::CallOutput { tool, excerpt, .. } if tool == "code" => {
                code_output2.push_str(&excerpt);
            }
            Event::Idle => break,
            _ => {}
        }
    }
    assert!(
        code_output2.contains("error") || code_output2.contains("read-only"),
        ".ka/plans/../../evil.txt must be denied in Mode::Readonly (got: {code_output2:?})"
    );
    assert!(
        !dir2.join("evil.txt").exists(),
        "traversal write must not create evil.txt"
    );
    let _ = std::fs::remove_dir_all(&dir2);
    Ok(())
}

#[tokio::test]
async fn teams_drop_cleans_up_member_no_arc_leak() -> Result<(), String> {
    let _guard = BARE_LOCK.lock().await;
    ka_engine::conventions::set_bare_mode(false);

    let dir = tmp_dir("teams-drop")?;
    let strand = ka_strand::StrandFile::create(&dir, None)
        .map_err(|e| format!("StrandFile::create: {e}"))?;
    let catalog = ka_dialect::Catalog::parse(
        "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 100000\n",
    )
    .map_err(|e| format!("catalog parse: {e}"))?;
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        ..Default::default()
    };

    let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel(64);
    let mode_slot = Arc::new(parking_lot::RwLock::new(ka_protocol::Mode::Ask));
    let table = ka_engine::hands::teams::TeamTable::new(
        2, &dir, catalog, cfg, mode_slot, evt_tx, &strand, None,
    );
    let spawned = table.spawn("worker", "say hello").await?;
    assert!(spawned.contains("worker"), "{spawned}");

    // Dropping TeamTable must drop TeamInner (no Arc cycle with watch_member),
    // which drops TeamInner.events (evt_tx) and aborts the member engine.
    drop(table);
    let closed = tokio::time::timeout(Duration::from_secs(3), async {
        while evt_rx.recv().await.is_some() {}
    })
    .await;
    assert!(
        closed.is_ok(),
        "TeamTable drop leaked Arc<TeamInner> (events channel never closed)"
    );

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

#[tokio::test]
async fn tangential_cancellation_is_question_not_cancel_usage() -> Result<(), String> {
    let _guard = BARE_LOCK.lock().await;
    ka_engine::conventions::set_bare_mode(false);

    let dir = tmp_dir("tan-word")?;
    let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel(16);
    let tan = ka_engine::hands::tangential::Tangential::new(
        Arc::new(parking_lot::RwLock::new(
            ka_engine::hands::pathfinder::PathfinderSource::default(),
        )),
        ka_protocol::Mode::Ask,
        ka_engine::hands::tasks::AgentTaskTable::new(),
        evt_tx,
        ka_engine::features::FeatureToggles::slot(Vec::new()),
        Arc::new(Vec::new()),
    );
    tan.handle("cancellation", &dir).await;
    // Wait briefly for any immediate usage note (if misparsed as `cancel`)
    let mut saw_usage = false;
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(150), evt_rx.recv()).await {
        if let ka_protocol::Event::Note { message } = ev {
            if message.contains("tan: usage") {
                saw_usage = true;
            }
        }
    }
    assert!(
        !saw_usage,
        "`/tan cancellation` must parse as a question, not cancel usage"
    );
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn secrets_redacts_common_token_and_key_formats() {
    let sample = "\
anthropic=sk-ant-api03-1234567890abcdef123456
github=ghp_1234567890abcdefghij123456
aws=AKIAIOSFODNN7EXAMPLE
-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAtestkeymaterial123456
-----END RSA PRIVATE KEY-----
db=postgres://alice:supersecret@db.example.com:5432/prod
";
    let out = secrets::redact(sample);
    assert!(
        !out.contains("sk-ant-api03-"),
        "anthropic key leaked: {out}"
    );
    assert!(
        !out.contains("ghp_1234567890"),
        "github token leaked: {out}"
    );
    assert!(
        !out.contains("AKIAIOSFODNN7EXAMPLE"),
        "AWS key leaked: {out}"
    );
    assert!(!out.contains("MIIEowIBAAKCAQEA"), "PEM key leaked: {out}");
    assert!(
        !out.contains("alice:supersecret@"),
        "postgres credentials leaked: {out}"
    );
}
