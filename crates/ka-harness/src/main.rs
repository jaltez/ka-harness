//! The `ka` binary. Phase 1 surface: `ka run` (headless turn — final
//! answer to stdout by default, `--print ndjson`/`stream-json` for
//! event streams), `ka models` (catalog + local discovery), `ka config
//! {schema,print}`. The TUI arrives in Phase 3.

use clap::{CommandFactory, Parser, Subcommand};
use ka_dialect::Catalog;
use ka_engine::config::Config;
use ka_engine::spawn_full;
use ka_protocol::{Command, Event, Stop, to_line};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "ka",
    version = env!("KA_VERSION"),
    about = "ka — model-agnostic, low-footprint coding agent"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,

    /// Continue the newest strand (with the terminal's waypoint preferred)
    #[arg(short = 'c', long)]
    continue_latest: bool,
    /// Resume a session by id (prefix ok) or strand file path
    #[arg(long, value_name = "ID")]
    session: Option<String>,
    /// Model selector override (vendor/model@effort)
    #[arg(long)]
    model: Option<String>,
    /// Permission mode override (guarded|free)
    #[arg(long)]
    mode: Option<String>,
    /// Disable features at startup, comma-separated or repeatable:
    /// agents|skills|mcp|hooks|web|lsp|debug, or one item as
    /// mcp:<server>, skill:<name>, tool:<name>, agent:<name>
    #[arg(long, value_delimiter = ',')]
    disable: Vec<ka_protocol::FeatureSpec>,
    /// Re-enable over a `[features]` disable (same grammar)
    #[arg(long, value_delimiter = ',')]
    enable: Vec<ka_protocol::FeatureSpec>,
    /// Sandbox mode override (off|fs)
    #[arg(long, value_parser = ["off", "fs"])]
    sandbox: Option<String>,
    /// Extra strict-TOML config layer (repeatable)
    #[arg(long = "config")]
    configs: Vec<std::path::PathBuf>,
    /// Extra dialect catalog overlay (strict TOML, repeatable)
    #[arg(long = "dialects")]
    dialects: Vec<std::path::PathBuf>,
    /// Skip local-endpoint discovery probes
    #[arg(long)]
    no_discovery: bool,
    /// Trust the project root's .ka/ka.toml (nearest .git ancestor;
    /// stores the decision)
    #[arg(long)]
    trust: bool,
    /// Disable all customizations (AGENTS.md, MEMORY.md, skills,
    /// agents, commands, hooks, MCP, LSP) keeping built-ins, config,
    /// and auth
    #[arg(long)]
    safe_mode: bool,
    /// Run one prompt before the TUI opens, landed in the session
    /// (`ka -i "fix the tests"` — the TUI resumes on that strand)
    #[arg(short = 'i', long = "interactive-prompt", value_name = "PROMPT")]
    interactive_prompt: Option<String>,
}

#[derive(Subcommand, Clone)]
enum CliCommand {
    /// Run one headless turn; prints the final answer (see --print)
    Run {
        /// Prompt text (omitted: read from stdin)
        prompt: Option<String>,
        /// Model selector override (vendor/model:effort)
        #[arg(long)]
        model: Option<String>,
        /// Permission mode override (guarded|free)
        #[arg(long)]
        mode: Option<String>,
        /// Disable features for this run, comma-separated or repeatable
        /// (agents|skills|mcp|hooks|web|lsp|debug, mcp:<server>,
        /// skill:<name>, tool:<name>, agent:<name>)
        #[arg(long, value_delimiter = ',')]
        disable: Vec<ka_protocol::FeatureSpec>,
        /// Re-enable over a `[features]` disable (same grammar)
        #[arg(long, value_delimiter = ',')]
        enable: Vec<ka_protocol::FeatureSpec>,
        /// Sandbox mode override (off|fs)
        #[arg(long, value_parser = ["off", "fs"])]
        sandbox: Option<String>,
        /// Extra strict-TOML config layer (repeatable, highest file wins)
        #[arg(long = "config")]
        configs: Vec<PathBuf>,
        /// Extra dialect catalog overlay (strict TOML, repeatable)
        #[arg(long = "dialects")]
        dialects: Vec<PathBuf>,
        /// Skip local-endpoint discovery probes
        #[arg(long)]
        no_discovery: bool,
        /// Continue the newest strand for this directory
        #[arg(short = 'c', long)]
        continue_latest: bool,
        /// Resume a session by id (prefix ok) or file path
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        /// Trust the project root's .ka/ka.toml (nearest .git ancestor;
        /// stores the decision)
        #[arg(long)]
        trust: bool,
        /// JSON-schema file the reply must satisfy (structured output)
        #[arg(long, value_name = "PATH")]
        schema: Option<PathBuf>,
        /// Output format: text (final answer only, default), ndjson
        /// (ka event stream), or stream-json (Claude-Code-shaped NDJSON)
        #[arg(long, default_value = "text", value_parser = ["text", "ndjson", "stream-json"])]
        print: String,
        /// Wrap the prompt as a read-only code review of the current
        /// changes (like the TUI's /review)
        #[arg(long)]
        review: bool,
    },
    /// Serve the Agent Client Protocol on stdin/stdout
    Acp,
    /// Rebuild the full-text search index over all strands
    #[cfg(feature = "index")]
    Index,
    /// Full-text search over strands
    #[cfg(feature = "index")]
    Search {
        /// FTS5 query
        query: String,
        /// Filter to one session id
        #[arg(long)]
        session: Option<String>,
        /// Max results
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Serve HTTP/SSE sessions on an address
    Serve {
        /// Bind address (loopback by default)
        #[arg(long, default_value = "127.0.0.1:8417")]
        addr: String,
        /// Require this bearer token on every request (or KA_SERVE_TOKEN)
        #[arg(long)]
        token: Option<String>,
    },
    /// Environment health checks
    Doctor {
        /// Probe provider + MCP reachability
        #[arg(long)]
        net: bool,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Check for / install a signed release update
    Update {
        /// Release channel (stable|edge)
        #[arg(long, default_value = "stable")]
        channel: String,
        /// Only report the latest release, do not install
        #[arg(long)]
        check: bool,
    },
    /// List known models (embedded catalog + local discovery)
    Models {
        /// Skip local discovery probes
        #[arg(long)]
        no_discovery: bool,
        /// Extra dialect catalog overlay (strict TOML, repeatable)
        #[arg(long = "dialects")]
        dialects: Vec<PathBuf>,
    },
    /// Rewind the newest strand N user turns (default 1)
    Rewind {
        /// Turns to rewind
        #[arg(default_value_t = 1)]
        turns: u32,
    },
    /// Export a strand as readable markdown (or standalone HTML)
    Export {
        /// Output path (default: stdout)
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Session to export (id prefix; default: newest for this cwd)
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        /// Render a self-contained HTML page instead of markdown
        #[arg(long)]
        html: bool,
    },
    /// Manage user skills (~/.config/ka/skills)
    Skill {
        #[command(subcommand)]
        cmd: SkillCmd,
    },
    /// Manage user-layer customizations (skills, agents, commands,
    /// rules) — generalizes `ka skill`
    Install {
        #[command(subcommand)]
        cmd: InstallCmd,
    },
    /// Import permission rules from another agent's config (one-shot:
    /// printed conversion, strict-validated write)
    Import {
        /// Import format: claude (codex/gemini later)
        format: String,
        /// Path to the source config (claude: settings.json)
        path: PathBuf,
        /// Write to the project layer (.ka/ka.toml)
        #[arg(long)]
        project: bool,
        /// Write to the user layer (~/.config/ka/ka.toml)
        #[arg(long)]
        user: bool,
        /// Print the conversion without writing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Observe a live `ka serve` session read-only (SSE; presence
    /// chips, no write path — prompting stays on the server API)
    Attach {
        /// Session id prefix (omit: the server's newest session)
        #[arg(value_name = "ID")]
        session: Option<String>,
        /// Server address
        #[arg(long, default_value = "127.0.0.1:8417")]
        addr: String,
        /// Bearer token (or KA_SERVE_TOKEN)
        #[arg(long)]
        token: Option<String>,
    },
    /// Emit shell completions (bash | zsh | fish)
    Completions {
        /// Target shell
        shell: String,
    },
    /// List sessions for this directory (ids for `ka --session`)
    Sessions {
        /// Print a JSON array of session objects (id, ts, title, messages,
        /// path, cost, tokens).
        #[arg(long)]
        json: bool,
    },
    /// Restore the latest snapshot of the newest session here
    Undo,
    /// Probe configured MCP servers and list their tools
    Mcp,
    /// List discovered markdown agents (.ka/agents/*.md)
    Agents,
    /// List known providers with API-key env status
    Providers,
    /// Generate a starter AGENTS.md from a quick repo scan
    Init,
    /// Inspect configuration
    Config {
        #[command(subcommand)]
        cmd: ConfigCommand,
    },

    /// Hidden trampoline: apply a landlock policy to self, then exec.
    /// Not for humans — ka-sandbox's wrap_command builds this argv.
    #[cfg(target_os = "linux")]
    #[command(hide = true)]
    KaSandboxExec {
        /// Policy JSON object: {"writable":[…], "net":bool, "env":[…]} built by ka_sandbox::wrap_command.
        policy: String,
        /// The command to run (everything after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
}

#[derive(Subcommand, Clone)]
enum ConfigCommand {
    /// Print the JSON schema for ka.toml layers
    Schema,
    /// Print the resolved configuration
    Print {
        /// Extra strict-TOML config layer (repeatable)
        #[arg(long = "config")]
        configs: Vec<PathBuf>,
    },
}

/// Ed25519 public key embedded at build time (`KA_PUBKEY=<base64>`); the
/// presence marks a signed build and lets `ka update` verify artifacts.
pub const PUBLIC_KEY: Option<&str> = option_env!("KA_PUBKEY");

/// `ka skill ...`: user-scope skill lifecycle. The user skills dir is
/// global, so running the command IS the trust act; discovery still
/// gates user skills on the project's trust decision.
#[derive(clap::Subcommand, Clone, Debug)]
pub enum SkillCmd {
    /// Install a skill from a git URL or a local directory (needs SKILL.md)
    Install {
        /// Git URL (https://, git@) or path to a directory with SKILL.md
        source: String,
        /// Overwrite an installed skill of the same name
        #[arg(long)]
        force: bool,
    },
    /// List installed user skills
    List,
    /// Remove an installed skill by directory name
    Remove {
        /// The skill's directory name (see `ka skill list`)
        name: String,
    },
}

/// `ka install ...`: the generalized customization lifecycle
/// (roadmap 9.5). Same trust model as `ka skill`: user-scope install
/// is the trust act; no registry, no manifests.
#[derive(clap::Subcommand, Clone, Debug)]
pub enum InstallCmd {
    /// Install a customization from a git URL or local path
    Install {
        /// skill | agent | command | rule
        kind: String,
        /// Git URL (https://, git@) or local path (file or directory)
        source: String,
        /// Overwrite an installed item of the same name
        #[arg(long)]
        force: bool,
    },
    /// List installed user customizations (all kinds, or one)
    List {
        /// skill | agent | command | rule (omit = all)
        kind: Option<String>,
    },
    /// Remove an installed customization
    Remove {
        /// skill | agent | command | rule
        kind: String,
        /// The installed item's bare name
        name: String,
    },
}

mod acp;
mod attach;
mod doctor;
mod herdr;
mod import;
mod serve;
mod skills;
mod update;

/// One startup flag that shapes a session but is never written to the
/// strand (only in-session changes are). The `/update` re-exec and the
/// Herdr resume command both replay these so a resumed session keeps its
/// model and posture; keeping each flag with its value lets a consumer
/// drop an unusable pair whole (Herdr rejects apostrophes and control
/// characters).
fn startup_args(cli: &Cli) -> Vec<(&'static str, Option<std::ffi::OsString>)> {
    let mut out: Vec<(&'static str, Option<std::ffi::OsString>)> = Vec::new();
    if let Some(model) = &cli.model {
        out.push(("--model", Some(model.into())));
    }
    if let Some(mode) = &cli.mode {
        out.push(("--mode", Some(mode.into())));
    }
    if let Some(sandbox) = &cli.sandbox {
        out.push(("--sandbox", Some(sandbox.into())));
    }
    for spec in &cli.disable {
        out.push(("--disable", Some(spec.to_string().into())));
    }
    for spec in &cli.enable {
        out.push(("--enable", Some(spec.to_string().into())));
    }
    for path in &cli.configs {
        out.push(("--config", Some(path.clone().into_os_string())));
    }
    for path in &cli.dialects {
        out.push(("--dialects", Some(path.clone().into_os_string())));
    }
    if cli.no_discovery {
        out.push(("--no-discovery", None));
    }
    if cli.safe_mode {
        out.push(("--safe-mode", None));
    }
    if cli.trust {
        out.push(("--trust", None));
    }
    out
}

/// [`startup_args`] as Herdr resume groups: one group per flag, and a
/// non-UTF-8 value drops its group (the argv contract is text-only).
fn herdr_startup(cli: &Cli) -> Vec<Vec<String>> {
    startup_args(cli)
        .into_iter()
        .filter_map(|(flag, value)| {
            let mut group = vec![flag.to_string()];
            if let Some(value) = value {
                group.push(value.into_string().ok()?);
            }
            Some(group)
        })
        .collect()
}

/// The strands storage root (`<data>/strands`). Delegates to
/// `ka_strand::data_dir()` so the index scans the same tree the writer
/// uses (the old local copy was missing the `ka` segment and indexed an
/// empty directory whenever KA_DATA_DIR/XDG_DATA_HOME were unset).
#[cfg(feature = "index")]
fn ka_data_dir_strands() -> PathBuf {
    ka_strand::data_dir().join("strands")
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.safe_mode {
        ka_engine::conventions::set_bare_mode(true);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            eprintln!("ka: failed to start runtime: {e}");
            std::process::exit(1);
        });
    match runtime.block_on(dispatch(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ka: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Startup feature-toggle flags (`--disable`/`--enable`/`--sandbox`) —
/// the CLI layer of the one spec grammar shared with `[features]` and
/// `/features`. `--enable` re-enables over a config-disabled spec.
#[derive(Debug, Clone, Default)]
struct FeatureFlags {
    disable: Vec<ka_protocol::FeatureSpec>,
    enable: Vec<ka_protocol::FeatureSpec>,
    sandbox: Option<String>,
}

impl FeatureFlags {
    /// Apply onto the merged config: disable-union minus enable, plus
    /// the sandbox-mode override.
    fn apply(&self, cfg: &mut Config) {
        if !self.disable.is_empty() || !self.enable.is_empty() {
            let mut list = cfg.features.disable.clone();
            for spec in &self.disable {
                let s = spec.to_string();
                if !list.contains(&s) {
                    list.push(s);
                }
            }
            let enabled: Vec<String> = self.enable.iter().map(|s| s.to_string()).collect();
            list.retain(|s| !enabled.contains(s));
            cfg.features.disable = list;
        }
        if let Some(mode) = &self.sandbox {
            cfg.sandbox.mode = Some(mode.clone());
        }
    }
}

/// Defaults < user < project (if trusted) < extra files < env < flags.
fn load_config(
    configs: &[PathBuf],
    flag_model: Option<String>,
    flag_mode: Option<String>,
    trust_project: bool,
    features: &FeatureFlags,
) -> Result<Config, String> {
    let mut cfg = Config::default();

    let user = Some(ka_engine::config::user_config_path());
    // the project layer anchors at the root project (nearest .git
    // ancestor, else cwd), symmetric with where "always allow" saves it;
    // an unknowable cwd skips the layer entirely rather than guessing a
    // relative path the trust gate would not have resolved to the same root
    let project = trust_project
        .then(|| std::env::current_dir().ok())
        .flatten()
        .map(|cwd| ka_engine::project_root(&cwd).join(".ka/ka.toml"));

    for path in user.iter().chain(project.iter()).chain(configs.iter()) {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let layer = Config::parse_layer(&text, &path.display().to_string())
                    .map_err(|e| e.to_string())?;
                cfg.overlay(layer);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }

    if let Ok(model) = std::env::var("KA_MODEL") {
        if !model.is_empty() {
            cfg.model = Some(model);
        }
    }
    if let Ok(mode) = std::env::var("KA_MODE") {
        if !mode.is_empty() {
            cfg.mode = Some(parse_mode(&mode)?);
        }
    }
    if let Some(model) = flag_model {
        cfg.model = Some(model);
    }
    if let Some(mode) = flag_mode {
        cfg.mode = Some(parse_mode(&mode)?);
    }
    // flags are the final baseline layer — a resumed strand's recorded
    // toggle snapshot still wins at attach, exactly like --model
    features.apply(&mut cfg);
    Ok(cfg)
}

fn parse_mode(s: &str) -> Result<ka_protocol::Mode, String> {
    match s {
        "guarded" | "needs-approval" | "needs_approval" => Ok(ka_protocol::Mode::Guarded),
        "accept-edits" | "accept_edits" => Ok(ka_protocol::Mode::AcceptEdits),
        "free" | "full-access" | "full_access" => Ok(ka_protocol::Mode::Free),
        "plan" => Ok(ka_protocol::Mode::Plan),
        other => Err(format!(
            "unknown mode {other:?} (expected \
guarded|needs-approval|accept-edits|free|full-access|plan)"
        )),
    }
}

/// Waypoint cwd match: exact first, then canonicalized — the same
/// directory reached by different mount spellings (`/mnt/c/Proj` vs
/// `/mnt/c/proj` under WSL's case-insensitive DrvFs, `C:\X` vs `c:\x`,
/// symlinked homes) must count, or `ka -c` misses the session the
/// terminal is actually in. Canonicalize fails on a missing dir → plain
/// inequality, matching the old exact-compare behavior.
fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

fn trust_for_cwd(force: bool) -> bool {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    project_config_trusted(&cwd, force)
}

fn wire_str(w: ka_dialect::Wire) -> String {
    match w {
        ka_dialect::Wire::OpenaiChat => "openai_chat".to_string(),
        ka_dialect::Wire::OpenaiResponses => "openai_responses".to_string(),
        ka_dialect::Wire::AnthropicMessages => "anthropic_messages".to_string(),
    }
}

fn load_catalog(overlays: &[PathBuf]) -> Result<Catalog, String> {
    let mut catalog = Catalog::embedded();
    for path in overlays {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let over = Catalog::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        catalog.overlay(over);
    }
    Ok(catalog)
}

async fn build_catalog(overlays: &[PathBuf], with_discovery: bool) -> Result<Catalog, String> {
    let mut catalog = load_catalog(overlays)?;
    if with_discovery {
        ka_dialect::discovery::overlay_discovered(&mut catalog).await;
    }
    Ok(catalog)
}

async fn dispatch(cli: Cli) -> Result<ExitCode, String> {
    // -i is a TUI-only affordance: refuse the combination instead of
    // silently dropping a user-typed prompt (and never spend a model
    // turn the user cannot see without an interactive terminal)
    if let Some(prompt) = cli.interactive_prompt.clone() {
        if cli.command.is_some() {
            return Err(format!(
                "-i only applies to the interactive TUI (no subcommand); \
                 pass the prompt to the subcommand instead — got {prompt:?}"
            ));
        }
        if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            return Err("-i needs an interactive terminal; use `ka run` for headless".to_string());
        }
    }
    if cli.command.is_none() {
        return run_tui(cli).await;
    }
    let cli = Cli {
        command: cli.command,
        continue_latest: cli.continue_latest,
        session: cli.session,
        model: cli.model,
        mode: cli.mode,
        configs: cli.configs,
        dialects: cli.dialects,
        no_discovery: cli.no_discovery,
        trust: cli.trust,
        safe_mode: cli.safe_mode,
        interactive_prompt: cli.interactive_prompt,
        disable: cli.disable,
        enable: cli.enable,
        sandbox: cli.sandbox,
    };
    match cli.command {
        Some(CliCommand::Run {
            prompt,
            model,
            mode,
            disable,
            enable,
            sandbox,
            configs,
            dialects,
            no_discovery,
            continue_latest,
            session,
            trust,
            schema,
            print,
            review,
        }) => {
            // --review wraps the prompt (or a default) as a read-only
            // review of the working tree — same preset as /review
            let prompt = if review {
                Some(wrap_review_prompt(prompt.as_deref()))
            } else {
                prompt
            };
            let mode = if review && mode.is_none() {
                Some("plan".to_string())
            } else {
                mode
            };
            let schema_value = match schema {
                Some(path) => {
                    let text = std::fs::read_to_string(&path)
                        .map_err(|e| format!("schema {}: {e}", path.display()))?;
                    let value: serde_json::Value = serde_json::from_str(&text)
                        .map_err(|e| format!("schema {}: invalid JSON: {e}", path.display()))?;
                    Some(value)
                }
                None => None,
            };
            let features = FeatureFlags {
                disable,
                enable,
                sandbox,
            };
            run_headless(
                prompt,
                model,
                mode,
                &configs,
                &dialects,
                !no_discovery,
                continue_latest,
                session,
                trust,
                schema_value,
                Vec::new(),
                print,
                features,
            )
            .await
        }
        Some(CliCommand::Acp) => acp::run().await,
        #[cfg(feature = "index")]
        Some(CliCommand::Index) => {
            let db = ka_index::open_db(&ka_index::default_db_path())?;
            let dir = ka_data_dir_strands();
            let stats = ka_index::rebuild(&dir, &db)?;
            println!(
                "indexed {} strand file(s), skipped {} unchanged",
                stats.indexed, stats.skipped
            );
            Ok(ExitCode::SUCCESS)
        }
        #[cfg(feature = "index")]
        Some(CliCommand::Search {
            query,
            session,
            limit,
        }) => {
            let db = ka_index::open_db(&ka_index::default_db_path())?;
            let hits = ka_index::search(&db, &query, session.as_deref(), limit)?;
            if hits.is_empty() {
                println!("(no results)");
                return Ok(ExitCode::SUCCESS);
            }
            for h in hits {
                println!("{} [{}] {}: {}", h.session, h.role, h.ts, h.snippet);
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(CliCommand::Serve { addr, token }) => serve::run(&addr, token).await,
        Some(CliCommand::Doctor { net, json }) => doctor::run(net, json).await,
        Some(CliCommand::Update { channel, check }) => {
            let trust = trust_for_cwd(false);
            let cfg = load_config(&[], None, None, trust, &FeatureFlags::default())?;
            let repo = cfg
                .update
                .repo
                .clone()
                .unwrap_or_else(|| update::DEFAULT_REPO.to_string());
            let message = update::run(&channel, check, &repo).await?;
            println!("{message}");
            Ok(ExitCode::SUCCESS)
        }
        Some(CliCommand::Models {
            no_discovery,
            dialects,
        }) => {
            let mut catalog = load_catalog(&dialects)?;
            if !no_discovery {
                ka_dialect::discovery::overlay_discovered(&mut catalog).await;
            }
            let mut rows: Vec<(&String, &ka_dialect::Dialect)> = catalog.dialects.iter().collect();
            rows.sort_by_key(|(id, _)| {
                let vendor = id.split('/').next().unwrap_or(id.as_str());
                (ka_dialect::providers::vendor_rank(vendor), (*id).clone())
            });
            for (id, d) in rows {
                println!(
                    "{:<34} {:<20} {:>9} {:>7}  {}",
                    id,
                    wire_str(d.wire),
                    d.context,
                    if d.priced {
                        format!("{:.2}", d.price.input_per_mtok)
                    } else {
                        "-".to_string()
                    },
                    d.api_key_env.as_deref().unwrap_or("-")
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(CliCommand::Undo) => run_undo(),
        Some(CliCommand::Mcp) => run_mcp().await,
        Some(CliCommand::Agents) => run_agents(),
        Some(CliCommand::Providers) => run_providers(),
        Some(CliCommand::Init) => run_init(),
        Some(CliCommand::Sessions { json }) => run_sessions(json),
        Some(CliCommand::Rewind { turns }) => run_rewind(turns).await,
        Some(CliCommand::Export { out, session, html }) => run_export(out, session, html),
        Some(CliCommand::Skill { cmd }) => skills::run_skill(cmd),
        Some(CliCommand::Install { cmd }) => skills::run_install(cmd),
        Some(CliCommand::Import {
            format,
            path,
            project,
            user,
            dry_run,
        }) => {
            let layer = match (project, user) {
                (true, false) => Some(import::Layer::Project),
                (false, true) => Some(import::Layer::User),
                (false, false) => None,
                (true, true) => {
                    return Err("--project and --user are mutually exclusive".to_string());
                }
            };
            import::run(&format, &path, layer, dry_run)
        }
        Some(CliCommand::Attach {
            session,
            addr,
            token,
        }) => attach::run(session, &addr, token).await,
        Some(CliCommand::Completions { shell }) => {
            print!("{}", completions_script(&shell)?);
            Ok(ExitCode::SUCCESS)
        }
        Some(CliCommand::Config { cmd }) => match cmd {
            ConfigCommand::Schema => {
                println!(
                    "{}",
                    Config::schema_json().map_err(|e| format!("schema: {e}"))?
                );
                Ok(ExitCode::SUCCESS)
            }
            ConfigCommand::Print { configs } => {
                let trust = trust_for_cwd(false);
                let cfg = load_config(&configs, None, None, trust, &FeatureFlags::default())?;
                let text =
                    toml::to_string_pretty(&cfg).map_err(|e| format!("serialize config: {e}"))?;
                print!("{text}");
                Ok(ExitCode::SUCCESS)
            }
        },
        #[cfg(target_os = "linux")]
        Some(CliCommand::KaSandboxExec { policy, argv }) => {
            match ka_sandbox::trampoline::exec_trampoline(&policy, &argv) {
                Ok(infallible) => match infallible {},
                Err(e) => Err(e),
            }
        }
        None => {
            Cli::command()
                .print_help()
                .map_err(|e| format!("help: {e}"))?;
            Ok(ExitCode::SUCCESS)
        }
    }
}
/// The `/review` preset as a headless prompt (focus: a base ref or a
/// review focus; default reviews against the default branch).
fn wrap_review_prompt(focus: Option<&str>) -> String {
    let base =
        "the repository's default branch — main or master, whichever exists; if neither, HEAD";
    let extra = focus.map(|f| format!("\nFocus: {f}")).unwrap_or_default();
    format!(
        "Review the current changes as a strict senior engineer — READ-ONLY, \
do not modify any files. Compare the working tree against {base} (git diff \
plus untracked files; read files for full context). Produce: (1) a one-line \
verdict (ship / fix first / blocked), (2) findings ordered blocker > major > \
minor > nit, each as `file:line — issue — concrete fix`, (3) what is missing \
(tests, docs, error handling).{extra}"
    )
}

#[allow(clippy::too_many_arguments)]
async fn run_headless(
    prompt: Option<String>,
    model: Option<String>,
    mode: Option<String>,
    configs: &[PathBuf],
    dialects: &[PathBuf],
    with_discovery: bool,
    continue_latest: bool,
    session: Option<String>,
    force_trust: bool,
    schema: Option<serde_json::Value>,
    images: Vec<ka_protocol::ImagePart>,
    print: String,
    features: FeatureFlags,
) -> Result<ExitCode, String> {
    let trust = trust_for_cwd(force_trust);
    warn_untrusted_conventions(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let cfg = load_config(configs, model, mode, trust, &features)?;
    let prompt = match prompt {
        Some(p) => p,
        None => read_stdin()?,
    };
    if prompt.trim().is_empty() {
        return Err("empty prompt".to_string());
    }

    let cfg_model = cfg.model.clone();
    let catalog = build_catalog(dialects, with_discovery).await?;
    let choice = if let Some(id) = session {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        resolve_session(&cwd, &id)?
    } else if continue_latest {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        match ka_engine::read_waypoint() {
            Some((way_cwd, path)) if same_dir(&way_cwd, &cwd) && path.exists() => {
                ka_engine::StrandChoice::Path(path)
            }
            _ => ka_engine::StrandChoice::Latest,
        }
    } else {
        ka_engine::StrandChoice::New
    };
    let mut handle = spawn_full(cfg, catalog, choice);
    handle
        .commands
        .send(Command::Prompt {
            allowed_tools: None,
            model: None,
            text: prompt,
            schema,
            images,
            skills: Vec::new(),
        })
        .await
        .map_err(|_| "engine closed before prompt".to_string())?;

    let mut stdout = std::io::stdout().lock();
    let mut final_stop: Option<Stop> = None;
    let mut final_text = String::new();
    let mut sj = StreamJsonPrinter::with_model(cfg_model.clone());
    while let Some(event) = handle.events.recv().await {
        // headless policy: permission asks auto-deny (last option = deny)
        if let Event::Ask { id, .. } = &event {
            handle
                .commands
                .send(Command::Answer {
                    question: id.clone(),
                    choice: 2,
                })
                .await
                .map_err(|_| "engine closed during ask")?;
        }
        match &event {
            Event::TurnFinished { stop, .. } => final_stop = Some(*stop),
            Event::Idle => break,
            _ => {}
        }
        for line in headless_sink(&print, &event, &mut sj, &mut final_text)? {
            use std::io::Write;
            writeln!(stdout, "{line}").map_err(|e| format!("stdout: {e}"))?;
        }
        // text mode has no event stream: surface hard errors on stderr
        if print == "text" {
            if let Event::Error {
                message,
                retryable: false,
                ..
            } = &event
            {
                eprintln!("ka: {message}");
            }
        }
    }
    // stream-json: the terminating result line (after the loop so cost
    // and stop reason are final)
    if print == "stream-json" {
        for line in sj.finish(final_stop) {
            use std::io::Write;
            writeln!(stdout, "{line}").map_err(|e| format!("stdout: {e}"))?;
        }
    }
    // text: the final answer only
    if print == "text" && !final_text.is_empty() {
        use std::io::Write;
        writeln!(stdout, "{final_text}").map_err(|e| format!("stdout: {e}"))?;
    }
    std::io::Write::flush(&mut stdout).map_err(|e| format!("stdout: {e}"))?;
    match final_stop {
        Some(Stop::Aborted) => Ok(ExitCode::from(2)),
        Some(Stop::Error) => Ok(ExitCode::from(1)),
        _ => Ok(ExitCode::SUCCESS),
    }
}

/// One event's stdout contribution for a headless run. All modes
/// accumulate text deltas into `final_text` — this is the single
/// accumulation point (the caller's loop must not also collect, or
/// every chunk lands twice); `ndjson` emits the raw ka event as one
/// NDJSON line, `stream-json` maps onto the Claude-Code shape, and
/// `text` (default) emits nothing — the caller prints the collected
/// final answer after the loop.
fn headless_sink(
    print: &str,
    event: &Event,
    sj: &mut StreamJsonPrinter,
    final_text: &mut String,
) -> Result<Vec<String>, String> {
    if let Event::Delta {
        kind: ka_protocol::DeltaKind::Text(t),
    } = event
    {
        final_text.push_str(t);
    }
    match print {
        "stream-json" => Ok(sj.map_event(event)),
        "ndjson" => to_line(event)
            .map(|l| vec![l])
            .map_err(|e| format!("serialize event: {e}")),
        _ => Ok(Vec::new()),
    }
}

/// Maps ka events onto the Claude-Code-shaped NDJSON surface used by
/// `ka run --print stream-json`:
///
/// - `{"type":"system","subtype":"init","model":...,"session":...}`
/// - assistant text: `{"type":"assistant","message":{"role":"assistant",
///   "content":[{"type":"text","text":...}]}}`
/// - tool round-trips: an assistant `tool_use` block per CallStarted and
///   a user `tool_result` per CallOutput
/// - terminal: `{"type":"result","subtype":"success|error|aborted",
///   "is_error":...,"total_cost_usd":...}`
///
/// Text deltas accumulate; a buffered assistant message flushes when a
/// tool call starts or the turn finishes. Event kinds without a mapping
/// (inventory, meters, notes) are dropped.
#[derive(Default)]
struct StreamJsonPrinter {
    session: Option<String>,
    model: Option<String>,
    text: String,
    finished: bool,
    total_cost: f64,
}

impl StreamJsonPrinter {
    fn with_model(model: Option<String>) -> Self {
        Self {
            model,
            ..Default::default()
        }
    }
}

impl StreamJsonPrinter {
    fn emit(&self, value: serde_json::Value) -> Option<String> {
        if self.finished {
            return None;
        }
        Some(value.to_string())
    }

    fn flush_text(&mut self) -> Option<String> {
        if self.text.is_empty() {
            return None;
        }
        let text = std::mem::take(&mut self.text);
        self.emit(serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": text}]
            }
        }))
    }

    fn map_event(&mut self, event: &Event) -> Vec<String> {
        if self.finished {
            return Vec::new();
        }
        match event {
            Event::SessionInfo { id } => {
                self.session = Some(id.clone());
                self.emit(serde_json::json!({
                    "type": "system",
                    "subtype": "init",
                    "session": id,
                    "model": self.model,
                }))
                .into_iter()
                .collect()
            }
            Event::Delta {
                kind: ka_protocol::DeltaKind::Text(t),
            } => {
                self.text.push_str(t);
                Vec::new()
            }
            Event::CallStarted { tool, id, .. } => {
                let mut out = Vec::new();
                if let Some(line) = self.flush_text() {
                    out.push(line);
                }
                if let Some(line) = self.emit(serde_json::json!({
                    "type": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": [{
                            "type": "tool_use",
                            "id": id,
                            "name": tool,
                            "input": {}
                        }]
                    }
                })) {
                    out.push(line);
                }
                out
            }
            Event::CallOutput {
                id,
                excerpt,
                is_error,
                ..
            } => {
                if let Some(line) = self.emit(serde_json::json!({
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": id,
                            "content": excerpt,
                            "is_error": is_error
                        }]
                    }
                })) {
                    vec![line]
                } else {
                    Vec::new()
                }
            }
            Event::ModelChanged { selector } => {
                self.model = Some(selector.clone());
                Vec::new()
            }
            Event::TurnFinished { usage, .. } => {
                self.total_cost += usage.cost;
                let mut out = Vec::new();
                if let Some(line) = self.flush_text() {
                    out.push(line);
                }
                out
            }
            _ => Vec::new(),
        }
    }

    fn finish(&mut self, final_stop: Option<Stop>) -> Vec<String> {
        self.finished = true;
        let mut out = Vec::new();
        if let Some(line) = self.flush_text() {
            out.push(line);
        }
        let (subtype, is_error) = match final_stop {
            Some(Stop::Aborted) => ("aborted", true),
            Some(Stop::Error) | None => ("error", true),
            _ => ("success", false),
        };
        let total_cost = self.total_cost;
        out.push(
            serde_json::json!({
                "type": "result",
                "subtype": subtype,
                "is_error": is_error,
                "total_cost_usd": total_cost,
            })
            .to_string(),
        );
        out
    }
}

/// The interactive surface: picker (unless -c/--session) then the TUI.
async fn run_tui(cli: Cli) -> Result<ExitCode, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let choice = if let Some(id) = cli.session.clone() {
        resolve_session(&cwd, &id)?
    } else if cli.continue_latest {
        // waypoint first, else newest
        match ka_engine::read_waypoint() {
            Some((way_cwd, path)) if same_dir(&way_cwd, &cwd) && path.exists() => {
                ka_engine::StrandChoice::Path(path)
            }
            _ => ka_engine::StrandChoice::Latest,
        }
    } else {
        // default: a fresh chat; /session inside the TUI lists the rest
        ka_engine::StrandChoice::New
    };

    let trust = trust_for_cwd(cli.trust);
    warn_untrusted_conventions(&cwd);
    let features = FeatureFlags {
        disable: cli.disable.clone(),
        enable: cli.enable.clone(),
        sandbox: cli.sandbox.clone(),
    };
    let cfg = load_config(
        &cli.configs,
        cli.model.clone(),
        cli.mode.clone(),
        trust,
        &features,
    )?;
    // the engine resolves its cwd as config cwd else process cwd; the
    // TUI must see the same value or plan-file paths, memory roots, and
    // custom-command roots drift from what the engine anchors to
    let session_cwd = cfg
        .cwd
        .clone()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let catalog = build_catalog(&cli.dialects, !cli.no_discovery).await?;
    let model_label = cfg.model.clone().unwrap_or_else(|| "(canned)".to_string());
    let mut providers: Vec<ka_term::tui::ProviderInfo> = ka_dialect::providers::PROVIDERS
        .iter()
        .map(|p| ka_term::tui::ProviderInfo {
            name: p.name.to_string(),
            env_var: p.key_env.unwrap_or("").to_string(),
            base_url: p.base_url.to_string(),
            key_set: p.key_env.is_some_and(ka_dialect::auth::key_is_set),
        })
        .collect();
    // catalog-derived vendors (models.dev: coding plans, z.ai tiers, ...)
    // beyond the curated registry, so settings and `ka providers` see them
    for (id, d) in catalog.dialects.iter() {
        let Some(vendor) = id.split('/').next() else {
            continue;
        };
        if providers.iter().any(|p| p.name == vendor) {
            continue;
        }
        let base_url = d.base_url.clone().unwrap_or_default();
        if base_url.is_empty() {
            continue;
        }
        let env_var = d.api_key_env.clone().unwrap_or_default();
        providers.push(ka_term::tui::ProviderInfo {
            name: vendor.to_string(),
            env_var: env_var.clone(),
            base_url,
            key_set: !env_var.is_empty() && ka_dialect::auth::key_is_set(&env_var),
        });
    }
    // official registry order (official block first, locals last);
    // catalog-only vendors (models.dev plans/tiers) rank as community
    providers.sort_by_key(|p| ka_dialect::providers::vendor_rank(&p.name));
    let mut models: Vec<ka_term::tui::ModelInfo> = catalog
        .dialects
        .iter()
        .map(|(id, d)| ka_term::tui::ModelInfo {
            id: id.clone(),
            wire: wire_str(d.wire),
            context: d.context,
            key_env: d.api_key_env.clone().unwrap_or_default(),
            key_set: d
                .api_key_env
                .as_deref()
                .is_some_and(ka_dialect::auth::key_is_set),
            doc_url: d.doc_url.clone().unwrap_or_default(),
            price_in: d.price.input_per_mtok,
            price_out: d.price.output_per_mtok,
            priced: d.priced,
            plan: id.split('/').next().is_some_and(|v| v.contains("plan")),
            efforts: d.efforts.clone(),
        })
        .collect();
    // the picker inherits catalog order: official block first, then
    // community vendors, local discoveries last (vendor_rank policy)
    models.sort_by_key(|m| {
        let vendor = m.id.split('/').next().unwrap_or_default();
        (ka_dialect::providers::vendor_rank(vendor), m.id.clone())
    });
    let fresh = matches!(choice, ka_engine::StrandChoice::New);
    // startup update check (read before `cfg` moves into the engine):
    // one rate-limited (24 h) availability probe against GitHub
    // releases — never installs, `ka update` stays the only install
    // path. Detached so a slow network never delays the first frame;
    // failures stay silent (no badge, no nagging).
    let update_repo = cfg
        .update
        .repo
        .clone()
        .unwrap_or_else(|| update::DEFAULT_REPO.to_string());
    let update_enabled = cfg.update.effective_check() == ka_engine::config::UpdateCheck::Startup;
    let (update_tx, update_rx) = tokio::sync::mpsc::channel::<ka_term::tui::UpdateNotice>(1);
    tokio::spawn(async move {
        if let Some((current, latest)) = update::startup_check(&update_repo, update_enabled).await {
            let _ = update_tx
                .send(ka_term::tui::UpdateNotice { current, latest })
                .await;
        }
    });
    let handle = ka_engine::spawn_full(cfg, catalog, choice);
    let ka_engine::EngineHandle { commands, events } = handle;
    // `ka -i "<prompt>"`: one turn runs before the TUI opens — the
    // engine's event channel buffers it, so the TUI streams the turn
    // (and answers any permission ask) live from its first frame
    if let Some(prompt) = cli.interactive_prompt.clone() {
        commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: prompt,
                schema: None,
                images: Vec::new(),
                skills: Vec::new(),
            })
            .await
            .map_err(|_| "engine closed before prompt".to_string())?;
    }
    let agents: Vec<(String, String)> = if ka_engine::conventions::bare_mode() {
        Vec::new()
    } else {
        ka_engine::agents::AgentDef::discover(&cwd)
            .into_iter()
            .map(|a| (a.name, a.description))
            .collect()
    };
    let (cfg_tui_header_glyph, tui_notify, tui_mouse_capture, tui_herdr) = {
        let trust = trust_for_cwd(false);
        load_config(
            &cli.configs,
            cli.model.clone(),
            cli.mode.clone(),
            trust,
            &features,
        )
        .map(|c| {
            (
                c.effective_header_glyph(),
                ka_term::tui::NotifySettings {
                    bell: c.effective_bell(),
                    command: c.tui.notify.clone(),
                },
                c.effective_mouse_capture(),
                c.tui.herdr.unwrap_or(false),
            )
        })
        .unwrap_or_else(|_| {
            (
                "\u{25c6}".to_string(),
                ka_term::tui::NotifySettings {
                    bell: true,
                    command: None,
                },
                true,
                false,
            )
        })
    };
    // opt-in Herdr integration: inert unless [tui] herdr = true AND the
    // process runs inside a Herdr pane
    let mut herdr = herdr::init(tui_herdr, herdr_startup(&cli));
    if let Some(reporter) = &herdr {
        ka_term::tui::set_activity_sink(reporter.sink());
    }
    let exit = ka_term::tui::run(
        commands,
        events,
        &model_label,
        providers,
        models,
        agents,
        &cfg_tui_header_glyph,
        tui_notify,
        tui_mouse_capture,
        fresh,
        session_cwd,
        update_rx,
    )
    .await
    .map_err(|e| format!("tui: {e}"))?;
    // the agent stops owning the pane the moment the TUI returns; the
    // reporter flushes a release (bounded by one report deadline)
    if let Some(mut reporter) = herdr.take() {
        reporter.finish();
    }
    match exit {
        ka_term::tui::Exit::Quit | ka_term::tui::Exit::EngineEnded => Ok(ExitCode::SUCCESS),
        ka_term::tui::Exit::Update { session } => {
            // /update: run the updater in this terminal, then re-exec
            // attached to the same session — the atomic swap has
            // already landed on disk, so the exec picks up whichever
            // binary is current (new on success, old on failure)
            let trust = trust_for_cwd(false);
            let cfg = load_config(&[], None, None, trust, &FeatureFlags::default())?;
            let repo = cfg
                .update
                .repo
                .clone()
                .unwrap_or_else(|| update::DEFAULT_REPO.to_string());
            let channel =
                std::env::var("KA_UPDATE_CHANNEL").unwrap_or_else(|_| "stable".to_string());
            match update::run(&channel, false, &repo).await {
                Ok(msg) => println!("{msg}"),
                Err(e) => eprintln!("ka: update failed: {e}"),
            }
            if session.is_empty() {
                return Ok(ExitCode::SUCCESS);
            }
            let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
            let mut relaunch = std::process::Command::new(exe);
            relaunch.arg("--session").arg(&session);
            // startup flags are never written to the strand (only
            // in-session changes are), so the re-exec must carry them
            // or the resumed session silently changes model and
            // sandbox posture
            for (flag, value) in startup_args(&cli) {
                relaunch.arg(flag);
                if let Some(value) = value {
                    relaunch.arg(value);
                }
            }
            println!("⟳ resuming session…");
            use std::os::unix::process::CommandExt;
            let err = relaunch.exec();
            Err(format!("resume after update failed: {err}"))
        }
    }
}

/// `ka completions <shell>` (roadmap 9.7): emit a completion script.
/// The word lists are derived from the live clap command at runtime —
/// subcommands and flags can never drift from the parser (data over
/// code; no hand-maintained tables, no clap_complete dependency).
fn completions_script(shell: &str) -> Result<String, String> {
    let command = Cli::command();
    let mut subs: Vec<String> = command
        .get_subcommands()
        .filter(|c| !c.is_hide_set() && c.get_name() != "help")
        .map(|c| c.get_name().to_string())
        .collect();
    subs.sort();
    let mut opts: Vec<String> = Vec::new();
    for arg in command.get_arguments() {
        if let Some(long) = arg.get_long() {
            opts.push(format!("--{long}"));
        }
        if let Some(short) = arg.get_short() {
            opts.push(format!("-{short}"));
        }
    }
    opts.sort();
    opts.dedup();
    let subs = subs.join(" ");
    let opts = opts.join(" ");
    match shell {
        "bash" => Ok(format!(
            "# ka shell completions (bash) — source me, or install to \\\n\
             # $(brew --prefix 2>/dev/null)/etc/bash_completion.d/ka\n\
             _ka_completions() {{\n\
             \x20 local cur=\"${{COMP_WORDS[COMP_CWORD]}}\"\n\
             \x20 local subs=\"{subs}\"\n\
             \x20 local opts=\"{opts}\"\n\
             \x20 if [[ ${{COMP_CWORD}} -eq 1 ]]; then\n\
             \x20   COMPREPLY=( $(compgen -W \"$subs $opts\" -- \"$cur\") )\n\
             \x20 else\n\
             \x20   COMPREPLY=( $(compgen -W \"$opts\" -- \"$cur\") )\n\
             \x20 fi\n\
             }}\n\
             complete -F _ka_completions ka\n"
        )),
        "zsh" => Ok(format!(
            "#compdef ka\n\
             # ka shell completions (zsh) — install to a fpath dir as _ka\n\
             _ka() {{\n\
             \x20 local -a subs opts\n\
             \x20 subs=({subs})\n\
             \x20 opts=({opts})\n\
             \x20 _arguments -A \"-*\" \"1: :->sub\" \"*::arg:->args\"\n\
             \x20 case $state in\n\
             \x20   sub) _describe 'ka command' subs ;;\n\
             \x20   args) _describe 'ka option' opts ;;\n\
             \x20 esac\n\
             }}\n\
             compdef _ka ka\n"
        )),
        "fish" => Ok(format!(
            "# ka shell completions (fish) — install to ~/.config/fish/completions/ka.fish\n\
             complete -c ka -n '__fish_use_subcommand' -a \"{subs}\" -d 'ka subcommand'\n\
             complete -c ka -n '__fish_use_subcommand' -a \"{opts}\"\n\
             complete -c ka -n 'not __fish_use_subcommand' -a \"{opts}\"\n"
        )),
        other => Err(format!(
            "unknown shell {other:?} (expected bash | zsh | fish)"
        )),
    }
}

/// Resolve a `--session` reference (id, id prefix, file path, or a
/// memorable session name — exact, then unique name prefix).
fn resolve_session(cwd: &std::path::Path, id: &str) -> Result<ka_engine::StrandChoice, String> {
    let matched =
        match ka_strand::resolve_id(cwd, id).map_err(|e| format!("session lookup: {e}"))? {
            ka_strand::IdMatch::None => {
                // memorable names sit between ids and titles in the
                // resolution order
                ka_strand::resolve_name(cwd, id).map_err(|e| format!("session lookup: {e}"))?
            }
            other => other,
        };
    match matched {
        ka_strand::IdMatch::Unique(summary) => Ok(ka_engine::StrandChoice::Path(summary.path)),
        ka_strand::IdMatch::None => Err(format!("no session matches '{id}'")),
        ka_strand::IdMatch::Ambiguous(candidates) => {
            let ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
            Err(format!(
                "session reference '{id}' is ambiguous: {}",
                ids.join(", ")
            ))
        }
    }
}

/// `ka sessions`: list strands for this cwd with resolvable ids.
fn run_sessions(json: bool) -> Result<ExitCode, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let strands = ka_strand::list(&cwd).map_err(|e| format!("listing sessions: {e}"))?;
    if json {
        let rows: Vec<serde_json::Value> = strands
            .iter()
            .map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "ts": s.ts,
                    "title": s.title,
                    "name": s.name,
                    "messages": s.messages,
                    "path": s.path.display().to_string(),
                    "cost": s.cost,
                    "tokens": s.tokens,
                })
            })
            .collect();
        let text = serde_json::to_string_pretty(&rows).map_err(|e| format!("serialize: {e}"))?;
        println!("{text}");
        return Ok(ExitCode::SUCCESS);
    }
    if strands.is_empty() {
        println!("no sessions yet for {}", cwd.display());
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{:<26} {:>5} {:>10}  {first_message:<}",
        "session id",
        "msgs",
        "cost",
        first_message = "first message"
    );
    for s in strands.iter().take(30) {
        // a set name rides beside the title: `fox · fix the pump`
        let label = match &s.name {
            Some(n) => format!("{n} · {}", s.title),
            None => s.title.clone(),
        };
        println!(
            "{:<26} {:>5} {:>10}  {}",
            s.id,
            s.messages,
            format!("${:.4}", s.cost),
            label
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `ka undo`: restore the newest session's latest snapshot (headless).
fn run_undo() -> Result<ExitCode, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let latest = ka_strand::latest(&cwd)
        .map_err(|e| format!("listing sessions: {e}"))?
        .ok_or_else(|| "no sessions for this directory".to_string())?;
    let mut snaps = ka_engine::hands::snapshots::Snapshots::open(&cwd);
    snaps.set_strand(latest.id.clone());
    match snaps.undo() {
        Ok(Some(entry)) => {
            let what = if entry.existed {
                format!("restored {}", entry.path.display())
            } else {
                format!(
                    "removed {} (was created that session)",
                    entry.path.display()
                )
            };
            println!("↩ {what}");
            Ok(ExitCode::SUCCESS)
        }
        Ok(None) => {
            eprintln!("↩ nothing to undo in session {}", latest.id);
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => Err(format!("undo failed: {e}")),
    }
}

/// `ka mcp`: spawn each configured server, list tools, exit.
/// `ka agents`: list discovered markdown agents.
fn run_agents() -> Result<ExitCode, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let agents = ka_engine::agents::AgentDef::discover(&cwd);
    if agents.is_empty() {
        println!("no agents configured (.ka/agents/*.md, ~/.config/ka/agents/*.md)");
        return Ok(ExitCode::SUCCESS);
    }
    for a in &agents {
        let desc = if a.description.is_empty() {
            "(no description)"
        } else {
            &a.description
        };
        println!("{:<16} {:<3} steps  {}", a.name, a.max_steps, desc);
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_mcp() -> Result<ExitCode, String> {
    let trust = trust_for_cwd(false);
    let cfg = load_config(&[], None, None, trust, &FeatureFlags::default())?;
    if cfg.mcp.is_empty() {
        println!("no [[mcp]] servers configured (~/.config/ka/ka.toml or .ka/ka.toml)");
        return Ok(ExitCode::SUCCESS);
    }
    for server in &cfg.mcp {
        println!(
            "{:<16} {} {}",
            server.name,
            server
                .command
                .clone()
                .unwrap_or_else(|| server.url.clone().unwrap_or_default()),
            server.args.join(" ")
        );
        match tokio::time::timeout(
            std::time::Duration::from_secs(20),
            ka_engine::mcp::McpClient::spawn_connect(server),
        )
        .await
        {
            Ok(Ok((_client, tools))) => {
                for t in &tools {
                    let desc: String = t.description.chars().take(60).collect();
                    println!("  {:<34} {}", t.name, desc);
                }
                if tools.is_empty() {
                    println!("  (no tools)");
                }
            }
            Ok(Err(e)) => println!("  ✗ {e}"),
            Err(_) => println!("  ✗ connect timed out"),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_providers() -> Result<ExitCode, String> {
    let header = format!(
        "{:<24} {:<26} {:<8} {}",
        "provider", "api key env", "key", "endpoint"
    );
    println!("{header}");
    // curated registry first, then catalog-derived vendors (models.dev:
    // coding plans, provider tiers, ...) the registry does not know
    let mut seen: Vec<String> = Vec::new();
    let mut rows: Vec<(String, String, bool, String)> = Vec::new();
    for p in ka_dialect::providers::PROVIDERS {
        let env = p.key_env.unwrap_or("-").to_string();
        let set = p.key_env.is_some_and(ka_dialect::auth::key_is_set);
        seen.push(p.name.to_string());
        rows.push((p.name.to_string(), env, set, p.base_url.to_string()));
    }
    let catalog = ka_dialect::dialects::Catalog::embedded();
    for (id, d) in catalog.dialects.iter() {
        let Some(vendor) = id.split('/').next() else {
            continue;
        };
        if seen.iter().any(|s| s == vendor) || vendor.len() > 24 {
            continue;
        }
        let Some(base_url) = d.base_url.clone() else {
            continue;
        };
        if base_url.is_empty() {
            continue;
        }
        let env = d.api_key_env.clone().unwrap_or_else(|| "-".to_string());
        let set = d
            .api_key_env
            .as_deref()
            .is_some_and(ka_dialect::auth::key_is_set);
        seen.push(vendor.to_string());
        rows.push((vendor.to_string(), env, set, base_url));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, env, set, base_url) in rows {
        let set = if env == "-" {
            "n/a"
        } else if set {
            "yes"
        } else {
            "no"
        };
        println!("{:<24} {:<26} {:<8} {}", name, env, set, base_url);
    }
    println!("\nany provider/<model> selector works against these vendors, catalog row or not");
    Ok(ExitCode::SUCCESS)
}

/// Whether `dir` exists and holds at least one entry.
fn dir_has_entries(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

/// Whether this project ships gateable `.ka/` content: a local config,
/// skills, or convention hooks. Nothing gateable → nothing to trust.
fn has_gateable_ka(cwd: &std::path::Path) -> bool {
    cwd.join(".ka/ka.toml").is_file()
        || dir_has_entries(&cwd.join(".ka/skills"))
        || dir_has_entries(&cwd.join(".ka/hooks"))
}

/// Whether the project `.ka/` layer (config, skills, hooks) for `cwd` may
/// load. Prompts on a TTY (first sighting), skips with a warning
/// otherwise. `--trust` forces. The store itself lives in
/// [`ka_engine::trust`]; approval unlocks all three layers.
fn project_config_trusted(cwd: &std::path::Path, force_trust: bool) -> bool {
    // project scope anchors at the root project: gateable content, the
    // store entry, and the prompt all address the same directory, so one
    // approval covers every session inside the same project
    let cwd = &ka_engine::project_root(cwd);
    // nothing to trust — and no prompt — without gateable .ka/ content
    if !has_gateable_ka(cwd) {
        return false;
    }
    if ka_engine::trust::trusted_in(cwd, &ka_engine::trust::load_trust()) {
        return true;
    }
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    if force_trust {
        ka_engine::trust::approve(cwd);
        return true;
    }
    // prompt only when interactive
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!(
            "ka: this project (root: {}) has a .ka/ project config, skills, or hooks.\n   Trust it (loads its rules/model settings, skills and hooks)? [y/N]",
            canonical.display()
        );
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() {
            let ans = line.trim().to_lowercase();
            if ans == "y" || ans == "yes" {
                ka_engine::trust::approve(cwd);
                return true;
            }
        }
    }
    eprintln!(
        "ka: project .ka/ NOT trusted; skipping its config, skills and hooks (pass --trust to trust it)"
    );
    false
}

/// Startup note: when an untrusted project's `.ka/` would have contributed
/// skills or hooks (both silently skipped by the engine), say so.
fn warn_untrusted_conventions(cwd: &std::path::Path) {
    let cwd = &ka_engine::project_root(cwd);
    if ka_engine::trust::project_trusted(cwd) {
        return;
    }
    let mut layers: Vec<&str> = Vec::new();
    if dir_has_entries(&cwd.join(".ka/skills")) {
        layers.push("skills");
    }
    if dir_has_entries(&cwd.join(".ka/hooks")) {
        layers.push("hooks");
    }
    if !layers.is_empty() {
        eprintln!(
            "ka: project .ka/{} present but NOT trusted; skipping {} (pass --trust to enable)",
            layers.join(" and "),
            if layers.len() == 1 { "it" } else { "them" }
        );
    }
}

/// Deterministic starter AGENTS.md from repo shape (no model call).
/// Generated at the root project — the nearest `.git` ancestor of the
/// launch dir, else the launch dir — where ka and other agents read it.
fn run_init() -> Result<ExitCode, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let root = ka_engine::project_root(&cwd);
    let target = root.join("AGENTS.md");
    if target.exists() {
        return Err(format!(
            "AGENTS.md already exists at {}; refusing to overwrite",
            target.display()
        ));
    }

    let mut langs = Vec::new();
    if root.join("Cargo.toml").exists() {
        langs.push(("Rust", "cargo build", "cargo test"));
    }
    if root.join("package.json").exists() {
        langs.push(("TypeScript/JavaScript", "npm install", "npm test"));
    }
    if root.join("go.mod").exists() {
        langs.push(("Go", "go build ./...", "go test ./..."));
    }
    if root.join("pyproject.toml").is_file() || root.join("requirements.txt").is_file() {
        langs.push(("Python", "pip install -e .", "pytest"));
    }
    let git = root.join(".git").exists();

    let mut body =
        String::from("# AGENTS.md\n\nGuidance for AI agents working in this repository.\n\n");
    if let Some((lang, build, test)) = langs.first() {
        body.push_str(&format!(
            "## Project\n\n- Language: {lang}\n- Build: `{build}`\n- Test: `{test}`\n{}\n",
            if git {
                "- VCS: git (never commit directly to main)\n"
            } else {
                ""
            }
        ));
    }
    if langs.len() > 1 {
        body.push_str("(Multiple build systems detected — refine this list.)\n\n");
    }
    body.push_str(
        "## Conventions\n\n- Describe code style, naming, and layout rules here.\n- List commands that must pass before finishing a task.\n\n## Notes\n\n- Anything an agent should know (quirks, forbidden areas, deployment).\n",
    );
    std::fs::write(&target, body).map_err(|e| format!("write: {e}"))?;
    println!("wrote {}", target.display());
    println!("edit it to describe real conventions; ka reads it automatically");
    Ok(ExitCode::SUCCESS)
}

/// Headless rewind: attach to the latest strand and drop N turns.
async fn run_rewind(turns: u32) -> Result<ExitCode, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let latest = ka_strand::latest(&cwd)
        .map_err(|e| format!("listing strands: {e}"))?
        .ok_or_else(|| "no strands for this directory".to_string())?;
    println!("rewinding {} turn(s) in {}", turns, latest.path.display());

    let choice = match ka_engine::read_waypoint() {
        Some((way_cwd, path)) if same_dir(&way_cwd, &cwd) && path.exists() => {
            ka_engine::StrandChoice::Path(path)
        }
        _ => ka_engine::StrandChoice::Path(latest.path.clone()),
    };
    // rewind only reads strand files — it must not silently approve the
    // project's .ka layer (trust is a side effect no reader should trip)
    let cfg = load_config(&[], None, None, false, &FeatureFlags::default())?;
    let catalog = build_catalog(&[], true).await?;
    let mut handle = ka_engine::spawn_full(cfg, catalog, choice);
    handle
        .commands
        .send(ka_protocol::Command::Rewind { turns })
        .await
        .map_err(|_| "engine closed")?;
    while let Some(evt) = handle.events.recv().await {
        match evt {
            ka_protocol::Event::Note { message } => println!("  {message}"),
            ka_protocol::Event::Error { message, .. } => {
                eprintln!("ka: {message}");
                return Ok(ExitCode::FAILURE);
            }
            ka_protocol::Event::Idle => break,
            _ => {}
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Export the latest (or waypoint) strand as markdown.
fn run_export(
    out: Option<PathBuf>,
    session: Option<String>,
    html: bool,
) -> Result<ExitCode, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let target = match session {
        Some(id) => match resolve_session(&cwd, &id)? {
            ka_engine::StrandChoice::Path(path) => path,
            _ => unreachable!("resolve_session returns a path"),
        },
        None => match ka_engine::read_waypoint() {
            Some((way_cwd, path)) if same_dir(&way_cwd, &cwd) && path.exists() => path,
            _ => {
                ka_strand::latest(&cwd)
                    .map_err(|e| format!("listing strands: {e}"))?
                    .ok_or_else(|| "no strands for this directory".to_string())?
                    .path
            }
        },
    };
    let records = ka_strand::read(&target).map_err(|e| format!("{}: {e}", target.display()))?;
    let md = ka_strand::render_markdown(&records);
    // the html path derives its output name from the strand when -o is
    // omitted, so `ka export --html` just works
    let out = match (out, html) {
        (Some(path), _) => Some(path),
        (None, true) => Some(cwd.join(format!(
            "{}.html",
            target
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "ka-export".to_string())
        ))),
        (None, false) => None,
    };
    match out {
        Some(path) => {
            if html {
                let title = ka_strand::title_of(&records);
                let page = ka_strand::render_html(&title, &md);
                std::fs::write(&path, page).map_err(|e| format!("{}: {e}", path.display()))?;
            } else {
                std::fs::write(&path, &md).map_err(|e| format!("{}: {e}", path.display()))?;
            }
            println!("wrote {}", path.display());
        }
        None => print!("{md}"),
    }
    Ok(ExitCode::SUCCESS)
}

fn read_stdin() -> Result<String, String> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| format!("stdin: {e}"))?;
    Ok(buf)
}

#[cfg(test)]
mod feature_flag_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn spec(s: &str) -> ka_protocol::FeatureSpec {
        s.parse().unwrap()
    }

    #[test]
    fn disable_flags_union_and_enable_removes() {
        // config disabled skills; flags add agents and re-enable skills
        let mut cfg = Config::parse_layer("[features]\ndisable = [\"skills\"]\n", "user").unwrap();
        let flags = FeatureFlags {
            disable: vec![spec("agents"), spec("mcp:github")],
            enable: vec![spec("skills")],
            sandbox: Some("fs".to_string()),
        };
        flags.apply(&mut cfg);
        assert_eq!(
            cfg.features.disable,
            vec!["agents".to_string(), "mcp:github".to_string()],
            "union minus enable"
        );
        assert_eq!(cfg.sandbox.mode.as_deref(), Some("fs"));
    }

    #[test]
    fn enable_over_nothing_is_a_noop() {
        let mut cfg = Config::default();
        FeatureFlags {
            disable: Vec::new(),
            enable: vec![spec("web")],
            sandbox: None,
        }
        .apply(&mut cfg);
        assert!(cfg.features.disable.is_empty());
    }

    #[test]
    fn cli_parses_comma_separated_specs_and_rejects_unknown() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["ka", "--disable", "agents,mcp:github", "--sandbox", "fs"])
            .unwrap();
        assert_eq!(cli.disable, vec![spec("agents"), spec("mcp:github")]);
        assert_eq!(cli.sandbox.as_deref(), Some("fs"));
        assert!(
            Cli::try_parse_from(["ka", "--disable", "bogus"]).is_err(),
            "unknown specs fail at flag parse"
        );
        assert!(
            Cli::try_parse_from(["ka", "--sandbox", "jail"]).is_err(),
            "unknown sandbox modes fail at flag parse"
        );
        // the run subcommand carries the same grammar
        let cli = Cli::try_parse_from(["ka", "run", "--disable", "skills", "do it"]).unwrap();
        match cli.command {
            Some(CliCommand::Run { disable, .. }) => {
                assert_eq!(disable, vec![spec("skills")])
            }
            _ => panic!("expected run subcommand"),
        }
    }
}

#[cfg(test)]
mod print_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn stream_json_maps_the_documented_shape() {
        let mut sj = StreamJsonPrinter::with_model(Some("anthropic/claude-sonnet-5".into()));

        // init
        let lines = sj.map_event(&Event::SessionInfo { id: "s123".into() });
        assert_eq!(lines.len(), 1);
        let init: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(init["type"], "system");
        assert_eq!(init["subtype"], "init");
        assert_eq!(init["session"], "s123");
        assert_eq!(init["model"], "anthropic/claude-sonnet-5");

        // text deltas buffer; a tool call flushes them as assistant text
        sj.map_event(&Event::Delta {
            kind: ka_protocol::DeltaKind::Text("thinking…".into()),
        });
        let lines = sj.map_event(&Event::CallStarted {
            tool: "read".into(),
            id: "c1".into(),
            detail: String::new(),
        });
        assert_eq!(lines.len(), 2, "{lines:?}");
        let text_msg: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(text_msg["type"], "assistant");
        assert_eq!(text_msg["message"]["content"][0]["type"], "text");
        assert_eq!(text_msg["message"]["content"][0]["text"], "thinking…");
        let tool_msg: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(tool_msg["message"]["content"][0]["type"], "tool_use");
        assert_eq!(tool_msg["message"]["content"][0]["id"], "c1");

        // tool result maps as a user tool_result
        let lines = sj.map_event(&Event::CallOutput {
            tool: "read".into(),
            id: "c1".into(),
            excerpt: "file body".into(),
            is_error: false,
            spill: None,
        });
        assert_eq!(lines.len(), 1);
        let result: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(result["type"], "user");
        assert_eq!(result["message"]["content"][0]["type"], "tool_result");
        assert_eq!(result["message"]["content"][0]["content"], "file body");

        // turn finished flushes the remaining text as the final assistant
        // message; the result line lands at finish()
        sj.map_event(&Event::Delta {
            kind: ka_protocol::DeltaKind::Text("answer".into()),
        });
        let lines = sj.map_event(&Event::TurnFinished {
            stop: Stop::Done,
            usage: ka_protocol::Usage {
                cost: 0.42,
                ..Default::default()
            },
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        let text_msg: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(text_msg["message"]["content"][0]["text"], "answer");

        // finish emits the terminal result with accumulated cost
        let lines = sj.finish(Some(Stop::Done));
        assert_eq!(lines.len(), 1, "{lines:?}");
        let result: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(result["type"], "result");
        assert_eq!(result["subtype"], "success");
        assert_eq!(result["is_error"], false);
        assert_eq!(result["total_cost_usd"], 0.42);

        // error stop maps to an error result
        let mut sj = StreamJsonPrinter::default();
        sj.finish(Some(Stop::Error));
        // error path: nothing more may be mapped after finish
        assert!(sj.map_event(&Event::Idle).is_empty());
    }

    #[test]
    fn stream_json_error_and_abort_subtypes() {
        let mut sj = StreamJsonPrinter::default();
        let lines = sj.finish(Some(Stop::Aborted));
        let result: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(result["subtype"], "aborted");
        assert_eq!(result["is_error"], true);

        let mut sj = StreamJsonPrinter::default();
        let lines = sj.finish(Some(Stop::Error));
        let result: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(result["subtype"], "error");
    }

    #[test]
    fn text_mode_collects_the_final_answer_only() {
        let mut sj = StreamJsonPrinter::default();
        let mut final_text = String::new();
        let events = [
            Event::TurnStarted {
                context: ka_protocol::ContextMeter::default(),
            },
            Event::Delta {
                kind: ka_protocol::DeltaKind::Text("hel".into()),
            },
            Event::Delta {
                kind: ka_protocol::DeltaKind::Text("lo".into()),
            },
            Event::TurnFinished {
                stop: Stop::Done,
                usage: ka_protocol::Usage::default(),
            },
        ];
        for e in &events {
            assert!(
                headless_sink("text", e, &mut sj, &mut final_text)
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(final_text, "hello");
    }

    #[test]
    fn ndjson_mode_emits_one_line_per_event() {
        let mut sj = StreamJsonPrinter::default();
        let mut final_text = String::new();
        let events = [
            Event::Delta {
                kind: ka_protocol::DeltaKind::Text("hi".into()),
            },
            Event::TurnFinished {
                stop: Stop::Done,
                usage: ka_protocol::Usage::default(),
            },
        ];
        let mut lines = 0;
        for e in &events {
            lines += headless_sink("ndjson", e, &mut sj, &mut final_text)
                .unwrap()
                .len();
        }
        assert_eq!(lines, events.len(), "one NDJSON line per event");
        assert_eq!(final_text, "hi", "deltas still accumulate");
    }

    #[test]
    fn ndjson_lines_are_parseable_ka_events() {
        let mut sj = StreamJsonPrinter::default();
        let mut final_text = String::new();
        let line = headless_sink(
            "ndjson",
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Text("hey".into()),
            },
            &mut sj,
            &mut final_text,
        )
        .unwrap()
        .remove(0);
        let parsed: ka_protocol::Event = ka_protocol::from_line(&line).unwrap();
        assert!(matches!(
            parsed,
            ka_protocol::Event::Delta {
                kind: ka_protocol::DeltaKind::Text(_)
            }
        ));
    }
}

#[cfg(test)]
mod completions_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// `ka completions` (roadmap 9.7): every visible subcommand and the
    /// load-bearing global flags appear in all three shells; hidden
    /// subcommands never do.
    #[test]
    fn completions_cover_subcommands_and_flags_for_each_shell() {
        for shell in ["bash", "zsh", "fish"] {
            let script = completions_script(shell).unwrap();
            for sub in [
                "run",
                "serve",
                "attach",
                "install",
                "import",
                "completions",
                "sessions",
                "export",
                "undo",
                "models",
                "doctor",
                "update",
                "init",
                "acp",
                "skill",
            ] {
                assert!(script.contains(sub), "{shell} lost subcommand {sub}");
            }
            for flag in [
                "--session",
                "--model",
                "--mode",
                "--safe-mode",
                "--interactive-prompt",
            ] {
                assert!(script.contains(flag), "{shell} lost flag {flag}");
            }
            assert!(
                !script.contains("ka-sandbox-exec"),
                "{shell} must not complete hidden subcommands"
            );
        }
        assert!(completions_script("powershell").is_err());
        assert!(completions_script("nushell").is_err());
    }

    /// `ka -i "<prompt>"` parses onto the global CLI.
    #[test]
    fn interactive_prompt_flag_parses() {
        let cli = Cli::try_parse_from(["ka", "-i", "fix the tests"]).unwrap();
        assert_eq!(cli.interactive_prompt.as_deref(), Some("fix the tests"));
        assert!(cli.command.is_none());
        let cli = Cli::try_parse_from([
            "ka",
            "--interactive-prompt",
            "review this",
            "--session",
            "abc",
        ])
        .unwrap();
        assert_eq!(cli.interactive_prompt.as_deref(), Some("review this"));
        assert_eq!(cli.session.as_deref(), Some("abc"));
    }
}
