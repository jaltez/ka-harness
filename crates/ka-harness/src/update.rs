//! `ka update`: self-update against GitHub releases. Signed release
//! builds verify every download against the embedded ed25519 public key
//! (see `cargo xtask sign`); unsigned (e.g. cargo-installed) builds fall
//! back to the CI-published `.sha256` checksum. Either way the artifact
//! is verified before the atomic binary swap.
//!
//! Layout: the CI release pipeline tags `vX.Y.Z` (assets
//! `ka-<triple>.tar.gz` + `.sig` + `.sha256`); the legacy
//! `ka-<channel>-*` tag shape still matches for hand-made releases.
//! `ka-<target-triple>.tar.gz` (+ `.sig`). Everything is parameterized
//! so tests can drive a local fixture server and swap a temp file.

use std::path::{Path, PathBuf};
use std::process::Command;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

/// Placeholder repo (config `[update] repo` overrides; anything else
/// fails with the configured name in the error — no guessing).
pub const DEFAULT_REPO: &str = "jaltez/ka-harness";
const DEFAULT_API: &str = "https://api.github.com";

/// The update result summary, ready to print.
#[derive(Debug)]
pub struct UpdateOutcome {
    /// Version before the update.
    pub old_version: String,
    /// New version after the swap (None = nothing installed).
    pub new_version: Option<String>,
    /// The release tag that was found/installed.
    pub latest_tag: String,
    /// True when the newest release is not newer than the running
    /// version (nothing to do — no download, no reinstall).
    pub up_to_date: bool,
}

/// CLI entry: resolve defaults from env/config, then run.
pub async fn run(channel: &str, check_only: bool, repo: &str) -> Result<String, String> {
    let api_base = std::env::var("KA_UPDATE_API_BASE").unwrap_or_else(|_| DEFAULT_API.to_string());
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let outcome = check_and_install(
        &api_base,
        repo,
        channel,
        crate::PUBLIC_KEY,
        &exe,
        check_only,
    )
    .await?;
    if outcome.up_to_date {
        return Ok(format!(
            "up to date: {} (latest release: {})",
            outcome.old_version, outcome.latest_tag
        ));
    }
    match outcome.new_version {
        Some(new) => Ok(format!(
            "updated: {} → {new} ({})",
            outcome.old_version, outcome.latest_tag
        )),
        None => Ok(format!(
            "current {} — latest release: {}",
            outcome.old_version, outcome.latest_tag
        )),
    }
}

/// Parse a `(major, minor, patch)` triple out of a release tag
/// (`v0.2.0`, `v0.2.0-edge`, `ka-stable-1.2.3`) or the running version
/// string (`0.1.0 (hash)`).
fn version_triple(s: &str) -> Option<(u64, u64, u64)> {
    let token = s.split([' ', '-']).find(|t| {
        let digits = t.strip_prefix('v').unwrap_or(t);
        digits.chars().next().is_some_and(|c| c.is_ascii_digit())
    })?;
    let digits = token.strip_prefix('v').unwrap_or(token);
    let mut it = digits.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().unwrap_or("0").parse().ok()?;
    let patch = it.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// XDG state dir for update check cache (`~/.local/state/ka/update`).
/// Refuses to silently fall back to a world-writable temp directory.
fn state_dir() -> Result<PathBuf, String> {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|base| base.join("ka/update"))
        .map_err(|_| {
            "cannot determine update state dir: neither XDG_STATE_HOME nor HOME is set".to_string()
        })
}

/// The release triple this binary updates for.
pub fn artifact_triple() -> String {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => "x86_64-unknown-linux-musl".to_string(),
        ("aarch64", "linux") => "aarch64-unknown-linux-musl".to_string(),
        ("x86_64", "macos") => "x86_64-apple-darwin".to_string(),
        ("aarch64", "macos") => "aarch64-apple-darwin".to_string(),
        (arch, os) => format!("{arch}-unknown-{os}"),
    }
}

/// The running binary's version string.
fn current_version() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            let out = Command::new(exe).arg("--version").output().ok()?;
            if !out.status.success() {
                return None;
            }
            let text = String::from_utf8(out.stdout).ok()?;
            let trimmed = text.trim().to_string();
            (!trimmed.is_empty()).then_some(trimmed)
        })
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

/// Maximum compressed artifact download size (32 MiB).
const MAX_DOWNLOAD_BYTES: usize = 32 * 1024 * 1024;

/// Maximum uncompressed tar size (64 MiB — gzip bomb guard).
const MAX_UNCOMPRESSED_TAR: usize = 64 * 1024 * 1024;

fn is_loopback_http(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "http" {
        return false;
    }
    // `http://127.0.0.1:80@evil.example/` parses as a userinfo URL, not
    // a loopback one — reject anything carrying credentials
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host == "127.0.0.1" || host == "::1"
}

/// Client with the update transport policy: every redirect hop must
/// still pass [`validate_update_url`] (HTTPS, or loopback only when the
/// fixture base is loopback), so an origin that 302s to plaintext
/// cannot downgrade the transport.
fn update_client(timeout: std::time::Duration, api_base: &str) -> Result<reqwest::Client, String> {
    let base = api_base.to_string();
    reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() > 5 {
                return attempt.error("too many redirects");
            }
            match validate_update_url(attempt.url().as_str(), &base) {
                Ok(()) => attempt.follow(),
                Err(e) => attempt.error(e),
            }
        }))
        .build()
        .map_err(|e| format!("client: {e}"))
}

/// Enforce HTTPS on all update URLs, allowing loopback HTTP only when
/// `api_base` itself is a loopback test fixture.
fn validate_update_url(url: &str, api_base: &str) -> Result<(), String> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if is_loopback_http(api_base) && is_loopback_http(url) {
        return Ok(());
    }
    Err(format!("refusing non-HTTPS update URL: {url}"))
}

/// One network hit per 24 h — the check state file is the rate
/// limiter (and the offline answer: a fresh cache needs no network).
const CHECK_EVERY_SECS: u64 = 24 * 60 * 60;

/// The startup check's cache (`<state>/check.json`).
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct CheckState {
    /// Unix seconds of the last network check.
    last_check: u64,
    /// Latest release tag as of that check.
    latest: String,
}

fn read_check_state(state_dir: &Path) -> Option<CheckState> {
    std::fs::read_to_string(state_dir.join("check.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn write_check_state(state_dir: &Path, state: &CheckState) {
    // best effort: a failed cache write only means one extra check later
    let _ = std::fs::create_dir_all(state_dir);
    if let Ok(json) = serde_json::to_string(state) {
        let _ = std::fs::write(state_dir.join("check.json"), json);
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether `tag` is strictly newer than the running `current` version
/// (hand-rolled triple compare, same as the installer; unparseable
/// versions read as up-to-date — quiet beats a nagging badge).
fn newer_release(tag: &str, current: &str) -> bool {
    match (version_triple(tag), version_triple(current)) {
        (Some(new), Some(cur)) => new > cur,
        _ => false,
    }
}

/// Latest `v*` (or legacy `ka-<channel>-*`) release tag. Short-capped
/// client: a startup check must never linger on a slow network.
async fn latest_release(api_base: &str, repo: &str, channel: &str) -> Result<String, String> {
    validate_update_url(api_base, api_base)?;
    let client = update_client(std::time::Duration::from_secs(5), api_base)?;
    let url = format!("{api_base}/repos/{repo}/releases");
    let releases: serde_json::Value = client
        .get(&url)
        .header("User-Agent", "ka-update")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("GET {url}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("releases JSON: {e}"))?;
    let legacy_prefix = format!("ka-{channel}");
    releases
        .as_array()
        .and_then(|list| {
            list.iter().find(|r| {
                r["tag_name"].as_str().is_some_and(|t| {
                    // CI tags `vX.Y.Z`; the legacy `ka-<channel>-*`
                    // shape still matches (same pick as check_and_install)
                    (t.starts_with('v') && version_triple(t).is_some())
                        || t.starts_with(&legacy_prefix)
                })
            })
        })
        .and_then(|r| r["tag_name"].as_str().map(str::to_string))
        .ok_or_else(|| format!("no release tagged v* (or {legacy_prefix:?}) in {repo}"))
}

/// The TUI startup check: is a newer release out? Checks, never
/// installs (`ka update` stays the only install path). Rate-limited to
/// one network hit per 24 h via the state file — a fresh cache answers
/// offline. Any failure is a quiet `None`: no badge, no nagging.
/// Returns `(current version, latest tag)`.
pub async fn startup_check(repo: &str, enabled: bool) -> Option<(String, String)> {
    if !enabled {
        return None;
    }
    let Ok(dir) = state_dir() else {
        return None;
    };
    let api_base = std::env::var("KA_UPDATE_API_BASE").unwrap_or_else(|_| DEFAULT_API.to_string());
    startup_check_in(&dir, &api_base, repo, enabled, &current_version()).await
}

/// Parameterized core (state dir + api base + version injectable for
/// tests: `current_version` re-exes the running binary, which is the
/// test harness there, not `ka --version`).
async fn startup_check_in(
    state_dir: &Path,
    api_base: &str,
    repo: &str,
    enabled: bool,
    current: &str,
) -> Option<(String, String)> {
    if !enabled {
        return None;
    }
    if let Some(state) = read_check_state(state_dir) {
        if now_unix().saturating_sub(state.last_check) < CHECK_EVERY_SECS {
            return newer_release(&state.latest, current)
                .then(|| (current.to_string(), state.latest));
        }
    }
    let latest = latest_release(api_base, repo, "stable").await.ok()?;
    write_check_state(
        state_dir,
        &CheckState {
            last_check: now_unix(),
            latest: latest.clone(),
        },
    );
    newer_release(&latest, current).then(|| (current.to_string(), latest))
}

/// Fetch, verify, and (unless `check_only`) atomically install.
pub async fn check_and_install(
    api_base: &str,
    repo: &str,
    channel: &str,
    pubkey: Option<&str>,
    exe: &Path,
    check_only: bool,
) -> Result<UpdateOutcome, String> {
    validate_update_url(api_base, api_base)?;
    let old_version = current_version();
    // unsigned builds (e.g. cargo install) verify the CI-published
    // sha256 instead of the ed25519 signature; `ka doctor` reports
    // which build identity is running
    let verifying = pubkey.map(verifying_key).transpose()?;

    let client = update_client(std::time::Duration::from_secs(60), api_base)?;
    let url = format!("{api_base}/repos/{repo}/releases");
    let releases: serde_json::Value = client
        .get(&url)
        .header("User-Agent", "ka-update")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("GET {url}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("releases JSON: {e}"))?;

    let legacy_prefix = format!("ka-{channel}");
    let release = releases
        .as_array()
        .and_then(|list| {
            list.iter().find(|r| {
                r["tag_name"].as_str().is_some_and(|t| {
                    // CI tags `vX.Y.Z` (version-shaped — a stray `v`
                    // prefix alone must not match); the legacy
                    // `ka-<channel>-*` shape still matches too.
                    (t.starts_with('v') && version_triple(t).is_some())
                        || t.starts_with(&legacy_prefix)
                })
            })
        })
        .ok_or_else(|| format!("no release tagged v* (or {legacy_prefix:?}) in {repo}"))?;
    let tag = release["tag_name"].as_str().unwrap_or_default().to_string();

    // Fail-closed downgrade protection: both the release tag and the
    // running version must parse as version triples before any download.
    let new_ver = version_triple(&tag)
        .ok_or_else(|| format!("refusing update: unparseable release tag {tag:?}"))?;
    let cur_ver = version_triple(&old_version)
        .ok_or_else(|| format!("refusing update: unparseable current version {old_version:?}"))?;
    if new_ver <= cur_ver {
        return Ok(UpdateOutcome {
            old_version,
            new_version: None,
            latest_tag: tag,
            up_to_date: true,
        });
    }

    let asset_name = format!("ka-{}.tar.gz", artifact_triple());
    let asset_url = |name: &str| -> Result<String, String> {
        release["assets"]
            .as_array()
            .and_then(|assets| {
                assets.iter().find_map(|a| {
                    (a["name"].as_str() == Some(name))
                        .then(|| a["browser_download_url"].as_str().map(str::to_string))
                        .flatten()
                })
            })
            .ok_or_else(|| format!("release {tag}: asset {name:?} missing"))
    };
    let tar_url = asset_url(&asset_name)?;
    let sig_url = asset_url(&format!("{asset_name}.sig"))?;

    let download = |url: String| {
        let client = &client;
        async move {
            validate_update_url(&url, api_base)?;
            let mut resp = client
                .get(&url)
                .header("User-Agent", "ka-update")
                .send()
                .await
                .map_err(|e| format!("GET {url}: {e}"))?
                .error_for_status()
                .map_err(|e| format!("GET {url}: {e}"))?;
            if let Some(cl) = resp.content_length() {
                if cl > MAX_DOWNLOAD_BYTES as u64 {
                    return Err(format!(
                        "GET {url}: download size {cl} exceeds {} byte cap",
                        MAX_DOWNLOAD_BYTES
                    ));
                }
            }
            let mut buf = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(|e| format!("GET {url}: {e}"))? {
                if buf.len().saturating_add(chunk.len()) > MAX_DOWNLOAD_BYTES {
                    return Err(format!(
                        "GET {url}: download exceeds {} byte cap",
                        MAX_DOWNLOAD_BYTES
                    ));
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(buf)
        }
    };
    let tarball = download(tar_url).await?;
    match &verifying {
        Some(key) => {
            let sig_bytes = download(sig_url).await?;
            let sig_b64 = String::from_utf8_lossy(&sig_bytes).into_owned();
            verify(&tarball, sig_b64.trim(), key)?;
        }
        None => {
            let sha_bytes = download(asset_url(&format!("{asset_name}.sha256"))?).await?;
            let sha_text = String::from_utf8_lossy(&sha_bytes).into_owned();
            verify_checksum(&tarball, &sha_text)?;
        }
    }

    if check_only {
        return Ok(UpdateOutcome {
            old_version,
            new_version: None,
            latest_tag: tag,
            up_to_date: false,
        });
    }

    // Pure-Rust tar.gz inspection & extraction: rejects tar-slip (`..`),
    // leading `/`, symlinks, hardlinks, devices, and gzip bombs without
    // ever shelling out to `tar`.
    let ka_bytes = extract_ka_from_tar_gz(&tarball)?;

    // Stage in `exe.parent()` so rename is on the same filesystem (EXDEV-safe).
    let stage_dir = exe.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(stage_dir)
        .map_err(|e| format!("mkdir {}: {e}", stage_dir.display()))?;
    let staged = stage_dir.join(format!(
        ".ka.update.{}.{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    {
        use std::io::Write as _;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o700);
        }
        let mut file = opts
            .open(&staged)
            .map_err(|e| format!("stage {}: {e}", staged.display()))?;
        if let Err(e) = file.write_all(&ka_bytes) {
            let _ = std::fs::remove_file(&staged);
            return Err(format!("stage {}: {e}", staged.display()));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)) {
            let _ = std::fs::remove_file(&staged);
            return Err(format!("chmod: {e}"));
        }
    }

    // atomic rename over the running binary; a busy executable (ETXTBSY)
    // lands the new binary next to the old one with instructions
    match std::fs::rename(&staged, exe) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(26) => {
            let beside = exe.with_file_name("ka.new");
            if let Err(re) = std::fs::rename(&staged, &beside) {
                let _ = std::fs::remove_file(&staged);
                return Err(format!("stage {}: {re}", beside.display()));
            }
            return Ok(UpdateOutcome {
                old_version,
                new_version: Some(format!(
                    "STAGED at {} — busy executable; swap manually: mv {} {}",
                    beside.display(),
                    beside.display(),
                    exe.display()
                )),
                latest_tag: tag,
                up_to_date: false,
            });
        }
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            return Err(format!("install: {e}"));
        }
    }

    let new_version = Command::new(exe)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| tag.clone());
    Ok(UpdateOutcome {
        old_version,
        new_version: Some(new_version),
        latest_tag: tag,
        up_to_date: false,
    })
}

/// Inspect every entry of a `.tar.gz` archive in pure Rust and return the
/// bytes of the single regular `ka` binary entry. Rejects path traversal
/// (`..`), absolute paths, symlinks, hardlinks, device nodes, and gzip
/// bombs.
fn extract_ka_from_tar_gz(tarball: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let decoder = flate2::read::GzDecoder::new(tarball);
    let mut raw = Vec::new();
    decoder
        .take((MAX_UNCOMPRESSED_TAR as u64) + 1)
        .read_to_end(&mut raw)
        .map_err(|e| format!("tarball gunzip failed: {e}"))?;
    if raw.len() > MAX_UNCOMPRESSED_TAR {
        return Err(format!(
            "tarball exceeds {} byte uncompressed cap",
            MAX_UNCOMPRESSED_TAR
        ));
    }

    let mut offset = 0usize;
    let mut ka_bytes: Option<Vec<u8>> = None;
    while offset + 512 <= raw.len() {
        let header = &raw[offset..offset + 512];
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let typeflag = header[156];
        let size = parse_tar_size(&header[124..136])?;
        let data_start = offset + 512;
        let data_end = data_start
            .checked_add(size)
            .ok_or_else(|| "tarball: entry size overflow".to_string())?;
        if data_end > raw.len() {
            return Err("tarball: truncated entry".to_string());
        }
        let padded = size
            .checked_add(511)
            .map(|n| (n / 512) * 512)
            .ok_or_else(|| "tarball: entry size overflow".to_string())?;
        let next_offset = data_start
            .checked_add(padded)
            .ok_or_else(|| "tarball: offset overflow".to_string())?;

        // PAX extended headers (`x` / `g`) carry metadata only; skip them.
        if typeflag == b'x' || typeflag == b'g' {
            offset = next_offset;
            continue;
        }

        // Only regular files (`0` / `\0`) and directories (`5`) are permitted.
        if !matches!(typeflag, 0 | b'0' | b'5') {
            return Err(format!(
                "tarball: refusing non-regular entry type {:?}",
                typeflag as char
            ));
        }

        let name = tar_cstr(&header[0..100])?;
        let prefix = if &header[257..262] == b"ustar" {
            tar_cstr(&header[345..500])?
        } else {
            String::new()
        };
        let full_path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if full_path.is_empty()
            || full_path.starts_with('/')
            || full_path.starts_with('\\')
            || full_path.contains('\0')
            || full_path.split(['/', '\\']).any(|seg| seg == "..")
        {
            return Err(format!("tarball: refusing unsafe entry path {full_path:?}"));
        }

        let mut normalized = full_path.as_str();
        while let Some(stripped) = normalized.strip_prefix("./") {
            normalized = stripped;
        }
        if normalized == "ka" && matches!(typeflag, 0 | b'0') {
            if ka_bytes.is_some() {
                return Err("tarball: duplicate 'ka' entry".to_string());
            }
            ka_bytes = Some(raw[data_start..data_end].to_vec());
        }

        offset = next_offset;
    }

    ka_bytes.ok_or_else(|| "tarball: no 'ka' entry".to_string())
}

fn tar_cstr(field: &[u8]) -> Result<String, String> {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    std::str::from_utf8(&field[..end])
        .map(str::to_string)
        .map_err(|_| "tarball: non-UTF-8 entry name".to_string())
}

fn parse_tar_size(field: &[u8]) -> Result<usize, String> {
    if field.first().copied() == Some(0x80) {
        let mut val: u64 = 0;
        for &b in &field[1..] {
            val = val
                .checked_shl(8)
                .ok_or_else(|| "tarball: size overflow".to_string())?
                | u64::from(b);
        }
        return usize::try_from(val).map_err(|_| "tarball: size overflow".to_string());
    }
    let s = std::str::from_utf8(field)
        .map_err(|_| "tarball: invalid size header".to_string())?
        .trim_matches(|c: char| c == '\0' || c.is_ascii_whitespace());
    if s.is_empty() {
        return Ok(0);
    }
    usize::from_str_radix(s, 8).map_err(|_| format!("tarball: invalid octal size {s:?}"))
}

/// Verify tarball bytes against a base64 ed25519 signature.
fn verify(payload: &[u8], sig_b64: &str, verifying: &VerifyingKey) -> Result<(), String> {
    let sig_bytes: [u8; 64] = unb64(sig_b64)
        .ok_or_else(|| "signature: not base64".to_string())?
        .try_into()
        .map_err(|_| "signature: wrong length".to_string())?;
    verifying
        .verify(payload, &Signature::from_bytes(&sig_bytes))
        .map_err(|_| "signature verification FAILED — artifact tampered or wrong key")?;
    Ok(())
}

/// Verify tarball bytes against a CI-published `sha256sum` line
/// (hex digest as the first whitespace token).
fn verify_checksum(payload: &[u8], sha_line: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let expected = sha_line
        .split_whitespace()
        .next()
        .ok_or_else(|| "checksum asset: empty".to_string())?;
    let mut hasher = Sha256::new();
    hasher.update(payload);
    let got = format!("{:x}", hasher.finalize());
    if got != expected.to_ascii_lowercase() {
        return Err(format!(
            "update checksum mismatch: expected {expected}, got {got}"
        ));
    }
    Ok(())
}

fn verifying_key(pubkey_b64: &str) -> Result<VerifyingKey, String> {
    let bytes: [u8; 32] = unb64(pubkey_b64)
        .ok_or_else(|| "KA_PUBKEY: not base64".to_string())?
        .try_into()
        .map_err(|_| "KA_PUBKEY: wrong length".to_string())?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| format!("KA_PUBKEY: {e}"))
}

/// Base64 decode (whitespace and '=' tolerated).
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use ed25519_dalek::SigningKey;

    const SEED: [u8; 32] = [7u8; 32];
    const TARGET: &str = "x86_64-unknown-linux-musl";

    fn b64(data: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
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

    /// Serve a releases list plus tarball/signature assets.
    async fn fixture_server(
        listener: tokio::net::TcpListener,
        server_addr: std::net::SocketAddr,
        tarball: Vec<u8>,
        sig: String,
        sha: String,
        wrong_sig: bool,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                match sock.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            let req = String::from_utf8_lossy(&buf).into_owned();
            let body: Vec<u8> = if req.starts_with("GET /repos/owner/ka/releases") {
                let releases = serde_json::json!([
                    {"tag_name": "v-unrelated", "assets": []},
                    {"tag_name": "ka-stable-9.9.9", "assets": [
                        {"name": format!("ka-{TARGET}.tar.gz"),
                         "browser_download_url": format!("http://{server_addr}/artifact")},
                        {"name": format!("ka-{TARGET}.tar.gz.sig"),
                         "browser_download_url": format!("http://{server_addr}/signature")},
                        {"name": format!("ka-{TARGET}.tar.gz.sha256"),
                         "browser_download_url": format!("http://{server_addr}/checksum")}
                    ]}
                ]);
                releases.to_string().into_bytes()
            } else if req.starts_with("GET /artifact") {
                tarball.clone()
            } else if req.starts_with("GET /signature") {
                if wrong_sig {
                    format!("{}==", "A".repeat(86)).into_bytes()
                } else {
                    format!("{sig}\n").into_bytes()
                }
            } else if req.starts_with("GET /checksum") {
                format!("{sha}\n").into_bytes()
            } else {
                b"not found".to_vec()
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.ok();
            sock.write_all(&body).await.ok();
            sock.shutdown().await.ok();
        }
    }

    fn make_tarball(dir: &Path, marker: &str) -> Vec<u8> {
        let payload = dir.join("ka");
        std::fs::write(&payload, marker).unwrap();
        let tar_path = dir.join("ka.tar.gz");
        let status = Command::new("tar")
            .args([
                "czf",
                tar_path.to_string_lossy().as_ref(),
                "-C",
                dir.to_string_lossy().as_ref(),
                "ka",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::read(&tar_path).unwrap()
    }

    /// Serve a bare releases list (one `v9.9.9` tag), counting hits.
    async fn releases_counter(
        listener: tokio::net::TcpListener,
        hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::Ordering;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                match sock.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            hits.fetch_add(1, Ordering::SeqCst);
            let releases = serde_json::json!([{"tag_name": "v-unrelated", "assets": []}, {"tag_name": "v9.9.9", "assets": []}]);
            let body = releases.to_string().into_bytes();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.ok();
            sock.write_all(&body).await.ok();
        }
    }

    /// The TUI startup check: a newer release is reported once, the
    /// state file rate-limits to one network hit per 24 h, an
    /// up-to-date or disabled or unreachable check stays quiet.
    #[tokio::test]
    async fn startup_check_reports_newer_and_rate_limits() {
        let work = std::env::temp_dir().join(format!("ka-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        let state = work.join("state");
        use std::sync::atomic::Ordering;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tokio::spawn(releases_counter(listener, hits.clone()));
        let api = format!("http://{addr}");

        // stale cache (25 h old) → one network hit, notice, fresh state
        write_check_state(
            &state,
            &CheckState {
                last_check: now_unix() - 25 * 60 * 60,
                latest: "v0.0.1".into(),
            },
        );
        let got = startup_check_in(&state, &api, "owner/ka", true, "0.2.9").await;
        assert_eq!(
            got.as_ref().map(|(c, l)| (c.as_str(), l.as_str())),
            Some(("0.2.9", "v9.9.9")),
            "newer release reported with current + latest"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // fresh cache → answered offline, zero new requests
        let got = startup_check_in(&state, &api, "owner/ka", true, "0.2.9").await;
        assert!(got.is_some(), "cached verdict stands");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "fresh cache answers without network"
        );

        // fresh cache, up-to-date tag → quiet None
        write_check_state(
            &state,
            &CheckState {
                last_check: now_unix(),
                latest: "v0.2.9".into(),
            },
        );
        assert!(
            startup_check_in(&state, &api, "owner/ka", true, "0.2.9")
                .await
                .is_none()
        );

        // disabled → never asks
        assert!(
            startup_check_in(&state, &api, "owner/ka", false, "0.2.9")
                .await
                .is_none()
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // stale cache + dead network → quiet None, cache untouched
        write_check_state(
            &state,
            &CheckState {
                last_check: now_unix() - 25 * 60 * 60,
                latest: "v0.0.1".into(),
            },
        );
        assert!(
            startup_check_in(&state, "http://127.0.0.1:1", "owner/ka", true, "0.2.9")
                .await
                .is_none()
        );
        assert_eq!(
            read_check_state(&state).map(|s| s.latest),
            Some("v0.0.1".into()),
            "a failed check must not poison the cache"
        );

        let _ = std::fs::remove_dir_all(&work);
    }

    #[tokio::test]
    async fn valid_artifact_swaps_and_tampered_refuses() {
        let work = std::env::temp_dir().join(format!("ka-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();

        let signing = SigningKey::from_bytes(&SEED);
        let pubkey = b64(signing.verifying_key().as_bytes());

        let marker = "KA-BINARY-V999\n";
        let tarball = make_tarball(&work, marker);
        use ed25519_dalek::Signer;
        let sig = b64(&signing.sign(&tarball).to_bytes());

        // tampered case: right key, wrong payload bytes
        std::fs::create_dir_all(work.join("tampered")).unwrap();
        let tampered_tarball = make_tarball(&work.join("tampered"), "TAMPERED");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(fixture_server(
            listener,
            addr,
            tampered_tarball,
            sig.clone(),
            String::new(),
            false,
        ));

        let exe = work.join("installed-ka");
        std::fs::write(&exe, "OLD").unwrap();

        let err = check_and_install(
            &format!("http://{addr}"),
            "owner/ka",
            "stable",
            Some(&pubkey),
            &exe,
            false,
        )
        .await
        .unwrap_err();
        assert!(err.contains("tampered"), "{err}");
        assert_eq!(
            std::fs::read(&exe).unwrap(),
            b"OLD",
            "exe untouched on refusal"
        );

        // tampered-signature case: garbage sig
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(fixture_server(
            listener,
            addr,
            tarball.clone(),
            sig.clone(),
            String::new(),
            true,
        ));
        let err = check_and_install(
            &format!("http://{addr}"),
            "owner/ka",
            "stable",
            Some(&pubkey),
            &exe,
            false,
        )
        .await
        .unwrap_err();
        assert!(err.contains("FAILED"), "{err}");

        // valid case: full update swaps the temp "binary"
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(fixture_server(
            listener,
            addr,
            tarball.clone(),
            sig.clone(),
            String::new(),
            false,
        ));
        let outcome = check_and_install(
            &format!("http://{addr}"),
            "owner/ka",
            "stable",
            Some(&pubkey),
            &exe,
            false,
        )
        .await
        .unwrap();
        assert_eq!(outcome.latest_tag, "ka-stable-9.9.9");
        assert_eq!(std::fs::read(&exe).unwrap(), marker.as_bytes());
        // the fixture "binary" is a text file: --version fails and the
        // tag name is reported as the new version instead
        assert_eq!(outcome.new_version.as_deref(), Some("ka-stable-9.9.9"));

        // check-only: reports without touching the exe
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(fixture_server(
            listener,
            addr,
            tarball,
            sig,
            String::new(),
            false,
        ));
        let outcome = check_and_install(
            &format!("http://{addr}"),
            "owner/ka",
            "stable",
            Some(&pubkey),
            &exe,
            true,
        )
        .await
        .unwrap();
        assert!(outcome.new_version.is_none());
        assert_eq!(outcome.latest_tag, "ka-stable-9.9.9");

        let _ = std::fs::remove_dir_all(&work);
    }

    fn sha256_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(data);
        format!("{:x}", h.finalize())
    }

    /// Unsigned builds update via the published sha256: correct digest
    /// installs, wrong digest refuses without touching the exe.
    #[tokio::test]
    async fn unsigned_build_verifies_sha256() {
        let work = std::env::temp_dir().join(format!("ka-update-unsigned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();

        let marker = "KA-BINARY-V999-UNSIGNED\n";
        let tarball = make_tarball(&work, marker);
        let good_sha = sha256_hex(&tarball);

        let exe = work.join("installed-ka");
        std::fs::write(&exe, "OLD").unwrap();

        // wrong digest: refuse, exe untouched
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(fixture_server(
            listener,
            addr,
            tarball.clone(),
            String::new(),
            format!("{:0>64}", "0"),
            false,
        ));
        let err = check_and_install(
            &format!("http://{addr}"),
            "owner/ka",
            "stable",
            None,
            &exe,
            false,
        )
        .await
        .unwrap_err();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"OLD");

        // correct digest: installs
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(fixture_server(
            listener,
            addr,
            tarball,
            String::new(),
            good_sha,
            false,
        ));
        let outcome = check_and_install(
            &format!("http://{addr}"),
            "owner/ka",
            "stable",
            None,
            &exe,
            false,
        )
        .await
        .unwrap();
        assert_eq!(outcome.latest_tag, "ka-stable-9.9.9");
        assert_eq!(std::fs::read(&exe).unwrap(), marker.as_bytes());

        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn checksum_parses_first_token_case_insensitive() {
        assert!(verify_checksum(b"abc", &format!("{}\n", sha256_hex(b"abc"))).is_ok());
        assert!(
            verify_checksum(
                b"abc",
                &format!("  {}  extra", sha256_hex(b"abc").to_uppercase())
            )
            .is_ok()
        );
        assert!(verify_checksum(b"abc", "deadbeef").is_err());
        assert!(verify_checksum(b"abc", "").is_err());
    }
    #[test]
    fn default_repo_is_the_real_repository() {
        // regression: shipped 0.2.x pointed at kampka/ka and 404'd
        assert_eq!(DEFAULT_REPO, "jaltez/ka-harness");
    }

    #[test]
    fn version_triples_parse_all_release_shapes() {
        assert_eq!(version_triple("v0.2.0"), Some((0, 2, 0)));
        assert_eq!(version_triple("v0.2.0-edge"), Some((0, 2, 0)));
        assert_eq!(version_triple("ka-stable-1.2.3"), Some((1, 2, 3)));
        assert_eq!(version_triple("0.1.0 (2c1f5fb)"), Some((0, 1, 0)));
        assert_eq!(version_triple("10.0.2"), Some((10, 0, 2)));
        assert_eq!(version_triple("no-version-here"), None);
    }
}
