//! Red-team regression tests for ka-harness external surfaces:
//! - `serve`: CSRF Origin/Host/Content-Type, loopback bind, non-sequential session IDs
//! - `mcp-serve`: workspace path-traversal confinement for read/glob/grep
//! - `update`: HTTPS enforcement, fail-closed downgrade tag check, tar-slip & symlink rejection
//! - `schedule`: cron newline/metacharacter and `%` name injection rejection
//! - `worktree`: `--detach`/`--force` flag injection rejection
//! - `import`: empty-pattern widening refusal (`Bash("")`, `Bash()`, `WebFetch(domain:)`)
//! - `import-session`: deeply nested Codex `output` envelopes without stack overflow

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

static TEMP_SEQ: AtomicU32 = AtomicU32::new(0);

fn ka_bin() -> &'static str {
    env!("CARGO_BIN_EXE_ka")
}

fn temp_dir(tag: &str) -> PathBuf {
    let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ka-redteam-{}-{n}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        panic!("create temp dir {}: {e}", dir.display());
    }
    dir
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn raw_http(addr: std::net::SocketAddr, req: &str) -> Result<(u16, String), String> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|e| format!("connect {addr}: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed HTTP response: {text:?}"))?;
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Ok((status, body))
}

#[test]
fn serve_rejects_non_loopback_without_token_and_enforces_csrf_defenses() {
    // Non-loopback bind without token must fail closed.
    let out = match Command::new(ka_bin())
        .args(["serve", "--addr", "0.0.0.0:0"])
        .env("KA_SERVE_TOKEN", "")
        .output()
    {
        Ok(o) => o,
        Err(e) => panic!("spawn ka serve: {e}"),
    };
    assert!(
        !out.status.success(),
        "0.0.0.0 bind without token must be refused"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--token"), "stderr: {stderr}");

    // Find a free loopback port, then start `ka serve` on it.
    let probe = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => panic!("bind probe: {e}"),
    };
    let addr = match probe.local_addr() {
        Ok(a) => a,
        Err(e) => panic!("local_addr: {e}"),
    };
    drop(probe);

    let data_dir = temp_dir("serve-csrf");
    let child = match Command::new(ka_bin())
        .args(["serve", "--addr", &addr.to_string()])
        .env("KA_DATA_DIR", &data_dir)
        .env("KA_SERVE_TOKEN", "")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => panic!("spawn ka serve: {e}"),
    };
    let _guard = KillOnDrop(child);

    // Wait for server readiness.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ready = false;
    while Instant::now() < deadline {
        if let Ok((200, _)) = raw_http(
            addr,
            "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        ) {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ready, "ka serve did not become ready on {addr}");

    // 1. DNS rebinding defense: non-loopback Host header -> 403.
    let res = raw_http(
        addr,
        "GET /health HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n",
    );
    assert!(
        matches!(res, Ok((403, _))),
        "non-loopback Host must 403, got {res:?}"
    );

    // 2. CSRF Origin defense: cross-origin Origin header -> 403.
    let res = raw_http(
        addr,
        "POST /sessions HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: https://evil.example\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    );
    assert!(
        matches!(res, Ok((403, _))),
        "cross-origin Origin must 403, got {res:?}"
    );

    // 3. CSRF Content-Type defense: text/plain POST -> 415.
    let res = raw_http(
        addr,
        "POST /sessions HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    );
    assert!(
        matches!(res, Ok((415, _))),
        "text/plain POST must 415, got {res:?}"
    );

    // 4. Valid loopback JSON POST -> 200 with CSPRNG hex session ID (not s1).
    let (status, body) = match raw_http(
        addr,
        "POST /sessions HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    ) {
        Ok(pair) => pair,
        Err(e) => panic!("POST /sessions failed: {e}"),
    };
    assert_eq!(status, 200, "body: {body}");
    let parsed: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => panic!("invalid JSON {body:?}: {e}"),
    };
    let id = parsed["id"].as_str().unwrap_or("");
    assert_eq!(id.len(), 32, "session id must be 32 hex chars, got {id:?}");
    assert!(
        id.bytes().all(|b| b.is_ascii_hexdigit()),
        "session id must be hex: {id:?}"
    );

    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn mcp_serve_confines_read_glob_grep_to_workspace() {
    let root = temp_dir("mcp-confine");
    let ws = root.join("workspace");
    if let Err(e) = std::fs::create_dir_all(&ws) {
        panic!("mkdir ws: {e}");
    }
    let secret_path = root.join("secret.txt");
    if let Err(e) = std::fs::write(&secret_path, "TOP_SECRET_VALUE\n") {
        panic!("write secret: {e}");
    }

    let mut child = match Command::new(ka_bin())
        .arg("mcp-serve")
        .current_dir(&ws)
        .env("KA_DATA_DIR", root.join("data"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => panic!("spawn mcp-serve: {e}"),
    };

    let reqs = [
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "rt", "version": "0"}}
        }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "read", "arguments": {"path": "../secret.txt"}}
        }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "read", "arguments": {"path": secret_path.display().to_string()}}
        }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "glob", "arguments": {"pattern": "*.txt", "path": ".."}}
        }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": {"name": "grep", "arguments": {"pattern": "TOP_SECRET", "path": ".."}}
        }),
    ];
    if let Some(mut stdin) = child.stdin.take() {
        for r in &reqs {
            let line = format!("{r}\n");
            if let Err(e) = stdin.write_all(line.as_bytes()) {
                panic!("write stdin: {e}");
            }
        }
    }
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => panic!("wait mcp-serve: {e}"),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("TOP_SECRET_VALUE"),
        "secret leaked in mcp-serve output: {stdout}"
    );
    let mut checked = 0usize;
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let id = v["id"].as_u64().unwrap_or(0);
        if (2..=5).contains(&id) {
            assert_eq!(
                v["result"]["isError"],
                serde_json::json!(true),
                "traversal id={id} must error: {line}"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 4, "expected 4 traversal responses in: {stdout}");
    let _ = std::fs::remove_dir_all(&root);
}

fn artifact_triple() -> String {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => "x86_64-unknown-linux-musl".to_string(),
        ("aarch64", "linux") => "aarch64-unknown-linux-musl".to_string(),
        ("x86_64", "macos") => "x86_64-apple-darwin".to_string(),
        ("aarch64", "macos") => "aarch64-apple-darwin".to_string(),
        (arch, os) => format!("{arch}-unknown-{os}"),
    }
}

fn make_raw_tar_gz(entries: &[(&str, u8, &[u8])]) -> Vec<u8> {
    let mut tar = Vec::new();
    for &(name, typeflag, data) in entries {
        let mut header = [0u8; 512];
        let nb = name.as_bytes();
        let nlen = nb.len().min(100);
        header[..nlen].copy_from_slice(&nb[..nlen]);
        header[100..108].copy_from_slice(b"0000755\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size_oct = format!("{:011o}\0", data.len());
        header[124..136].copy_from_slice(size_oct.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = typeflag;
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let chksum: u32 = header.iter().map(|&b| u32::from(b)).sum();
        let chk_oct = format!("{chksum:06o}\0 ");
        header[148..156].copy_from_slice(chk_oct.as_bytes());
        tar.extend_from_slice(&header);
        tar.extend_from_slice(data);
        let rem = data.len() % 512;
        if rem != 0 {
            tar.extend(std::iter::repeat_n(0u8, 512 - rem));
        }
    }
    tar.extend(std::iter::repeat_n(0u8, 1024));
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    if let Err(e) = encoder.write_all(&tar) {
        panic!("gzip write: {e}");
    }
    match encoder.finish() {
        Ok(b) => b,
        Err(e) => panic!("gzip finish: {e}"),
    }
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

fn serve_update_fixture(
    tag: String,
    tarball: Vec<u8>,
) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => panic!("bind fixture: {e}"),
    };
    let addr = match listener.local_addr() {
        Ok(a) => a,
        Err(e) => panic!("local_addr: {e}"),
    };
    let triple = artifact_triple();
    let sha = sha256_hex(&tarball);
    let handle = std::thread::spawn(move || {
        let _ = listener.set_nonblocking(false);
        // Serve up to 4 requests (releases + tarball + sha256/sig).
        for _ in 0..4 {
            let Ok((mut sock, _)) = listener.accept() else {
                break;
            };
            let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let body: Vec<u8> = if req.contains("/releases") {
                serde_json::json!([{
                    "tag_name": tag,
                    "assets": [
                        {"name": format!("ka-{triple}.tar.gz"), "browser_download_url": format!("http://{addr}/tar")},
                        {"name": format!("ka-{triple}.tar.gz.sig"), "browser_download_url": format!("http://{addr}/sig")},
                        {"name": format!("ka-{triple}.tar.gz.sha256"), "browser_download_url": format!("http://{addr}/sha")},
                    ]
                }])
                .to_string()
                .into_bytes()
            } else if req.starts_with("GET /tar ") {
                tarball.clone()
            } else if req.starts_with("GET /sha ") {
                format!("{sha}  ka-{triple}.tar.gz\n").into_bytes()
            } else {
                b"AAAA".to_vec()
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.write_all(&body);
        }
    });
    (addr, handle)
}

#[test]
fn update_rejects_unparseable_tag_and_tar_slip_and_symlink() {
    let work = temp_dir("update-rt");

    // 1. Non-HTTPS non-loopback API base must be rejected.
    let out = match Command::new(ka_bin())
        .args(["update", "--check"])
        .env("XDG_STATE_HOME", &work)
        .env("KA_UPDATE_API_BASE", "http://example.com")
        .output()
    {
        Ok(o) => o,
        Err(e) => panic!("spawn ka update: {e}"),
    };
    assert!(!out.status.success(), "non-HTTPS update URL must fail");

    // 2. Unparseable release tag must fail closed (never bypass downgrade check).
    let (addr, _h) = serve_update_fixture("ka-stable-notaversion".to_string(), Vec::new());
    let out = match Command::new(ka_bin())
        .args(["update", "--check"])
        .env("XDG_STATE_HOME", &work)
        .env("KA_UPDATE_API_BASE", format!("http://{addr}"))
        .output()
    {
        Ok(o) => o,
        Err(e) => panic!("spawn ka update: {e}"),
    };
    assert!(!out.status.success(), "unparseable tag must fail closed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unparseable release tag"),
        "unexpected stderr: {stderr}"
    );

    // 3. Tar-slip (`../escape`) entry must be rejected before install.
    let slip_tar = make_raw_tar_gz(&[("../escape", b'0', b"pwned"), ("ka", b'0', b"fake")]);
    let (addr, _h) = serve_update_fixture("v99.0.0".to_string(), slip_tar);
    let out = match Command::new(ka_bin())
        .arg("update")
        .env("XDG_STATE_HOME", &work)
        .env("KA_UPDATE_API_BASE", format!("http://{addr}"))
        .output()
    {
        Ok(o) => o,
        Err(e) => panic!("spawn ka update: {e}"),
    };
    assert!(!out.status.success(), "tar-slip archive must be rejected");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unsafe entry path") || stderr.contains("signature"),
        "unexpected stderr: {stderr}"
    );

    // 4. Symlink entry in tarball must be rejected.
    let symlink_tar = make_raw_tar_gz(&[("ka", b'2', b"")]);
    let (addr, _h) = serve_update_fixture("v99.0.0".to_string(), symlink_tar);
    let out = match Command::new(ka_bin())
        .arg("update")
        .env("XDG_STATE_HOME", &work)
        .env("KA_UPDATE_API_BASE", format!("http://{addr}"))
        .output()
    {
        Ok(o) => o,
        Err(e) => panic!("spawn ka update: {e}"),
    };
    assert!(!out.status.success(), "symlink archive must be rejected");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("non-regular entry") || stderr.contains("signature"),
        "unexpected stderr: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn schedule_rejects_cron_and_name_injections() {
    let cases: &[(&[&str], &str)] = &[
        (
            &[
                "schedule",
                "add",
                "0 9 * * *\n* * * * * id",
                "hello",
                "--session-name",
                "ok",
            ],
            "newline in cron",
        ),
        (
            &[
                "schedule",
                "add",
                "0 9 * * ;id",
                "hello",
                "--session-name",
                "ok",
            ],
            "shell metachar in cron",
        ),
        (
            &[
                "schedule",
                "add",
                "0 9 * * *",
                "hello",
                "--session-name",
                "bad%name",
            ],
            "percent in name",
        ),
    ];
    for (args, label) in cases {
        let out = match Command::new(ka_bin()).args(*args).output() {
            Ok(o) => o,
            Err(e) => panic!("spawn ka schedule ({label}): {e}"),
        };
        assert!(
            !out.status.success(),
            "schedule must reject {label}, stdout={}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

#[test]
fn worktree_rejects_flag_injection_in_name_and_branch() {
    let repo = temp_dir("worktree-rt");
    let git = |args: &[&str]| {
        let out = match Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
        {
            Ok(o) => o,
            Err(e) => panic!("git {}: {e}", args.join(" ")),
        };
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&[
        "-c",
        "user.email=ka@test",
        "-c",
        "user.name=ka",
        "commit",
        "--allow-empty",
        "-qm",
        "init",
    ]);

    for args in [
        &["worktree", "add", "--", "--detach"][..],
        &["worktree", "add", "fix", "--", "--force"][..],
        &["worktree", "add", "fix", "--", "--detach"][..],
    ] {
        let out = match Command::new(ka_bin())
            .args(args)
            .current_dir(&repo)
            .output()
        {
            Ok(o) => o,
            Err(e) => panic!("spawn ka worktree: {e}"),
        };
        assert!(
            !out.status.success(),
            "worktree must reject {:?}, stdout={}",
            args,
            String::from_utf8_lossy(&out.stdout)
        );
    }
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn import_refuses_empty_pattern_widening() {
    let work = temp_dir("import-rt");
    let src = work.join("settings.json");
    let payload = r#"{
        "permissions": {
            "allow": ["Bash(\"\")", "Bash()", "WebFetch(domain:)"]
        }
    }"#;
    if let Err(e) = std::fs::write(&src, payload) {
        panic!("write settings.json: {e}");
    }
    let out = match Command::new(ka_bin())
        .args(["import", "claude", &src.display().to_string(), "--dry-run"])
        .output()
    {
        Ok(o) => o,
        Err(e) => panic!("spawn ka import: {e}"),
    };
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("nothing to import"),
        "empty patterns must not widen to allow-all: {stdout}"
    );
    assert!(
        stdout.contains("empty pattern"),
        "skip reason must mention empty pattern: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn session_import_handles_deeply_nested_codex_output_without_stack_overflow() {
    let work = temp_dir("session-import-rt");
    let src = work.join("rollout.jsonl");
    let data = work.join("data");

    // Build 64 levels of nested {"output": ...} objects.
    let mut nested = serde_json::json!("leaf");
    for _ in 0..64 {
        nested = serde_json::json!({ "output": nested });
    }
    let line = serde_json::json!({
        "timestamp": "2026-01-01T00:00:00Z",
        "type": "response_item",
        "payload": {
            "type": "function_call_output",
            "call_id": "fc1",
            "output": nested
        }
    });
    if let Err(e) = std::fs::write(&src, format!("{line}\n")) {
        panic!("write rollout: {e}");
    }

    let out = match Command::new(ka_bin())
        .args([
            "import-session",
            "codex",
            &src.display().to_string(),
            "--cwd",
            &work.display().to_string(),
        ])
        .env("KA_DATA_DIR", &data)
        .output()
    {
        Ok(o) => o,
        Err(e) => panic!("spawn ka import-session: {e}"),
    };
    assert!(
        out.status.success(),
        "deeply nested codex output must not crash: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&work);
}
