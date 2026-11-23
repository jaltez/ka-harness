//! `ka doctor`: environment health checks. Pass/fail table over the
//! build, config layers, provider keys, trust store, local data, and
//! (with `--net`) live provider + MCP reachability. Any failure exits 1.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ka_dialect::Catalog;
use ka_engine::config::Config;
/// One health check row.
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// Run all checks; print the table (or JSON) and return the exit code.
pub async fn run(net: bool, json: bool) -> Result<ExitCode, String> {
    let mut checks: Vec<Check> = Vec::new();

    // build identity: version + signing-pubkey presence (unsigned dev
    // builds are fine; the detail is what matters)
    checks.push(Check {
        name: "version",
        ok: true,
        detail: match crate::PUBLIC_KEY {
            Some(_) => format!("{} (signed build)", env!("KA_VERSION")),
            None => format!(
                "{} (unsigned build — `ka update` verifies sha256 only)",
                env!("KA_VERSION")
            ),
        },
    });

    // config layers: strict parse of every existing layer
    let (cfg, cfg_detail, cfg_ok) = match load_layers() {
        Ok((cfg, layers, ok)) => (cfg, layers, ok),
        Err(e) => (Config::default(), e, false),
    };
    checks.push(Check {
        name: "config",
        ok: cfg_ok,
        detail: cfg_detail,
    });

    // provider keys: every selector the config pins (model + roles) must
    // resolve to a dialect whose key is present
    checks.push(provider_keys_check(&cfg));

    // trust store: entry count
    checks.push(trust_check());

    // local data: strand count + spills size
    checks.push(local_data_check());

    // lsp: every configured language server command must exist on PATH
    checks.push(lsp_check(&cfg));

    // sandbox: which fs-mode enforcement engine is active (and whether
    // the configured mode has one at all)
    checks.push(sandbox_check(&cfg));

    if net {
        checks.push(provider_net_check().await);
        checks.push(mcp_net_check(&cfg).await);
    }

    if json {
        let rows: Vec<serde_json::Value> = checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "check": c.name,
                    "ok": c.ok,
                    "detail": c.detail,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(|e| format!("json: {e}"))?
        );
    } else {
        let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
        for c in &checks {
            let mark = if c.ok { "ok  " } else { "FAIL" };
            println!("{mark} {:<width$}  {}", c.name, c.detail, width = width);
        }
    }

    let failed = checks.iter().any(|c| !c.ok);
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Parse global + project layers, reporting every failure.
fn load_layers() -> Result<(Config, String, bool), String> {
    let mut layers: Vec<(String, PathBuf)> = vec![(
        "user".to_string(),
        dirs_next()
            .map(|d| d.join("config/ka/ka.toml"))
            .unwrap_or_else(|| PathBuf::from("~/.config/ka/ka.toml")),
    )];
    if let Ok(cwd) = std::env::current_dir() {
        layers.push((
            "project".to_string(),
            ka_engine::project_root(&cwd).join(".ka/ka.toml"),
        ));
    }
    let mut cfg = Config::default();
    let mut detail = String::new();
    let mut ok = true;
    for (name, path) in layers {
        match std::fs::read_to_string(&path) {
            Ok(text) => match Config::parse_layer(&text, &name) {
                Ok(layer) => {
                    cfg.overlay(layer);
                    detail.push_str(&format!("{name}:ok "));
                }
                Err(e) => {
                    ok = false;
                    detail.push_str(&format!("{name}:ERROR "));
                    detail.push_str(&e.to_string());
                }
            },
            Err(_) => detail.push_str(&format!("{name}:absent ")),
        }
    }
    Ok((cfg, detail.trim_end().to_string(), ok))
}

fn dirs_next() -> Option<PathBuf> {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok()
}

/// Every model selector the config pins must have its provider key.
fn provider_keys_check(cfg: &Config) -> Check {
    let catalog = Catalog::embedded();
    let mut selectors: Vec<&String> = Vec::new();
    if let Some(m) = &cfg.model {
        selectors.push(m);
    }
    if let Some(f) = &cfg.roles.fast {
        selectors.push(f);
    }
    if let Some(d) = &cfg.roles.default {
        selectors.push(d);
    }
    if selectors.is_empty() {
        return Check {
            name: "keys",
            ok: true,
            detail: "no model configured (nothing needs a key)".to_string(),
        };
    }
    let mut missing: Vec<String> = Vec::new();
    for sel in &selectors {
        let parsed = match ka_dialect::parse_selector(sel) {
            Ok(p) => p,
            Err(_) => {
                missing.push(format!("{sel} (invalid selector)"));
                continue;
            }
        };
        let model_id = parsed.model_id();
        let Some(dialect) = catalog.get(&model_id) else {
            missing.push(format!("{model_id} (not in catalog)"));
            continue;
        };
        let Some(env_var) = &dialect.api_key_env else {
            continue; // local/keyless model
        };
        if !ka_dialect::auth::key_is_set(env_var) {
            missing.push(format!("{model_id} needs ${env_var}"));
        }
    }
    if missing.is_empty() {
        Check {
            name: "keys",
            ok: true,
            detail: format!("all {} configured selector(s) have keys", selectors.len()),
        }
    } else {
        Check {
            name: "keys",
            ok: false,
            detail: format!("missing: {}", missing.join(", ")),
        }
    }
}

/// Trust store entry count.
fn trust_check() -> Check {
    let path = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("ka/trust.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let entries = text.matches("\"path\"").count();
            Check {
                name: "trust",
                ok: true,
                detail: format!("{} trusted path(s) at {}", entries, path.display()),
            }
        }
        Err(_) => Check {
            name: "trust",
            ok: true,
            detail: "no trust store yet (fresh install)".to_string(),
        },
    }
}

/// Strand count + spills disk usage.
fn local_data_check() -> Check {
    let data = ka_strand::data_dir();

    // strands live at <data>/strands/<encoded-cwd>/<id>.jsonl: count
    // the FILES one level down (the old read_dir counted project
    // directories and called them strands)
    let strands = data
        .join("strands")
        .read_dir()
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.path().read_dir().ok())
                .map(|files| files.filter_map(|f| f.ok()).count())
                .sum::<usize>()
        })
        .unwrap_or(0);
    let spills_bytes = dir_size(&data.join("spills"));
    Check {
        name: "data",
        ok: true,
        detail: format!(
            "{} strand file(s), {:.1} MB spills at {}",
            strands,
            spills_bytes as f64 / (1024.0 * 1024.0),
            data.display()
        ),
    }
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = path.read_dir() {
        for e in entries.filter_map(|e| e.ok()) {
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_file() {
                total += meta.len();
            } else if meta.is_dir() {
                total += dir_size(&e.path());
            }
        }
    }
    total
}

/// LSP health: when enabled, every configured language server command
/// must resolve on PATH (first whitespace token is the binary).
fn lsp_check(cfg: &Config) -> Check {
    if cfg.lsp.enable != Some(true) {
        return Check {
            name: "lsp",
            ok: true,
            detail: "disabled ([lsp] enable = true to turn on)".to_string(),
        };
    }
    let Some(commands) = cfg.lsp.commands.as_ref() else {
        return Check {
            name: "lsp",
            ok: true,
            detail: "enabled, no [lsp.commands] configured".to_string(),
        };
    };
    let mut parts: Vec<String> = Vec::with_capacity(commands.len());
    let mut ok = true;
    for (lang, command) in commands {
        let Some(bin) = command.split_whitespace().next() else {
            parts.push(format!("{lang}: (empty command)"));
            ok = false;
            continue;
        };
        let found = on_path(bin);
        ok &= found;
        parts.push(format!(
            "{lang}: {bin}{}",
            if found { "" } else { " (not on PATH)" }
        ));
    }
    Check {
        name: "lsp",
        ok,
        detail: parts.join("; "),
    }
}

/// Whether `bin` exists as a file on PATH (or is an explicit path).
fn on_path(bin: &str) -> bool {
    if bin.contains('/') {
        return std::path::Path::new(bin).exists();
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

/// Which fs-mode enforcement engine this host offers. A configured
/// `[sandbox] mode = "fs"` with no engine is a failure: every bash
/// call will refuse at runtime.
fn sandbox_check(cfg: &Config) -> Check {
    let engine = match ka_sandbox::detect_tool() {
        Some(ka_sandbox::Tool::Bubblewrap) => "bubblewrap (bwrap)",
        Some(ka_sandbox::Tool::Firejail) => "firejail",
        Some(ka_sandbox::Tool::Landlock) => "kernel landlock (re-exec trampoline)",
        Some(ka_sandbox::Tool::Seatbelt) => "macOS seatbelt (sandbox-exec)",
        None => "none",
    };
    let fs_configured = cfg.sandbox.mode.as_deref() == Some("fs");
    let ok = !(fs_configured && engine == "none");
    Check {
        name: "sandbox",
        ok,
        detail: if fs_configured && engine == "none" {
            "mode \"fs\" configured but no engine (bwrap, firejail, landlock, seatbelt) — bash will refuse"
                .to_string()
        } else if fs_configured {
            format!("mode \"fs\" via {engine}")
        } else {
            format!("{engine} (mode \"fs\" would use it)")
        },
    }
}

/// Live `/v1/models` reachability for cloud providers with keys set.
async fn provider_net_check() -> Check {
    let catalog = Catalog::embedded();
    let mut rows: Vec<(String, String, bool)> = Vec::new(); // vendor, base, reachable
    let mut seen = std::collections::HashSet::new();
    for dialect in catalog.dialects.values() {
        let (Some(base), Some(env_var)) = (&dialect.base_url, &dialect.api_key_env) else {
            continue;
        };
        if !seen.insert(base.clone()) {
            continue;
        }
        if !ka_dialect::auth::key_is_set(env_var) {
            continue; // no key: not configured, not a failure
        }
        let url = format!("{}/v1/models", base.trim_end_matches('/'));
        let client = reqwest::Client::new();
        let token = ka_dialect::auth::resolve_token(env_var).unwrap_or_default();
        let ok = client
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        let vendor = base
            .split_once("://")
            .and_then(|(_, rest)| rest.split('.').next())
            .unwrap_or(base)
            .to_string();
        rows.push((vendor, url, ok));
    }
    if rows.is_empty() {
        return Check {
            name: "net:providers",
            ok: true,
            detail: "no keyed providers to probe".to_string(),
        };
    }
    let failed: Vec<String> = rows
        .iter()
        .filter(|(_, _, ok)| !ok)
        .map(|(v, _, _)| v.clone())
        .collect();
    Check {
        name: "net:providers",
        ok: failed.is_empty(),
        detail: format!(
            "{}/{} reachable{}",
            rows.iter().filter(|(_, _, ok)| *ok).count(),
            rows.len(),
            if failed.is_empty() {
                String::new()
            } else {
                format!(" (unreachable: {})", failed.join(", "))
            }
        ),
    }
}

/// Spawn probe for every configured MCP server.
async fn mcp_net_check(cfg: &Config) -> Check {
    if cfg.mcp.is_empty() {
        return Check {
            name: "net:mcp",
            ok: true,
            detail: "no MCP servers configured".to_string(),
        };
    }
    let mut results: Vec<String> = Vec::new();
    let mut failures = 0usize;
    for server in &cfg.mcp {
        match tokio::time::timeout(
            std::time::Duration::from_secs(20),
            ka_engine::mcp::McpClient::spawn_connect(server),
        )
        .await
        {
            Ok(Ok((_client, tools))) => {
                results.push(format!("{} ok ({} tools)", server.name, tools.len()));
            }
            Ok(Err(e)) => {
                failures += 1;
                results.push(format!("{} FAIL ({e})", server.name));
            }
            Err(_) => {
                failures += 1;
                results.push(format!("{} FAIL (timeout)", server.name));
            }
        }
    }
    Check {
        name: "net:mcp",
        ok: failures == 0,
        detail: results.join("; "),
    }
}
