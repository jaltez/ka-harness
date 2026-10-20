//! Repo automation for ka, following the cargo-xtask convention.
//! Usage: `cargo xtask <task>` — see `help` for the task list.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Binary-size contract from the design docs (MB).
const SIZE_BUDGET_MB: f64 = 10.0;

fn main() {
    let task = env::args().nth(1).unwrap_or_else(|| "help".to_string());
    let rest: Vec<String> = env::args().skip(2).collect();
    let code = match task.as_str() {
        "install" => install(),
        "publish" => publish(&rest),
        "link" => link(),
        "unlink" => unlink(),
        "dev" => dev(&rest),
        "ci" => ci(),
        "size" => size(&rest),
        "deps" => deps(),
        "models-sync" => models_sync(),
        "keygen" => keygen(&rest),
        "sign" => sign(&rest),
        "release" => release(),
        "bench" => bench(&rest),
        "live-smoke" => live_smoke(),
        "help" | "--help" | "-h" => {
            print_help();
            0
        }
        other => {
            eprintln!("xtask: unknown task {other:?}\n");
            print_help();
            2
        }
    };
    std::process::exit(code);
}

fn print_help() {
    println!(
        "ka repo automation (cargo xtask <task>)

  install   install STABLE ka globally (cargo install --path, --locked)
  publish   publish the workspace crates to crates.io in dependency
            order; skips crates already live at the current version, so
            a rate-limited run resumes with the same command
            (--dry-run verifies without uploading)
  link      build release + symlink kad -> ./target/release/ka in ~/.cargo/bin (DEV binary)
  unlink    remove the kad symlink
  dev [...] rebuild release, then run the dev binary with any args
            (hint: KA_DATA_DIR=/tmp/ka-dev cargo xtask dev -- models)
  ci        fmt --check + clippy -D warnings + tests (what CI runs)
  models-sync  regenerate crates/ka-dialect/models-dev.toml from models.dev
  keygen    generate ka-harness-release.key/.pub (ed25519, refused if keys exist;
            --force overwrites)
  sign <file>  sign with ka-harness-release.key (or $KA_SIGNING_KEY base64) -> <file>.sig
  release   build musl release, tar.gz it, sign, print artifact paths
  size [--bin <path>]
            release binary size vs the 10 MB contract (default
            target/release/ka; --bin gates any artifact, e.g. musl)
  deps      ka binary dependency-graph allowlist gate — fails on any new
            crate name (list: crates/xtask/deps-allowlist.txt)
  bench [--update] [--bin <path>]
            p50 `ka --version` latency + peak RSS vs baselines.json
            (±30% tolerance; --update rewrites the baseline)
  live-smoke  scripted one-turn conversations against real providers
            (ANTHROPIC_API_KEY / OPENAI_API_KEY / KA_LM_URL; never in CI)
  help      this message"
    );
}

/// Repo root (parent of crates/xtask).
fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo>/crates/xtask → two levels up
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn cargo_bin_dir() -> PathBuf {
    let home = env::var("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|_| env::var("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .unwrap_or_else(|_| PathBuf::from(".cargo"));
    home.join("bin")
}

fn run(cmd: &mut Command) -> i32 {
    println!("$ {:?}", cmd);
    match cmd.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("xtask: failed to run {:?}: {e}", cmd.get_program());
            1
        }
    }
}

/// `cargo install --path crates/ka-harness --locked` — the stable channel.
fn install() -> i32 {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(repo_root())
        .args(["install", "--path", "crates/ka-harness", "--locked"]);
    let code = run(&mut cmd);
    if code == 0 {
        println!("\nstable `ka` installed to {}", cargo_bin_dir().display());
    }
    code
}

/// Publish the workspace crates to crates.io in dependency order
/// (leaf first, the `ka-harness` binary last). Crates already live at
/// the current version are skipped, so a run interrupted by a
/// crates.io rate limit resumes with the same command.
/// `--dry-run` packages and verifies every crate without uploading.
fn publish(args: &[String]) -> i32 {
    const ORDER: [&str; 8] = [
        "ka-protocol",
        "ka-strand",
        "ka-dialect",
        "ka-sandbox",
        "ka-engine",
        "ka-index",
        "ka-term",
        "ka-harness",
    ];
    let dry = args.iter().any(|a| a == "--dry-run");
    for name in ORDER {
        if !dry && published_version(name) == Some(current_version()) {
            println!("skip {name} {} (already on crates.io)", current_version());
            continue;
        }
        let mut cmd = Command::new("cargo");
        cmd.current_dir(repo_root())
            .args(["publish", "-p", name, "--locked"]);
        if dry {
            // verification runs against the working tree; the real publish
            // stays strict so a release always ships committed state
            cmd.args(["--dry-run", "--allow-dirty"]);
        }
        let code = run(&mut cmd);
        if code != 0 {
            eprintln!("xtask: publish stopped at {name}");
            return code;
        }
    }
    if dry {
        println!("\ndry-run passed for all workspace crates");
    } else {
        println!("\nall workspace crates published to crates.io");
    }
    0
}

/// The workspace version every crate inherits — xtask shares it via
/// `version.workspace = true`.
fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Latest version of `name` on crates.io, or None when unpublished or
/// unreachable (a network hiccup then just defers to cargo publish's
/// own error).
fn published_version(name: &str) -> Option<String> {
    let out = Command::new("cargo")
        .args(["search", name, "--limit", "1"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    // first matching line: `ka-protocol = "0.1.0"    # description`
    let line = stdout
        .lines()
        .find(|l| l.starts_with(&format!("{name} = \"")))?;
    line.split('"').nth(1).map(str::to_string)
}

/// Build release and symlink `kad` → repo's target/release/ka (dev channel).
fn link() -> i32 {
    let build = run(Command::new("cargo")
        .args(["build", "--release", "-p", "ka-harness"])
        .current_dir(repo_root()));
    if build != 0 {
        return build;
    }
    let target = repo_root().join("target/release/ka");
    if !target.exists() {
        eprintln!("xtask: {} missing after build", target.display());
        return 1;
    }
    let link = cargo_bin_dir().join("kad");
    if link.exists() || link.symlink_metadata().is_ok() {
        if let Err(e) = fs::remove_file(&link) {
            eprintln!("xtask: cannot remove old kad link: {e}");
            return 1;
        }
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap_or_else(|e| {
        panic!(
            "xtask: symlink {} -> {}: {e}",
            link.display(),
            target.display()
        )
    });
    println!("dev `kad` -> {}", target.display());
    println!("tip: KA_DATA_DIR=/tmp/ka-dev kad ... isolates dev sessions");
    0
}

/// Remove the kad symlink.
fn unlink() -> i32 {
    let link = cargo_bin_dir().join("kad");
    if link.symlink_metadata().is_ok() {
        if let Err(e) = fs::remove_file(&link) {
            eprintln!("xtask: {e}");
            return 1;
        }
        println!("removed {}", link.display());
    } else {
        println!("nothing to remove at {}", link.display());
    }
    0
}

/// Rebuild and execute the dev binary with passthrough args.
fn dev(rest: &[String]) -> i32 {
    let build = run(Command::new("cargo")
        .args(["build", "--release", "-p", "ka-harness"])
        .current_dir(repo_root()));
    if build != 0 {
        return build;
    }
    let bin = repo_root().join("target/release/ka");
    let mut cmd = Command::new(&bin);
    cmd.args(rest);
    run(&mut cmd)
}

/// The full CI gate locally.
fn ci() -> i32 {
    let fmt = run(Command::new("cargo")
        .args(["fmt", "--all", "--check"])
        .current_dir(repo_root()));
    if fmt != 0 {
        return fmt;
    }
    let clippy = run(Command::new("cargo")
        .args([
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ])
        .current_dir(repo_root()));
    if clippy != 0 {
        return clippy;
    }
    let graph_gate = deps();
    if graph_gate != 0 {
        return graph_gate;
    }
    run(Command::new("cargo")
        .args(["test", "--workspace", "--all-features"])
        .current_dir(repo_root()))
}

/// Release binary size vs the footprint contract (`--bin <path>` gates any
/// artifact, e.g. the musl build CI ships).
fn size(rest: &[String]) -> i32 {
    let bin = match rest.iter().position(|a| a == "--bin") {
        Some(i) => {
            let Some(path) = rest.get(i + 1) else {
                eprintln!("xtask: size --bin missing a path");
                return 2;
            };
            PathBuf::from(path)
        }
        None => repo_root().join("target/release/ka"),
    };
    if !bin.exists() {
        if rest.iter().any(|a| a == "--bin") {
            eprintln!("xtask: {} missing — build it first", bin.display());
            return 2;
        }
        let build = run(Command::new("cargo")
            .args(["build", "--release", "-p", "ka-harness"])
            .current_dir(repo_root()));
        if build != 0 {
            return build;
        }
    }
    let bytes = fs::metadata(&bin).map(|m| m.len()).unwrap_or(0);
    let mb = bytes as f64 / 1024.0 / 1024.0;
    let verdict = if mb <= SIZE_BUDGET_MB {
        "OK"
    } else {
        "OVER BUDGET"
    };
    println!(
        "{:.2} MB / {:.0} MB contract — {verdict}",
        mb, SIZE_BUDGET_MB
    );
    if mb <= SIZE_BUDGET_MB { 0 } else { 1 }
}

/// `xtask deps`: the `ka` binary's dependency-graph allowlist — the guard
/// that keeps resident-agent concerns (messaging channels, web
/// frameworks, schedulers, plugin runtimes; see
/// research/ka-harness/agent-boundary.md) out of the shipped binary, and makes
/// any new dependency a deliberate, reviewed act. Compares
/// `cargo tree -p ka-harness` (default features, normal edges) against
/// `crates/xtask/deps-allowlist.txt` (crate names, one per line).
/// Unknown names in the graph fail; names gone from the graph are
/// reported as stale (platform-specific entries, prune when real).
fn deps() -> i32 {
    let allowlist = repo_root().join("crates/xtask/deps-allowlist.txt");
    let Ok(text) = fs::read_to_string(&allowlist) else {
        eprintln!("xtask: {} missing", allowlist.display());
        return 2;
    };
    let allowed: BTreeSet<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let Ok(output) = Command::new("cargo")
        .args([
            "tree",
            "-p",
            "ka-harness",
            "--edges",
            "normal",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ])
        .current_dir(repo_root())
        .output()
    else {
        eprintln!("xtask: cargo tree failed to start");
        return 2;
    };
    if !output.status.success() {
        eprintln!("xtask: cargo tree failed");
        return 2;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let graph: BTreeSet<&str> = stdout
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(|spec| spec.split_once(' ').map_or(spec, |(name, _)| name))
        .collect();
    let added: Vec<&str> = graph.difference(&allowed).copied().collect();
    let stale: Vec<&str> = allowed.difference(&graph).copied().collect();
    if !stale.is_empty() {
        println!(
            "stale allowlist entries (not in today's graph — platform-specific or pruned): {}",
            stale.join(", ")
        );
    }
    if added.is_empty() {
        println!(
            "deps OK — {} crate names allowed, graph matches",
            allowed.len()
        );
        0
    } else {
        eprintln!(
            "new crates in the ka binary graph: {}\n  not in {} — review against the footprint contract, then add deliberately",
            added.join(", "),
            allowlist.display()
        );
        1
    }
}

/// Base64 (standard alphabet) encoder — keeps xtask dependency-light.
fn b64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = ((chunk[0] as u32) << 16)
            | ((*chunk.get(1).unwrap_or(&0) as u32) << 8)
            | (*chunk.get(2).unwrap_or(&0) as u32);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Base64 decode (ignores whitespace and '=' padding).
fn unb64(text: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = text
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = Vec::with_capacity(cleaned.len() * 3 / 4);
    for chunk in cleaned.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// 32 random bytes from the OS (keygen only needs /dev/urandom).
fn os_random_32() -> Option<[u8; 32]> {
    use std::io::Read;
    let mut key = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .ok()?
        .read_exact(&mut key)
        .ok()?;
    Some(key)
}

fn key_paths() -> (PathBuf, PathBuf) {
    (
        repo_root().join("ka-harness-release.key"),
        repo_root().join("ka-harness-release.pub"),
    )
}

/// `xtask keygen [--force]`: write ka-harness-release.key/.pub (base64), print pub.
fn keygen(rest: &[String]) -> i32 {
    let force = rest.iter().any(|a| a == "--force");
    let (key_path, pub_path) = key_paths();
    if !force && (key_path.exists() || pub_path.exists()) {
        eprintln!(
            "xtask: {} already exists (use --force to overwrite)",
            key_path.display()
        );
        return 2;
    }
    let Some(seed) = os_random_32() else {
        eprintln!("xtask: cannot read /dev/urandom");
        return 2;
    };
    let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
    let secret = b64(signing.as_bytes());
    let public = b64(signing.verifying_key().as_bytes());
    if let Err(e) = std::fs::write(&key_path, secret + "\n") {
        eprintln!("xtask: write {}: {e}", key_path.display());
        return 2;
    }
    if let Err(e) = std::fs::write(&pub_path, public.clone() + "\n") {
        eprintln!("xtask: write {}: {e}", pub_path.display());
        return 2;
    }
    println!("wrote {} and {}", key_path.display(), pub_path.display());
    println!("public key: {public}");
    println!(
        "keep the .key private; set KA_SIGNING_KEY or leave the file at the repo root to sign"
    );
    0
}

/// The signing key: $KA_SIGNING_KEY (base64) or ka-harness-release.key at the root.
fn load_signing_key() -> Option<ed25519_dalek::SigningKey> {
    let text = match env::var("KA_SIGNING_KEY") {
        Ok(k) if !k.trim().is_empty() => k,
        _ => std::fs::read_to_string(key_paths().0).ok()?,
    };
    let seed: [u8; 32] = unb64(text.trim())?.try_into().ok()?;
    Some(ed25519_dalek::SigningKey::from_bytes(&seed))
}

/// `xtask sign <file>`: write <file>.sig (base64 ed25519 signature).
fn sign(rest: &[String]) -> i32 {
    let Some(file) = rest.first() else {
        eprintln!("xtask: sign <file> required");
        return 2;
    };
    let Some(signing) = load_signing_key() else {
        eprintln!("xtask: no signing key (set KA_SIGNING_KEY or run `cargo xtask keygen`)");
        return 2;
    };
    let Ok(bytes) = std::fs::read(file) else {
        eprintln!("xtask: cannot read {file}");
        return 2;
    };
    use ed25519_dalek::Signer;
    let sig = signing.sign(&bytes);
    let sig_path = format!("{file}.sig");
    if let Err(e) = std::fs::write(&sig_path, b64(&sig.to_bytes()) + "\n") {
        eprintln!("xtask: write {sig_path}: {e}");
        return 2;
    }
    println!("signed {file} -> {sig_path}");
    0
}

/// `xtask release`: musl build -> tar.gz -> signature -> print artifacts.
fn release() -> i32 {
    let target = "x86_64-unknown-linux-musl";
    let code = run(Command::new(cargo_bin_dir().join("cargo"))
        .args(["build", "-p", "ka-harness", "--release", "--target", target])
        .current_dir(repo_root()));
    if code != 0 {
        return code;
    }
    let bin = repo_root()
        .join("target")
        .join(target)
        .join("release")
        .join("ka");
    let artifact = format!("ka-{target}.tar.gz");
    let code = run(Command::new("tar")
        .args([
            "czf",
            &artifact,
            "-C",
            bin.parent().and_then(Path::to_str).unwrap_or("."),
            "ka",
        ])
        .current_dir(repo_root()));
    if code != 0 {
        return code;
    }
    let code = sign(std::slice::from_ref(&artifact));
    if code != 0 {
        return code;
    }
    println!("artifacts: {artifact} ({artifact}.sig)");
    println!(
        "CI release job embeds KA_PUBKEY into the binary so `ka update` verifies {artifact}.sig"
    );
    0
}

/// One measured perf baseline.
#[derive(serde::Serialize, serde::Deserialize)]
struct Baselines {
    /// p50 of 5 `ka --version` runs, microseconds (fresh process each).
    version_p50_us: u64,
    /// Peak RSS of one `ka --version` run in KB (None: no GNU time).
    peak_rss_kb: Option<u64>,
}

/// µs timer around one fresh-process run; returns (duration µs, peak RSS KB).
fn time_run(bin: &str) -> (u64, Option<u64>) {
    let start = std::time::Instant::now();
    let output = Command::new("/usr/bin/time")
        .args(["-v", bin, "--version"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let elapsed_us = start.elapsed().as_micros() as u64;
    let rss = output.ok().and_then(|o| {
        String::from_utf8_lossy(&o.stderr)
            .lines()
            .find_map(|l| l.trim().strip_prefix("Maximum resident set size (kbytes):"))
            .and_then(|v| v.trim().parse::<u64>().ok())
    });
    (elapsed_us, rss)
}

/// ±30% regression gate: only upward drift fails.
fn within_tolerance(baseline: u64, current: u64) -> bool {
    current <= baseline.saturating_add(baseline * 30 / 100)
}

/// `xtask bench [--update] [--bin <path>]`: measure and gate.
fn bench(rest: &[String]) -> i32 {
    let update = rest.iter().any(|a| a == "--update");
    let bin = rest
        .iter()
        .position(|a| a == "--bin")
        .and_then(|i| rest.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "target/release/ka".to_string());

    let bin_path = repo_root().join(&bin);
    if !bin_path.exists() {
        eprintln!(
            "xtask: {} missing — build it first (cargo build --release -p ka-harness or --bin <path>)",
            bin_path.display()
        );
        return 2;
    }

    // 5 fresh runs, p50 (sorted[2]); RSS from the median run
    let mut runs: Vec<(u64, Option<u64>)> = (0..5).map(|_| time_run(&bin)).collect();
    runs.sort_by_key(|(us, _)| *us);
    let (p50_us, rss) = runs[2];

    let baseline_path = repo_root().join("baselines.json");
    if update || !baseline_path.exists() {
        let b = Baselines {
            version_p50_us: p50_us,
            peak_rss_kb: rss,
        };
        let json = serde_json::to_string_pretty(&b).unwrap_or_else(|_| "{}".into());
        if let Err(e) = std::fs::write(&baseline_path, json + "\n") {
            eprintln!("xtask: write {}: {e}", baseline_path.display());
            return 2;
        }
        println!(
            "baseline written: p50 {p50_us}µs, rss {}",
            rss.map(|k| format!("{k} KB"))
                .unwrap_or_else(|| "n/a".into())
        );
        return 0;
    }

    let parsed: Baselines =
        match serde_json::from_str(&std::fs::read_to_string(&baseline_path).unwrap_or_default()) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("xtask: parse {}: {e}", baseline_path.display());
                return 2;
            }
        };

    let mut failed = false;
    if !within_tolerance(parsed.version_p50_us, p50_us) {
        failed = true;
    }
    println!(
        "version p50: {p50_us}µs (baseline {}µs) {}",
        parsed.version_p50_us,
        if within_tolerance(parsed.version_p50_us, p50_us) {
            "ok"
        } else {
            "REGRESSION"
        }
    );
    match (parsed.peak_rss_kb, rss) {
        (Some(base), Some(cur)) => {
            let ok = within_tolerance(base, cur);
            failed |= !ok;
            println!(
                "peak rss: {cur} KB (baseline {base} KB) {}",
                if ok { "ok" } else { "REGRESSION" }
            );
        }
        (base, cur) => {
            println!(
                "peak rss: {} (baseline {}) — skipped",
                cur.map(|k| format!("{k} KB"))
                    .unwrap_or_else(|| "n/a".into()),
                base.map(|k| format!("{k} KB"))
                    .unwrap_or_else(|| "n/a".into()),
            );
        }
    }
    if failed {
        eprintln!("xtask: perf regression vs baselines.json (investigate or --update)");
        return 1;
    }
    0
}

/// One live-smoke scenario.
struct Smoke {
    name: &'static str,
    model: String,
    prompt: &'static str,
    /// Substrings the NDJSON stream must contain (text markers).
    expect_text: &'static str,
    /// Whether a tool call must round-trip.
    expect_tool: bool,
    /// Extra `--dialects` overlay contents (local servers).
    overlay: Option<String>,
}

/// `xtask live-smoke`: scripted one-turn conversations through `ka run`.
/// Requires ANTHROPIC_API_KEY / OPENAI_API_KEY / KA_LM_URL; refuses to
/// run in CI. Exit 1 on the first failure, printing the NDJSON tail.
fn live_smoke() -> i32 {
    if env::var("CI").is_ok() {
        eprintln!("xtask: live-smoke never runs in CI");
        return 2;
    }
    let mut scenarios: Vec<Smoke> = Vec::new();
    if env::var("ANTHROPIC_API_KEY").is_ok_and(|v| !v.is_empty()) {
        scenarios.push(Smoke {
            name: "anthropic-text",
            model: "anthropic/claude-sonnet-5".into(),
            prompt: "Reply with exactly SMOKE-OK and nothing else.",
            expect_text: "SMOKE-OK",
            expect_tool: false,
            overlay: None,
        });
        scenarios.push(Smoke {
            name: "anthropic-tool",
            model: "anthropic/claude-sonnet-5".into(),
            prompt: "Use the read tool on Cargo.toml, then reply with exactly SMOKE-READ.",
            expect_text: "SMOKE-READ",
            expect_tool: true,
            overlay: None,
        });
    }
    if env::var("OPENAI_API_KEY").is_ok_and(|v| !v.is_empty()) {
        scenarios.push(Smoke {
            name: "openai-text",
            model: "openai/gpt-5.1".into(),
            prompt: "Reply with exactly SMOKE-OK and nothing else.",
            expect_text: "SMOKE-OK",
            expect_tool: false,
            overlay: None,
        });
    }
    if let Ok(base) = env::var("KA_LM_URL") {
        let base = base.trim_end_matches('/').to_string();
        scenarios.push(Smoke {
            name: "local-text",
            model: "local/smoke".into(),
            prompt: "Reply with exactly SMOKE-OK and nothing else.",
            expect_text: "SMOKE-OK",
            expect_tool: false,
            overlay: Some(format!(
                "[dialects.\"local/smoke\"]\nwire = \"openai_chat\"\nbase_url = \"{base}/v1\"\ncontext = 32768\n"
            )),
        });
    }
    if scenarios.is_empty() {
        eprintln!(
            "xtask: live-smoke needs ANTHROPIC_API_KEY, OPENAI_API_KEY or KA_LM_URL in the environment"
        );
        return 2;
    }

    let root = repo_root();
    let ka_bin = root.join("target/release/ka");
    if !ka_bin.exists() {
        eprintln!("xtask: build the release binary first (target/release/ka)");
        return 2;
    }

    for smoke in &scenarios {
        println!("── {} ({})…", smoke.name, smoke.model);
        let mut overlay_path = None;
        let mut cmd = Command::new(&ka_bin);
        cmd.arg("run")
            .arg("--model")
            .arg(&smoke.model)
            .arg(smoke.prompt);
        if let Some(overlay) = &smoke.overlay {
            let path = root.join("target/live-smoke-dialects.toml");
            if std::fs::write(&path, overlay).is_err() {
                eprintln!("xtask: cannot write overlay");
                return 2;
            }
            overlay_path = Some(path.clone());
            cmd.arg("--dialects").arg(path);
        }
        let output = cmd.current_dir(&root).output();
        let lines: Vec<String> = match output {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_string)
                .collect(),
            Err(e) => {
                eprintln!("FAIL {}: spawn ka: {e}", smoke.name);
                return 1;
            }
        };
        let joined = lines.join("\n");
        let mut problems: Vec<String> = Vec::new();
        if !joined.contains(smoke.expect_text) {
            problems.push(format!("missing text {:?} in stream", smoke.expect_text));
        }
        if smoke.expect_tool
            && !joined.contains("\"call_started\"")
            && !joined.contains("\"call_output\"")
        {
            problems.push("no tool call round-trip in stream".to_string());
        }
        if !joined.contains("\"turn_finished\"") {
            problems.push("stream never finished (no turn_finished event)".to_string());
        }
        match problems.is_empty() {
            true => println!("   ok"),
            false => {
                eprintln!("FAIL {}: {}", smoke.name, problems.join("; "));
                let tail: Vec<String> = lines.iter().rev().take(20).rev().cloned().collect();
                eprintln!(
                    "--- NDJSON tail ---\n{}\n-------------------",
                    tail.join("\n")
                );
                return 1;
            }
        }
        if let Some(path) = overlay_path {
            let _ = fs::remove_file(path);
        }
    }
    println!("live-smoke: all scenarios passed");
    0
}

/// Regenerate `crates/ka-dialect/models-dev.toml` from https://models.dev.
/// Keeps curated `dialects.toml` selectors untouched (their flags and effort
/// budgets win); adds every tool-capable model on a static OpenAI-compatible
/// or Anthropic-compatible endpoint, with real pricing when published.
fn models_sync() -> i32 {
    let url = "https://models.dev/api.json";
    eprintln!("models-sync: fetching {url}");
    let out = Command::new("curl")
        .args(["-sSL", "--max-time", "60", url])
        .output();
    let bytes = match out {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => {
            eprintln!("models-sync: curl failed: {}", o.status);
            return 1;
        }
        Err(e) => {
            eprintln!("models-sync: curl not available: {e}");
            return 1;
        }
    };
    let root: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("models-sync: bad json: {e}");
            return 1;
        }
    };

    // curated selectors stay authoritative
    let dialects_path = repo_root().join("crates/ka-dialect/dialects.toml");
    let curated = fs::read_to_string(&dialects_path).unwrap_or_default();
    let mut curated_ids: Vec<String> = Vec::new();
    for line in curated.lines() {
        if let Some(rest) = line.strip_prefix("[dialects.\"") {
            if let Some(id) = rest.strip_suffix("\"]") {
                curated_ids.push(id.to_string());
            }
        }
    }

    let mut toml = String::new();
    toml.push_str("# ka dialect catalog — generated from https://models.dev (api.json).\n");
    toml.push_str("# Regenerate with: cargo xtask models-sync\n");
    toml.push_str("# Curated rows in dialects.toml win for the same selector; this file only\n");
    toml.push_str("# adds providers and models (real pricing where published, subscription\n");
    toml.push_str("# plans carry priced = false so costs are never fabricated).\n\n");

    let empty = serde_json::Map::new();
    let mut providers_written = 0usize;
    let mut rows_written = 0usize;
    let Some(providers) = root.as_object() else {
        eprintln!("models-sync: unexpected root shape");
        return 1;
    };
    let mut pids: Vec<&String> = providers.keys().collect();
    pids.sort();
    for pid in pids {
        let p = providers[pid].as_object().unwrap_or(&empty);
        let api = p.get("api").and_then(|v| v.as_str()).unwrap_or("");
        let env = p
            .get("env")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if api.is_empty() || env.is_empty() {
            continue; // needs dynamic config (vertex etc.) or keyless
        }
        let wire = if api.contains("/anthropic") {
            "anthropic_messages"
        } else {
            "openai_chat"
        };
        let Some(models) = p.get("models").and_then(|m| m.as_object()) else {
            continue;
        };
        // marketplaces with hundreds of models drown the picker; keep
        // first-party vendors, subscription plans, and small specialists.
        // Giant aggregators stay reachable through custom selectors.
        // policy: OFFICIAL vendor APIs and subscription plans only, plus a
        // few established routers. Western first-parties (openai, anthropic,
        // google, ...) publish no static endpoint here and live in the
        // curated dialects.toml instead. Everything else stays reachable
        // through custom `--dialects` overlays.
        const FIRST_PARTY: &[&str] = &[
            "deepseek",
            "zai",
            "zhipuai",
            "alibaba",
            "meta",
            "minimax",
            "modelscope",
            "opencode-go",
        ];
        const ROUTERS: &[&str] = &["openrouter", "huggingface", "nvidia"];
        let is_plan = pid.contains("plan");
        let is_first_party = FIRST_PARTY.contains(&pid.as_str());
        let is_router = ROUTERS.contains(&pid.as_str());
        if !is_plan && !is_first_party && !is_router {
            continue;
        }
        let mut mids: Vec<&String> = models.keys().collect();
        mids.sort();
        let mut wrote_for_provider = false;
        for mid in mids {
            let m = models[mid].as_object().unwrap_or(&empty);
            if !m
                .get("tool_call")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                continue;
            }
            if is_router
                && !m
                    .get("reasoning")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            {
                continue;
            }
            let selector = format!("{pid}/{mid}");
            if curated_ids.contains(&selector) {
                continue;
            }
            let limit = m.get("limit").and_then(|v| v.as_object()).unwrap_or(&empty);
            let context = limit
                .get("context")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32;
            let max_output = limit
                .get("output")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32;
            let cost = m.get("cost").and_then(|v| v.as_object()).unwrap_or(&empty);
            let pin = cost.get("input").and_then(|v| v.as_f64());
            let pout = cost.get("output").and_then(|v| v.as_f64());
            // subscription plans publish 0/0 token costs — that is not
            // per-token pricing, keep them unpriced and badge them as plans
            let priced = pin.is_some()
                && pout.is_some()
                && (pin.unwrap_or(0.0) > 0.0 || pout.unwrap_or(0.0) > 0.0);
            toml.push_str(&format!("[dialects.\"{selector}\"]\n"));
            toml.push_str(&format!("wire = \"{wire}\"\n"));
            toml.push_str(&format!("base_url = \"{api}\"\n"));
            toml.push_str(&format!("api_key_env = \"{env}\"\n"));
            if let Some(doc) = p.get("doc").and_then(|v| v.as_str()) {
                toml.push_str(&format!("doc_url = \"{doc}\"\n"));
            };
            toml.push_str(&format!("context = {context}\n"));
            if max_output > 0 {
                toml.push_str(&format!("max_output = {max_output}\n"));
            }
            toml.push_str(&format!("priced = {priced}\n"));
            if priced {
                toml.push_str(&format!(
                    "[dialects.\"{selector}\".price]\ninput_per_mtok = {}\noutput_per_mtok = {}\n",
                    pin.unwrap_or_default(),
                    pout.unwrap_or_default()
                ));
            }
            toml.push('\n');
            rows_written += 1;
            wrote_for_provider = true;
        }
        if wrote_for_provider {
            providers_written += 1;
        }
    }

    let out_path = repo_root().join("crates/ka-dialect/models-dev.toml");
    if let Err(e) = fs::write(&out_path, &toml) {
        eprintln!("models-sync: write failed: {e}");
        return 1;
    }
    eprintln!(
        "models-sync: {} providers, {} models, {} bytes -> {}",
        providers_written,
        rows_written,
        toml.len(),
        out_path.display()
    );
    0
}

#[cfg(test)]
mod tests {
    #[test]
    fn tolerance_gates_upward_drift_only() {
        use super::within_tolerance;
        assert!(within_tolerance(1000, 1000));
        assert!(within_tolerance(1000, 1299));
        assert!(within_tolerance(1000, 1300));
        assert!(!within_tolerance(1000, 1301));
        // improvements always pass
        assert!(within_tolerance(1000, 100));
    }
}
