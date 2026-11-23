//! The `ka` binary. Phase 1 surface: `ka run` (headless turn — final
//! answer to stdout by default, `--print ndjson`/`stream-json` for
//! event streams), `ka models` (catalog + local discovery), `ka config
//! {schema,print}`. The TUI arrives in Phase 3.

use clap::{CommandFactory, Parser, Subcommand};
use ka_dialect::Catalog;
use ka_engine::config::{Config, ShellEnvMode};
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
    /// Permission mode override (ask|accept_edits|allow|readonly)
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
    /// Resume and RE-SEND the prompt of the last interrupted turn
    /// (crash recovery: `ka --restore` picks up where the aborted
    /// turn stopped, re-asking the question that never got answered)
    #[arg(long)]
    restore: bool,
    /// Disable all customizations (AGENTS.md, MEMORY.md, skills,
    /// agents, commands, hooks, MCP, LSP) keeping built-ins, config,
    /// and auth
    #[arg(long)]
    safe_mode: bool,
    /// Run one prompt before the TUI opens, landed in the session
    /// (`ka -i "fix the tests"` — the TUI resumes on that strand)
    #[arg(short = 'i', long = "interactive-prompt", value_name = "PROMPT")]
    interactive_prompt: Option<String>,
    /// Reasoning-effort override (off|low|medium|high|max)
    #[arg(long)]
    effort: Option<String>,
    /// Attach an image to the initial prompt (repeatable, png/jpg/webp/gif,
    /// 5 MB cap each; needs `-i` for the TUI or a `ka run` prompt)
    #[arg(long, value_name = "PATH")]
    image: Vec<PathBuf>,
    /// Hermetic run: no local discovery, no update check, no web tools,
    /// no telemetry-adjacent traffic — nothing but the model call
    #[arg(long)]
    offline: bool,
    /// Replace ka's built-in identity sentence with this text for the
    /// session (config `[prompt] identity_file` still wins)
    #[arg(long)]
    system_prompt: Option<String>,
    /// Positive tool allowlist for the session, comma-separated hand
    /// names (read, edit, bash, ...): everything else is hidden from the
    /// model and rejected at admit time
    #[arg(long, value_delimiter = ',')]
    tools: Vec<String>,
    /// Tools to disable for the session, comma-separated hand names
    /// (same as `--disable tool:<name>`)
    #[arg(long, value_delimiter = ',')]
    exclude_tools: Vec<String>,
    /// Skip the user config layer (~/.config/ka/ka.toml) for this run
    #[arg(long)]
    ignore_user_config: bool,
    /// Ignore permission rules for this run: `[[rules]]` tables and the
    /// `.ka/rules/*.md` conventions (hardstops still apply)
    #[arg(long)]
    ignore_rules: bool,
    /// Leave no persistent trace: strands, spills and snapshots go to a
    /// temp data dir removed at exit (TUI and `ka run` only)
    #[arg(long)]
    ephemeral: bool,
    /// Load a profile layer on top of the user config
    /// (~/.config/ka/profiles/<name>.toml; missing file is a hard error)
    #[arg(long)]
    profile: Option<String>,
    /// Grant sandbox write access to an additional directory (repeatable;
    /// file access only — .ka config and trust are not extended)
    #[arg(long = "add-dir", value_name = "DIR")]
    add_dirs: Vec<PathBuf>,
    /// Brief the model with a cheap repo prewalk before the first turn
    /// (same as `[context] prewalk = true`)
    #[arg(long)]
    prewalk: bool,
}

/// The Run variant carries the whole headless flag surface and dwarfs
/// the other arms; the enum lives on the stack once per process, so
/// boxing fields to shrink the discriminant buys nothing real.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand, Clone)]
enum CliCommand {
    /// Run one headless turn; prints the final answer (see --print)
    Run {
        /// Prompt text (omitted: read from stdin)
        prompt: Option<String>,
        /// Model selector override (vendor/model[@effort])
        #[arg(long)]
        model: Option<String>,
        /// Permission mode override (ask|accept_edits|allow|readonly)
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
        /// Resume and RE-SEND the prompt of the last interrupted turn
        #[arg(long)]
        restore: bool,
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
        /// Bidirectional RPC over stdio: read Command JSONL from stdin
        /// (Prompt, Abort, Answer, SetModel, ...), write Event JSONL to
        /// stdout; ends when stdin closes and the engine settles
        #[arg(long)]
        rpc: bool,
        /// Reasoning-effort override (off|low|medium|high|max)
        #[arg(long)]
        effort: Option<String>,
        /// Attach an image to the prompt (repeatable, png/jpg/webp/gif)
        #[arg(long, value_name = "PATH")]
        image: Vec<PathBuf>,
        /// Hermetic run: no discovery, no update check, no web tools
        #[arg(long)]
        offline: bool,
        /// Replace ka's built-in identity sentence with this text
        #[arg(long)]
        system_prompt: Option<String>,
        /// Positive tool allowlist, comma-separated hand names
        #[arg(long, value_delimiter = ',')]
        tools: Vec<String>,
        /// Tools to disable, comma-separated hand names
        #[arg(long, value_delimiter = ',')]
        exclude_tools: Vec<String>,
        /// Skip the user config layer for this run
        #[arg(long)]
        ignore_user_config: bool,
        /// Ignore permission rules for this run (hardstops still apply)
        #[arg(long)]
        ignore_rules: bool,
        /// Leave no persistent trace (temp data dir, removed at exit)
        #[arg(long)]
        ephemeral: bool,
        /// Load a profile layer on top of the user config
        #[arg(long)]
        profile: Option<String>,
        /// Grant sandbox write access to an additional directory
        #[arg(long = "add-dir", value_name = "DIR")]
        add_dirs: Vec<PathBuf>,
        /// Brief the model with a cheap repo prewalk first
        #[arg(long)]
        prewalk: bool,
        /// Write the final answer to this file instead of stdout
        /// (text mode only)
        #[arg(short = 'o', long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// Cap tool-execution steps for this run (default: config max_steps)
        #[arg(long, value_name = "N")]
        max_turns: Option<u32>,
        /// Ask to stop once session cost crosses this many USD
        /// (headless: the ask auto-denies, so the run stops)
        #[arg(long, value_name = "USD")]
        max_budget_usd: Option<f64>,
        /// Hard cost cap: when the session's final cost exceeds this,
        /// the run exits with code 3
        #[arg(long, value_name = "USD")]
        max_cost: Option<f64>,
        /// Warn on stderr when the session's final cost crosses this
        #[arg(long, value_name = "USD")]
        warn_cost: Option<f64>,
        /// Wall-clock cap for the whole run in seconds; on elapse the
        /// turn is aborted and the run exits with code 3
        #[arg(long, value_name = "SECS")]
        max_time: Option<u64>,
    },
    /// Serve the Agent Client Protocol on stdin/stdout
    Acp {
        /// Extra strict-TOML config layer (repeatable, highest file wins)
        #[arg(long = "config")]
        configs: Vec<PathBuf>,
    },
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
        /// Extra strict-TOML config layer (repeatable, highest file wins)
        #[arg(long = "config")]
        configs: Vec<PathBuf>,
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
        /// Fuzzy filter: case-insensitive substring on the model id
        #[arg(value_name = "QUERY")]
        query: Option<String>,
        /// Refresh the runtime catalog from models.dev and store it in
        /// the data dir (loaded before embedded + user overlays)
        #[arg(long)]
        update: bool,
    },
    /// Manage passport subscription auth (Claude Pro/Max; `passport`
    /// cargo feature)
    Passport {
        #[command(subcommand)]
        cmd: PassportCmd,
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
        /// Export the raw strand records as JSONL (roundtrippable)
        #[arg(long)]
        jsonl: bool,
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
    /// Reclaim session storage: expired spills, orphaned or aged
    /// snapshots, empty directories (strands are only reported —
    /// `ka session delete` removes those)
    Gc {
        /// Report what would be removed without removing anything
        #[arg(long)]
        dry_run: bool,
        /// Age threshold in days (default 7)
        #[arg(long, default_value_t = 7)]
        max_age_days: u64,
    },
    /// Run a command under ka's sandbox policy
    /// (bwrap / firejail / landlock — the same policy bash children get)
    Sandbox {
        /// Force the fs sandbox even when `[sandbox] mode` is off
        #[arg(long)]
        fs: bool,
        /// The command to run (joined into one shell line under the policy)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Queue a message into a live `ka serve` session
    Queue {
        /// Session id prefix, memorable name, or title substring
        #[arg(long, value_name = "ID")]
        session: String,
        /// The message text to queue
        #[arg(long, value_name = "TEXT")]
        message: String,
        /// Server address
        #[arg(long, default_value = "127.0.0.1:8417")]
        addr: String,
        /// Bearer token (or KA_SERVE_TOKEN)
        #[arg(long)]
        token: Option<String>,
    },
    /// Import a session from another agent into a ka strand
    /// (claude | codex; one-shot, never modifies the source)
    ImportSession {
        /// Source format: claude | codex
        format: String,
        /// Path to the source session file (.jsonl)
        path: PathBuf,
        /// Working directory the imported strand anchors to
        /// (default: the current directory)
        #[arg(long)]
        cwd: Option<PathBuf>,
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
        /// List archived sessions instead (see `ka session archive`)
        #[arg(long)]
        archived: bool,
    },
    /// Manage one session: rename (retitle), delete (cascades to
    /// children), archive / unarchive
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// Restore the latest snapshot of the newest session here
    Undo,
    /// Probe configured MCP servers and list their tools
    Mcp,
    /// Serve ka's read hands as a read-only MCP stdio server
    /// (JSON-RPC 2.0, one message per line)
    McpServe {
        /// Only expose these tools, comma-separated or repeatable
        /// (read, glob, grep, sessions_search, session_info)
        #[arg(long, value_delimiter = ',')]
        tools: Vec<String>,
        /// Restrict to read-only tools (v1: the server never exposes
        /// bash regardless — see mcp_serve module docs)
        #[arg(long)]
        read_only: bool,
    },
    /// List discovered markdown agents (.ka/agents/*.md)
    Agents,
    /// List known providers with API-key env status
    Providers,
    /// Generate a starter AGENTS.md from a quick repo scan (foreign
    /// agent rule files are imported as marked sections unless
    /// --no-import)
    Init {
        /// Skip importing .cursorrules / .cursor/rules/*.mdc /
        /// .github/copilot-instructions.md into the generated AGENTS.md
        #[arg(long)]
        no_import: bool,
    },
    /// GitHub Actions integration: vendor the PR-automation workflow
    /// (.github/workflows/ka.yml)
    Action {
        #[command(subcommand)]
        cmd: action::ActionCmd,
    },
    /// Manage crontab entries that invoke `ka run`
    /// (# ka-schedule <name> marked lines)
    Schedule {
        #[command(subcommand)]
        cmd: schedule::ScheduleCmd,
    },
    /// Run one recipe: .ka/recipes/<name>.toml, its steps as sequential
    /// prompts in a single session
    Recipe {
        /// Recipe name (bare stem; the file is .ka/recipes/<name>.toml)
        name: String,
        /// Placeholder override k=v (repeatable; beats the recipe's
        /// [vars] defaults)
        #[arg(long = "var", value_name = "K=V")]
        vars: Vec<String>,
        /// Print the rendered prompts and exit — no session, no model
        #[arg(long)]
        dry_run: bool,
    },
    /// Manage git worktrees under <repo>/.worktrees/
    Worktree {
        #[command(subcommand)]
        cmd: worktree::WorktreeCmd,
    },
    /// Inspect configuration
    Config {
        #[command(subcommand)]
        cmd: ConfigCommand,
    },
    /// Dry-run permission rules against a command
    Rules {
        #[command(subcommand)]
        cmd: RulesCommand,
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
    /// Set one scalar key in the user layer (~/.config/ka/ka.toml)
    Set {
        /// Key name (model, effort, mode, tui.mouse, guards.spend_usd, ...)
        key: String,
        /// Value (parsed by the key's type; list keys take commas)
        value: String,
    },
    /// Print one key's effective value from the resolved config
    Get {
        /// Key name (same table as `ka config set`)
        key: String,
        /// Extra strict-TOML config layer (repeatable)
        #[arg(long = "config")]
        configs: Vec<PathBuf>,
    },
    /// Print the config layer paths ka reads
    Path,
}

/// `ka passport …`: subscription auth lifecycle (claude-plan vendor).
#[derive(Subcommand, Clone)]
enum PassportCmd {
    /// Run the Claude OAuth flow (browser + loopback callback) and
    /// store the tokens in the OS keyring under `ka-passport`
    Login,
    /// Clear the stored credentials
    Logout,
    /// Print the stored-credential status
    Status,
}

/// `ka rules …`: dry-run access to the permission gate.
#[derive(Subcommand, Clone)]
enum RulesCommand {
    /// Evaluate the effective rules against a command, showing which
    /// rule matches first and the verdict the gate would apply
    Check {
        /// The bash command line to test
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// Tool to evaluate against (default: bash)
        #[arg(long, default_value = "bash")]
        tool: String,
        /// Extra strict-TOML config layer (repeatable)
        #[arg(long = "config")]
        configs: Vec<PathBuf>,
    },
}

/// `ka session …`: one-session management backed by the strand store.
#[derive(Subcommand, Clone)]
enum SessionCmd {
    /// Set the session's display title (the `/name` alias is untouched)
    Rename {
        /// Session id prefix or memorable name
        id: String,
        /// New display title
        title: String,
    },
    /// Delete a session and every child forked from it
    /// (irreversible; the waypointer falls back to the newest)
    Delete {
        /// Session id prefix
        id: String,
    },
    /// Move a session out of the active list
    /// (`ka sessions --archived` lists it; `unarchive` brings it back)
    Archive {
        /// Session id prefix
        id: String,
    },
    /// Move an archived session back into the active list
    Unarchive {
        /// Session id prefix (searched in the archive)
        id: String,
    },
    /// Pin a session: `★` in the picker, sorted first
    Pin {
        /// Session id prefix
        id: String,
    },
    /// Remove a session's pin
    Unpin {
        /// Session id prefix
        id: String,
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
mod action;
mod attach;
mod doctor;
mod herdr;
mod import;
mod mcp_serve;
mod recipes;
mod schedule;
mod serve;
mod session_import;
mod skills;
mod update;
mod worktree;

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
    if let Some(effort) = &cli.effort {
        out.push(("--effort", Some(effort.into())));
    }
    for path in &cli.image {
        out.push(("--image", Some(path.clone().into_os_string())));
    }
    if cli.offline {
        out.push(("--offline", None));
    }
    if let Some(text) = &cli.system_prompt {
        out.push(("--system-prompt", Some(text.into())));
    }
    if !cli.tools.is_empty() {
        out.push(("--tools", Some(cli.tools.join(",").into())));
    }
    if !cli.exclude_tools.is_empty() {
        out.push(("--exclude-tools", Some(cli.exclude_tools.join(",").into())));
    }
    if cli.ignore_user_config {
        out.push(("--ignore-user-config", None));
    }
    if cli.ignore_rules {
        out.push(("--ignore-rules", None));
    }
    if cli.ephemeral {
        out.push(("--ephemeral", None));
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
    if cli.ignore_rules {
        ka_engine::conventions::set_rules_disabled(true);
    }
    // --ephemeral: session state lands in a fresh temp data dir that is
    // removed when the process exits — nothing persists, including
    // spills and snapshots. Only the session-producing surfaces make
    // sense; resuming contradicts the whole point.
    let mut ephemeral_dir: Option<PathBuf> = None;
    if cli.ephemeral {
        match cli.command {
            None | Some(CliCommand::Run { .. }) => {}
            Some(_) => {
                eprintln!("ka: --ephemeral applies to the TUI and `ka run` only");
                return ExitCode::FAILURE;
            }
        }
        if cli.continue_latest || cli.session.is_some() {
            eprintln!("ka: --ephemeral cannot resume a session (--continue/--session)");
            return ExitCode::FAILURE;
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("ka-ephemeral-{}-{stamp}", std::process::id()));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("ka: --ephemeral: {e}");
            return ExitCode::FAILURE;
        }
        ka_strand::set_data_dir_override(Some(dir.clone()));
        ephemeral_dir = Some(dir);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            eprintln!("ka: failed to start runtime: {e}");
            std::process::exit(1);
        });
    let outcome = runtime.block_on(dispatch(cli));
    if let Some(dir) = ephemeral_dir {
        ka_strand::set_data_dir_override(None);
        let _ = std::fs::remove_dir_all(&dir);
    }
    match outcome {
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

/// Build the startup flags from the CLI fields, mapping
/// `--exclude-tools <name>` onto `tool:<name>` disable specs (the same
/// grammar as `--disable`).
fn feature_flags(
    disable: Vec<ka_protocol::FeatureSpec>,
    enable: Vec<ka_protocol::FeatureSpec>,
    sandbox: Option<String>,
    exclude_tools: &[String],
) -> Result<FeatureFlags, String> {
    let mut disable = disable;
    for name in exclude_tools {
        let spec: ka_protocol::FeatureSpec = format!("tool:{name}")
            .parse()
            .map_err(|e| format!("--exclude-tools {name}: {e}"))?;
        disable.push(spec);
    }
    Ok(FeatureFlags {
        disable,
        enable,
        sandbox,
    })
}

/// Project-layer anchor for an interactive trust decision: Some(cwd)
/// when trusted, else None (no project layer).
fn trusted_project_cwd(trusted: bool) -> Option<PathBuf> {
    if trusted {
        std::env::current_dir().ok()
    } else {
        None
    }
}

/// Config chain for the non-CLI surfaces (`serve`, `acp`): defaults <
/// user < project (trusted, non-interactive — a server must never
/// prompt) < extra `--config` files < env. No CLI model/mode/feature
/// flags: those belong to the interactive surface.
fn load_surface_config(configs: &[PathBuf], cwd: &std::path::Path) -> Result<Config, String> {
    let project =
        ka_engine::trust::project_trusted(&ka_engine::project_root(cwd)).then(|| cwd.to_path_buf());
    load_config(
        configs,
        None,
        None,
        project,
        &FeatureFlags::default(),
        false,
        None,
    )
}

/// Defaults < user (unless skipped) < profile < project (if trusted) <
/// extra files < env (`KA_*` per-key layer) < flags.
fn load_config(
    configs: &[PathBuf],
    flag_model: Option<String>,
    flag_mode: Option<String>,
    project_cwd: Option<PathBuf>,
    features: &FeatureFlags,
    skip_user: bool,
    profile: Option<PathBuf>,
) -> Result<Config, String> {
    let mut cfg = Config::default();

    let user = (!skip_user).then(ka_engine::config::user_config_path);
    // the project layer anchors at the root project (nearest .git
    // ancestor, else cwd), symmetric with where "always allow" saves it;
    // the caller owns the trust decision (Some = trusted anchor, None =
    // no project layer)
    let project = project_cwd.map(|cwd| ka_engine::project_root(&cwd).join(".ka/ka.toml"));

    let chain = user
        .iter()
        .chain(profile.iter())
        .chain(project.iter())
        .chain(configs.iter());
    for path in chain {
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
    // CFG-02: `KA_<KEY>` env layer — one variable per settable key,
    // `__` spells the dot (`KA_TUI__MOUSE=native` → `tui.mouse`).
    // Unknown suffixes are ignored (the KA_ namespace also carries
    // runtime vars: KA_DATA_DIR, KA_HOOK, KA_SERVE_TOKEN, KA_TTY, ...);
    // a known key with a bad value is a hard error naming the variable.
    // The layer sits above the file layers, below flags.
    for (var, value) in std::env::vars() {
        let Some(suffix) = var.strip_prefix("KA_") else {
            continue;
        };
        if suffix.is_empty() || var.as_str() == "KA_MODEL" || var.as_str() == "KA_MODE" {
            continue;
        }
        let key = suffix.to_lowercase().replace("__", ".");
        if !known_config_key(&key) {
            continue;
        }
        apply_config_key(&mut cfg, &key, &value).map_err(|e| format!("{var}: {e}"))?;
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
        "ask" | "guarded" | "needs-approval" | "needs_approval" => Ok(ka_protocol::Mode::Ask),
        "accept-edits" | "accept_edits" | "accept edits" => Ok(ka_protocol::Mode::AcceptEdits),
        "allow" | "free" | "full-access" | "full_access" => Ok(ka_protocol::Mode::Allow),
        "readonly" | "read-only" | "read_only" | "plan" => Ok(ka_protocol::Mode::Readonly),
        "dontask" | "dont-ask" | "dont_ask" => Ok(ka_protocol::Mode::DontAsk),
        other => Err(format!(
            "unknown mode {other:?} (expected \
ask|accept_edits|allow|readonly|dontask — guarded/free/plan still parse as aliases)"
        )),
    }
}

/// `--effort <LEVEL>`: same spelling as `/thinking` and the selector's
/// `@effort` suffix.
fn parse_effort(s: Option<&str>) -> Result<Option<ka_protocol::Effort>, String> {
    match s {
        None => Ok(None),
        Some("off") => Ok(Some(ka_protocol::Effort::Off)),
        Some("low") => Ok(Some(ka_protocol::Effort::Low)),
        Some("medium") => Ok(Some(ka_protocol::Effort::Medium)),
        Some("high") => Ok(Some(ka_protocol::Effort::High)),
        Some("max") => Ok(Some(ka_protocol::Effort::Max)),
        Some(other) => Err(format!(
            "unknown effort {other:?} (expected off|low|medium|high|max)"
        )),
    }
}

/// Detect an image file by magic bytes (png/jpeg/gif/webp) — the same
/// table the TUI's `/image` uses, kept byte-identical.
fn sniff_image_bytes(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// `--image <PATH>`: load each file into an attachment part (magic-byte
/// sniff, 5 MB cap per image — same caps as the TUI's paste path).
fn image_parts(paths: &[PathBuf]) -> Result<Vec<ka_protocol::ImagePart>, String> {
    const MAX_BYTES: usize = 5 * 1024 * 1024;
    paths
        .iter()
        .map(|path| {
            let bytes =
                std::fs::read(path).map_err(|e| format!("image {}: {e}", path.display()))?;
            if bytes.len() > MAX_BYTES {
                return Err(format!(
                    "image {} is {:.1} MB, over the 5 MB cap",
                    path.display(),
                    bytes.len() as f64 / (1024.0 * 1024.0)
                ));
            }
            let media_type = sniff_image_bytes(&bytes).ok_or_else(|| {
                format!(
                    "image {}: unsupported type (png/jpg/webp/gif only)",
                    path.display()
                )
            })?;
            Ok(ka_protocol::ImagePart {
                data: ka_term::tui::b64encode(&bytes),
                media_type: media_type.to_string(),
            })
        })
        .collect()
}

/// Parse a scalar value for `ka config set` by target type.
fn parse_value<T: std::str::FromStr>(key: &str, value: &str) -> Result<T, String> {
    value
        .parse::<T>()
        .map_err(|_| format!("-- {key}: {value:?} is not a valid value for this key"))
}

fn parse_bool(key: &str, value: &str) -> Result<bool, String> {
    match value {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        other => Err(format!("-- {key}: expected true|false, got {other:?}")),
    }
}

/// Set one scalar key in the user config layer (~/.config/ka/ka.toml).
/// The layer is re-serialized (strict-parsed first: an already-broken
/// file is refused, never silently rewritten). List keys take
/// comma-separated values. Table/vec-of-table keys ([[rules]],
/// [[hooks]], [[mcp]], [[search]], [[verify.lints]], [[model_pins]])
/// are deliberately not settable here — edit the file for those.
fn config_set(key: &str, value: &str) -> Result<PathBuf, String> {
    let path = ka_engine::config::user_config_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    // strip a previous `#:schema` directive so it is not duplicated by
    // the re-serialization below (the writer re-prepends it)
    let body = text
        .lines()
        .skip_while(|l| l.trim_start().starts_with("#:"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut layer = if body.trim().is_empty() {
        Config::default()
    } else {
        Config::parse_layer(&body, &path.display().to_string()).map_err(|e| e.to_string())?
    };
    apply_config_key(&mut layer, key, value)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    }
    // the `#:schema` pointer gives editors (taplo and friends)
    // autocomplete against the emitted schema file
    let schema_path = path.with_extension("schema.json");
    std::fs::write(
        &schema_path,
        Config::schema_json().map_err(|e| format!("schema: {e}"))?,
    )
    .map_err(|e| format!("{}: {e}", schema_path.display()))?;
    let mut out = String::from("#:schema ka.schema.json\n\n");
    out.push_str(&toml::to_string_pretty(&layer).map_err(|e| format!("serialize: {e}"))?);
    std::fs::write(&path, out).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Apply one `ka config set` key onto a config layer. The match IS the
/// key table: adding a key here is the whole change.
fn apply_config_key(cfg: &mut Config, key: &str, value: &str) -> Result<(), String> {
    if !known_config_key(key) {
        return Err(format!(
            "unknown key {key:?} — see `ka config schema`; list-of-table keys \
([[rules]], [[hooks]], [[mcp]], [[search]], [[verify.lints]], [[model_pins]]) \
are file-edited only"
        ));
    }
    let list = |v: &str| -> Vec<String> {
        v.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    };
    match key {
        "model" => cfg.model = Some(value.to_string()),
        "effort" => {
            cfg.effort = parse_effort(Some(value))?;
        }
        "mode" => cfg.mode = Some(parse_mode(value)?),
        "roles.default" => cfg.roles.default = Some(value.to_string()),
        "roles.fast" => cfg.roles.fast = Some(value.to_string()),
        "tui.header_glyph" => cfg.tui.header_glyph = Some(value.to_string()),
        "tui.bell" => cfg.tui.bell = Some(parse_bool(key, value)?),
        "tui.notify" => cfg.tui.notify = Some(value.to_string()),
        "tui.mouse" => {
            cfg.tui.mouse = Some(match value {
                "capture" => ka_engine::config::MouseMode::Capture,
                "native" => ka_engine::config::MouseMode::Native,
                other => {
                    return Err(format!(
                        "-- tui.mouse: expected capture|native, got {other:?}"
                    ));
                }
            });
        }
        "tui.herdr" => cfg.tui.herdr = Some(parse_bool(key, value)?),
        "tools.bash.background_after_ms" => {
            cfg.tools.bash.background_after_ms = Some(parse_value(key, value)?)
        }
        "tools.read.max_image_mb" => cfg.tools.read.max_image_mb = Some(parse_value(key, value)?),
        "tools.web.allow_private_hosts" => {
            cfg.tools.web.allow_private_hosts = Some(parse_bool(key, value)?)
        }
        "tools.mcp.discovery" => {
            cfg.tools.mcp.discovery = Some(match value {
                "eager" => ka_engine::config::McpDiscovery::Eager,
                "lazy" => ka_engine::config::McpDiscovery::Lazy,
                other => {
                    return Err(format!(
                        "-- tools.mcp.discovery: expected eager|lazy, got {other:?}"
                    ));
                }
            });
        }
        "git.auto_commit" => cfg.git.auto_commit = Some(parse_bool(key, value)?),
        "guards.spend_usd" => cfg.guards.spend_usd = Some(parse_value(key, value)?),
        "guards.context_pct" => cfg.guards.context_pct = Some(parse_value(key, value)?),
        "guards.auto_review" => cfg.guards.auto_review = Some(parse_bool(key, value)?),
        "context.promote" => cfg.context.promote = Some(parse_bool(key, value)?),
        "context.autocompact_pct"
        | "context.idle_minutes"
        | "context.idle_compact_pct"
        | "context.recap_minutes" => {
            let parsed: Option<u64> = match value {
                "off" | "unset" | "" => None,
                other => Some(parse_value(key, other)?),
            };
            match key {
                "context.autocompact_pct" => cfg.context.autocompact_pct = parsed,
                "context.idle_minutes" => cfg.context.idle_minutes = parsed,
                "context.idle_compact_pct" => cfg.context.idle_compact_pct = parsed,
                "context.recap_minutes" => cfg.context.recap_minutes = parsed,
                _ => unreachable!("matched arm"),
            }
        }
        "pokes.max" => cfg.pokes.max = Some(parse_value(key, value)?),
        "update.repo" => cfg.update.repo = Some(value.to_string()),
        "update.check" => {
            cfg.update.check = Some(match value {
                "startup" => ka_engine::config::UpdateCheck::Startup,
                "off" => ka_engine::config::UpdateCheck::Off,
                other => {
                    return Err(format!(
                        "-- update.check: expected startup|off, got {other:?}"
                    ));
                }
            });
        }
        "sandbox.mode" => {
            if value != "off" && value != "fs" {
                return Err(format!("-- sandbox.mode: expected off|fs, got {value:?}"));
            }
            cfg.sandbox.mode = Some(value.to_string());
        }
        "shell.env" => {
            let mode = match value {
                "all" => ShellEnvMode::All,
                "core" => ShellEnvMode::Core,
                "none" => ShellEnvMode::None,
                other => {
                    return Err(format!(
                        "-- shell.env: expected all|core|none, got {other:?}"
                    ));
                }
            };
            cfg.shell.env = Some(mode);
        }
        "lsp.enable" => cfg.lsp.enable = Some(parse_bool(key, value)?),
        "lsp.write_through" => cfg.lsp.write_through = Some(parse_bool(key, value)?),
        "debug.enable" => cfg.debug.enable = Some(parse_bool(key, value)?),
        "verify.test" => cfg.verify.test = Some(value.to_string()),
        "prompt.identity_file" => cfg.prompt.identity_file = Some(value.to_string()),
        "prompt.identity_inline" => cfg.prompt.identity_inline = Some(value.to_string()),
        "features.disable" => cfg.features.disable = list(value),
        "features.allow" => cfg.features.allow = list(value),
        "project_doc_fallback_filenames" => cfg.project_doc_fallback_filenames = list(value),
        "fallback.models" => cfg.fallback.models = list(value),
        "sandbox.allow_write" => cfg.sandbox.allow_write = list(value),
        other => {
            // known_config_key is the gatekeeper: reaching here with an
            // unlisted key means the table and the const drifted
            unreachable!("key {other:?} passed known_config_key but has no arm");
        }
    }
    Ok(())
}

/// Every key `ka config set` / the `KA_*` env layer accepts. The
/// union-by-name and priority semantics live in `Config::overlay`
/// (CFG-03); this table only gates spelling. Keep in sync with
/// `apply_config_key` — the drift test proves it.
const KNOWN_CONFIG_KEYS: &[&str] = &[
    "model",
    "effort",
    "mode",
    "roles.default",
    "roles.fast",
    "shell.env",
    "tui.header_glyph",
    "tui.bell",
    "tui.notify",
    "tui.mouse",
    "tui.herdr",
    "tools.bash.background_after_ms",
    "tools.read.max_image_mb",
    "tools.web.allow_private_hosts",
    "tools.mcp.discovery",
    "git.auto_commit",
    "guards.spend_usd",
    "guards.context_pct",
    "guards.auto_review",
    "context.promote",
    "context.autocompact_pct",
    "context.idle_minutes",
    "context.idle_compact_pct",
    "context.recap_minutes",
    "pokes.max",
    "update.repo",
    "update.check",
    "sandbox.mode",
    "lsp.enable",
    "lsp.write_through",
    "debug.enable",
    "verify.test",
    "prompt.identity_file",
    "prompt.identity_inline",
    "features.disable",
    "features.allow",
    "project_doc_fallback_filenames",
    "fallback.models",
    "sandbox.allow_write",
];

/// Whether a key spelling is settable at all (set table / env layer).
fn known_config_key(key: &str) -> bool {
    KNOWN_CONFIG_KEYS.contains(&key)
}

/// `--profile <name>` layer path: `~/.config/ka/profiles/<name>.toml`,
/// loaded between the user layer and the project layer.
fn profile_layer_path(name: &str) -> Result<PathBuf, String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "--profile {name:?}: profile names are [a-zA-Z0-9_-] (they name a file)"
        ));
    }
    let dir = ka_engine::config::user_config_path()
        .parent()
        .map(|p| p.join("profiles"))
        .unwrap_or_else(|| PathBuf::from("profiles"));
    Ok(dir.join(format!("{name}.toml")))
}

/// Print one key's effective value from the resolved config chain —
/// the same table as [`apply_config_key`].
fn config_get(cfg: &Config, key: &str) -> Result<String, String> {
    let s = |v: &Option<String>| v.clone().unwrap_or_else(|| "(unset)".into());
    match key {
        "model" => Ok(s(&cfg.model)),
        "effort" => Ok(cfg
            .effort
            .map(|e| format!("{e:?}").to_lowercase())
            .unwrap_or_else(|| "(unset)".into())),
        "mode" => Ok(format!("{:?}", cfg.effective_mode()).to_lowercase()),
        "roles.default" => Ok(s(&cfg.roles.default)),
        "roles.fast" => Ok(s(&cfg.roles.fast)),
        "tui.header_glyph" => Ok(s(&cfg.tui.header_glyph)),
        "tui.bell" => Ok(cfg.effective_bell().to_string()),
        "tui.notify" => Ok(s(&cfg.tui.notify)),
        "tui.mouse" => Ok(if cfg.effective_mouse_capture() {
            "capture"
        } else {
            "native"
        }
        .to_string()),
        "tui.herdr" => Ok(cfg.tui.herdr.unwrap_or(false).to_string()),
        "tools.bash.background_after_ms" => {
            Ok(cfg.effective_bash_background_after_ms().to_string())
        }
        "tools.read.max_image_mb" => Ok(cfg.effective_max_image_mb().to_string()),
        "tools.web.allow_private_hosts" => Ok(cfg.effective_web_allow_private().to_string()),
        "tools.mcp.discovery" => Ok(if cfg.effective_mcp_lazy() {
            "lazy"
        } else {
            "eager"
        }
        .to_string()),
        "git.auto_commit" => Ok(cfg.git.auto_commit.unwrap_or(false).to_string()),
        "guards.spend_usd" => Ok(cfg
            .guards
            .spend_usd
            .map(|v| v.to_string())
            .unwrap_or_else(|| "(unset)".into())),
        "guards.context_pct" => Ok(cfg
            .guards
            .context_pct
            .map(|v| v.to_string())
            .unwrap_or_else(|| "(unset)".into())),
        "guards.auto_review" => Ok(cfg.guards.auto_review.unwrap_or(false).to_string()),
        "context.promote" => Ok(cfg.effective_context_promote().to_string()),
        "context.autocompact_pct" => Ok(cfg
            .context
            .autocompact_pct
            .map(|v| v.to_string())
            .unwrap_or_else(|| "default".into())),
        "context.idle_minutes" => Ok(cfg
            .context
            .idle_minutes
            .map(|v| v.to_string())
            .unwrap_or_else(|| "off".into())),
        "context.idle_compact_pct" => Ok(cfg
            .context
            .idle_compact_pct
            .map(|v| v.to_string())
            .unwrap_or_else(|| "off".into())),
        "context.recap_minutes" => Ok(cfg
            .context
            .recap_minutes
            .map(|v| v.to_string())
            .unwrap_or_else(|| "off".into())),
        "pokes.max" => Ok(cfg.pokes.max.unwrap_or(3).to_string()),
        "update.repo" => Ok(s(&cfg.update.repo)),
        "update.check" => Ok(match cfg.update.effective_check() {
            ka_engine::config::UpdateCheck::Startup => "startup".into(),
            ka_engine::config::UpdateCheck::Off => "off".into(),
        }),
        "sandbox.mode" => Ok(s(&cfg.sandbox.mode)),
        "shell.env" => Ok(match cfg.shell.env {
            Some(ShellEnvMode::All) | None => "all".into(),
            Some(ShellEnvMode::Core) => "core".into(),
            Some(ShellEnvMode::None) => "none".into(),
        }),
        "lsp.enable" => Ok(cfg.lsp.enable.unwrap_or(false).to_string()),
        "lsp.write_through" => Ok(cfg.lsp.write_through.unwrap_or(false).to_string()),
        "debug.enable" => Ok(cfg.debug.enable.unwrap_or(false).to_string()),
        "verify.test" => Ok(s(&cfg.verify.test)),
        "prompt.identity_file" => Ok(s(&cfg.prompt.identity_file)),
        "features.disable" => Ok(cfg.features.disable.join(",")),
        "features.allow" => Ok(if cfg.features.allow.is_empty() {
            "(all tools)".into()
        } else {
            cfg.features.allow.join(",")
        }),
        "project_doc_fallback_filenames" => Ok(cfg.project_doc_fallback_filenames.join(",")),
        "fallback.models" => Ok(cfg.fallback.models.join(",")),
        "sandbox.allow_write" => Ok(cfg.sandbox.allow_write.join(",")),
        other => Err(format!(
            "unknown key {other:?} — the settable table matches `ka config set`; \
list-of-table keys are file-edited only"
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
    // Leaf-merge ALL user overlays against the embedded catalog in one
    // pass: plain `Catalog::parse` + `overlay` would hard-error on a
    // partial row (missing `wire`) and whole-row-replace the fields the
    // user did not mention (base_url, api_key_env, flags). Accumulating
    // the texts before the merge means file B's leaves compose with
    // file A's instead of reverting them to embedded.
    let texts: Vec<String> = overlays
        .iter()
        .map(|path| std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display())))
        .collect::<Result<Vec<_>, _>>()?;
    let str_refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    // bare texts cannot name their file inside the merge; list the
    // candidate overlays so a syntax error is traceable to a file
    let catalog =
        ka_dialect::dialects::Catalog::merged_embedded_with_overlays(str_refs).map_err(|e| {
            let files = overlays
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!("catalog overlay failed ({files}): {e}")
        })?;
    Ok(catalog)
}

/// Headless auto-deny: pick the deny option of an ask, which by
/// convention is the last one. Hardcoding 2 was only right while every
/// ask had exactly three options.
fn deny_choice(questions: &[ka_protocol::AskQuestion]) -> usize {
    questions
        .first()
        .map_or(2, |q| q.options.len().saturating_sub(1))
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
        disable: cli.disable,
        enable: cli.enable,
        sandbox: cli.sandbox,
        configs: cli.configs,
        dialects: cli.dialects,
        no_discovery: cli.no_discovery,
        trust: cli.trust,
        restore: cli.restore,
        safe_mode: cli.safe_mode,
        interactive_prompt: cli.interactive_prompt,
        effort: cli.effort,
        image: cli.image,
        offline: cli.offline,
        system_prompt: cli.system_prompt,
        tools: cli.tools,
        exclude_tools: cli.exclude_tools,
        ignore_user_config: cli.ignore_user_config,
        ignore_rules: cli.ignore_rules,
        ephemeral: cli.ephemeral,
        profile: cli.profile,
        add_dirs: cli.add_dirs,
        prewalk: cli.prewalk,
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
            restore,
            schema,
            print,
            review,
            rpc,
            effort,
            image,
            offline,
            system_prompt,
            tools,
            exclude_tools,
            ignore_user_config,
            ignore_rules,
            ephemeral: _,
            profile,
            add_dirs,
            prewalk,
            out,
            max_turns,
            max_budget_usd,
            max_cost,
            warn_cost,
            max_time,
        }) => {
            // --review wraps the prompt (or a default) as a read-only
            // review of the working tree — same preset as /review
            let prompt = if review {
                Some(wrap_review_prompt(prompt.as_deref()))
            } else {
                prompt
            };
            let mode = if review && mode.is_none() {
                Some("readonly".to_string())
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
            let images = image_parts(&image)?;
            if !image.is_empty()
                && prompt.is_none()
                && std::io::IsTerminal::is_terminal(&std::io::stdin())
            {
                return Err(
                    "--image needs a prompt: pass one positionally or pipe it on stdin".to_string(),
                );
            }
            let features = feature_flags(disable, enable, sandbox, &exclude_tools)?;
            let opts = RunOpts {
                images,
                out,
                max_turns,
                max_budget_usd,
                max_cost,
                warn_cost,
                max_time,
                effort: parse_effort(effort.as_deref())?,
                system_prompt,
                tools_allow: tools,
                offline,
                ignore_user_config,
                ignore_rules,
                profile,
                add_dirs,
                rpc,
                restore,
                prewalk,
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
                print,
                features,
                opts,
            )
            .await
        }
        Some(CliCommand::Acp { configs }) => acp::run(&configs).await,
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
        Some(CliCommand::Serve {
            addr,
            token,
            configs,
        }) => serve::run(&addr, token, &configs).await,
        Some(CliCommand::Doctor { net, json }) => doctor::run(net, json).await,
        Some(CliCommand::Update { channel, check }) => {
            let trust = trust_for_cwd(false);
            let cfg = load_config(
                &[],
                None,
                None,
                trusted_project_cwd(trust),
                &FeatureFlags::default(),
                false,
                None,
            )?;
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
            query,
            update,
        }) => {
            if update {
                let json = ka_dialect::dialects::fetch_models_dev()
                    .await
                    .map_err(|e| format!("models.dev fetch: {e}"))?;
                let root = ka_strand::data_dir();
                ka_dialect::dialects::store_runtime_catalog(&root, &json)
                    .map_err(|e| format!("store catalog: {e}"))?;
                println!(
                    "runtime catalog stored at {} (loads before embedded + user overlays)",
                    root.join("catalog.json").display()
                );
                return Ok(ExitCode::SUCCESS);
            }
            let mut catalog = load_catalog(&dialects)?;
            if !no_discovery {
                ka_dialect::discovery::overlay_discovered(&mut catalog).await;
            }
            let needle = query.as_deref().map(str::to_lowercase);
            let mut rows: Vec<(&String, &ka_dialect::Dialect)> = catalog
                .dialects
                .iter()
                .filter(|(id, _)| {
                    needle
                        .as_deref()
                        .is_none_or(|q| id.to_lowercase().contains(q))
                })
                .collect();
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
        Some(CliCommand::McpServe { tools, read_only }) => {
            mcp_serve::run(mcp_serve::ServeOpts { tools, read_only }).await
        }
        Some(CliCommand::Agents) => run_agents(),
        Some(CliCommand::Providers) => run_providers(),
        Some(CliCommand::Init { no_import }) => run_init(no_import),
        Some(CliCommand::Action { cmd }) => action::run(cmd),
        Some(CliCommand::Schedule { cmd }) => schedule::run(cmd),
        Some(CliCommand::Recipe {
            name,
            vars,
            dry_run,
        }) => {
            let overrides = parse_var_overrides(&vars)?;
            recipes::run(&name, &overrides, dry_run).await
        }
        Some(CliCommand::Worktree { cmd }) => worktree::run(cmd),
        Some(CliCommand::Sessions { json, archived }) => run_sessions_archived(json, archived),
        Some(CliCommand::Gc {
            dry_run,
            max_age_days,
        }) => run_gc(dry_run, max_age_days),
        Some(CliCommand::Session { cmd }) => run_session_mgmt(cmd),
        Some(CliCommand::Rewind { turns }) => run_rewind(turns).await,
        Some(CliCommand::Export {
            out,
            session,
            html,
            jsonl,
        }) => run_export(out, session, html, jsonl),
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
        Some(CliCommand::Sandbox { fs, command }) => run_sandbox(fs, &command),
        Some(CliCommand::Queue {
            session,
            message,
            addr,
            token,
        }) => run_queue(&session, &message, &addr, token.as_deref()).await,
        Some(CliCommand::ImportSession { format, path, cwd }) => {
            let target = cwd
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let report = session_import::import_session(&format, &path, &target)?;
            println!(
                "imported {} record(s) ({} mapped, {} skipped) → {}",
                report.records,
                report.mapped,
                report.skipped,
                report.path.display()
            );
            for note in &report.notes {
                println!("note: {note}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(CliCommand::Rules {
            cmd:
                RulesCommand::Check {
                    command,
                    tool,
                    configs,
                },
        }) => {
            let trust = trust_for_cwd(false);
            let cfg = load_config(
                &configs,
                None,
                None,
                trusted_project_cwd(trust),
                &FeatureFlags::default(),
                false,
                None,
            )?;
            let joined = command.join(" ");
            let rules = cfg.effective_rules();
            if rules.is_empty() {
                println!("no rules configured — the gate falls through to mode logic");
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "evaluating {} rule(s) for {} `{}`:",
                rules.len(),
                tool,
                joined
            );
            // the gate's first-match-wins, replayed rule by rule: the
            // first suffix that still yields a verdict identifies the
            // matching rule
            let mut matched: Option<(usize, &ka_engine::config::Rule)> = None;
            for (idx, rule) in rules.iter().enumerate() {
                if ka_engine::config::evaluate_rules(&rules[idx..], &tool, &joined).is_some() {
                    matched = Some((idx, rule));
                    break;
                }
            }
            match matched {
                Some((idx, rule)) => {
                    println!(
                        "  rule #{}: {} {} → {}",
                        idx + 1,
                        rule.tool,
                        rule.pattern.as_deref().unwrap_or("(any)"),
                        match rule.verdict {
                            ka_engine::config::Verdict::Allow => "allow",
                            ka_engine::config::Verdict::Ask => "ask",
                            ka_engine::config::Verdict::Deny => "deny",
                        }
                    );
                    println!(
                        "  gate verdict: {}",
                        match rule.verdict {
                            ka_engine::config::Verdict::Allow => "allow (runs without asking)",
                            ka_engine::config::Verdict::Ask => "ask",
                            ka_engine::config::Verdict::Deny => "deny",
                        }
                    );
                }
                None => {
                    println!("  no rule matches — falls through to mode logic");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(CliCommand::Passport { cmd }) => run_passport(cmd).await,
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
                let cfg = load_config(
                    &configs,
                    None,
                    None,
                    trusted_project_cwd(trust),
                    &FeatureFlags::default(),
                    false,
                    None,
                )?;
                let text =
                    toml::to_string_pretty(&cfg).map_err(|e| format!("serialize config: {e}"))?;
                print!("{text}");
                Ok(ExitCode::SUCCESS)
            }
            ConfigCommand::Set { key, value } => {
                let path = config_set(&key, &value)?;
                println!("{} — set {key} = {value}", path.display());
                Ok(ExitCode::SUCCESS)
            }
            ConfigCommand::Get { key, configs } => {
                let trust = trust_for_cwd(false);
                let cfg = load_config(
                    &configs,
                    None,
                    None,
                    trusted_project_cwd(trust),
                    &FeatureFlags::default(),
                    false,
                    None,
                )?;
                let text = config_get(&cfg, &key)?;
                println!("{text}");
                Ok(ExitCode::SUCCESS)
            }
            ConfigCommand::Path => {
                println!(
                    "user:    {}",
                    ka_engine::config::user_config_path().display()
                );
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let project = ka_engine::project_root(&cwd).join(".ka/ka.toml");
                let trusted = ka_engine::trust::project_trusted(&ka_engine::project_root(&cwd));
                println!(
                    "project: {} ({})",
                    project.display(),
                    if trusted { "trusted" } else { "untrusted" }
                );
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

/// Per-run options for `ka run` beyond the shared model/mode/features
/// flags: cost/time caps, output file, and session-shaping overrides
/// (effort, system prompt, tool allowlist). `offline` implies no
/// discovery and no web tools; `ignore_user_config` drops the user
/// config layer for this run.
#[derive(Debug, Default)]
struct RunOpts {
    images: Vec<ka_protocol::ImagePart>,
    out: Option<PathBuf>,
    max_turns: Option<u32>,
    max_budget_usd: Option<f64>,
    max_cost: Option<f64>,
    warn_cost: Option<f64>,
    max_time: Option<u64>,
    effort: Option<ka_protocol::Effort>,
    system_prompt: Option<String>,
    tools_allow: Vec<String>,
    offline: bool,
    ignore_user_config: bool,
    ignore_rules: bool,
    profile: Option<String>,
    add_dirs: Vec<PathBuf>,
    rpc: bool,
    restore: bool,
    prewalk: bool,
}

/// Exit code for a cost/time cap breach (distinct from abort=2 and
/// error=1 so scripts can tell "I hit my own cap" from failures).
const EXIT_CAP: u8 = 3;

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
    print: String,
    features: FeatureFlags,
    opts: RunOpts,
) -> Result<ExitCode, String> {
    let trust = trust_for_cwd(force_trust);
    warn_untrusted_conventions(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mut cfg = load_config(
        configs,
        model,
        mode,
        trusted_project_cwd(trust),
        &features,
        opts.ignore_user_config,
        match opts.profile.as_deref() {
            Some(name) => {
                let path = profile_layer_path(name)?;
                if !path.exists() {
                    return Err(format!(
                        "--profile {name}: no profile file at {}",
                        path.display()
                    ));
                }
                Some(path)
            }
            None => None,
        },
    )?;
    apply_run_opts(&mut cfg, &opts);
    if !opts.add_dirs.is_empty() {
        for dir in &opts.add_dirs {
            if !cfg
                .sandbox
                .allow_write
                .contains(&dir.to_string_lossy().to_string())
            {
                cfg.sandbox
                    .allow_write
                    .push(dir.to_string_lossy().to_string());
            }
        }
    }
    let with_discovery = with_discovery && !opts.offline;
    let cfg_model = cfg.model.clone();
    let catalog = build_catalog(dialects, with_discovery).await?;
    let choice = if let Some(id) = session {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        resolve_session(&cwd, &id)?
    } else if continue_latest || opts.restore {
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
    // --restore: resolve the strand file up front (choice is consumed
    // by spawn below); the pending prompt rides the pre-synthesis file
    let restore_path = if opts.restore {
        match &choice {
            ka_engine::StrandChoice::Path(p) => Some(p.clone()),
            ka_engine::StrandChoice::Latest => {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                ka_strand::latest(&cwd)
                    .map_err(|e| format!("listing strands: {e}"))?
                    .map(|s| s.path)
            }
            ka_engine::StrandChoice::New => None,
        }
    } else {
        None
    };
    let mut handle = spawn_full(cfg, catalog, choice);
    if opts.rpc {
        // the stdin side of the protocol IS the prompt channel: reading
        // a prompt here would swallow the host's command lines
        return rpc_loop(handle).await;
    }
    let prompt = if opts.restore {
        let path = restore_path.ok_or("--restore: nothing to restore (no prior session)")?;
        let strand = ka_strand::StrandFile::open(&path)
            .map_err(|e| format!("--restore {}: {e}", path.display()))?;
        Some(
            strand
                .pending_user_prompt()
                .ok_or("--restore: no interrupted turn to restore (the last turn completed)")?,
        )
    } else {
        prompt
    };
    let prompt = match prompt {
        Some(p) => p,
        None => read_stdin()?,
    };
    if prompt.trim().is_empty() {
        return Err("empty prompt".to_string());
    }

    handle
        .commands
        .send(Command::Prompt {
            allowed_tools: None,
            model: None,
            text: prompt,
            schema,
            images: opts.images,
            skills: Vec::new(),
        })
        .await
        .map_err(|_| "engine closed before prompt".to_string())?;

    let mut stdout = std::io::stdout().lock();
    let mut final_stop: Option<Stop> = None;
    let mut idle_seen = false;
    let mut final_text = String::new();
    let mut total_cost = 0.0f64;
    let mut sj = StreamJsonPrinter::with_model(cfg_model.clone());
    // wall-clock cap: on elapse the turn is aborted and the run exits
    // with the cap code, distinct from a user abort
    let deadline = opts.max_time.map(|s| {
        tokio::time::Instant::from_std(
            std::time::Instant::now() + std::time::Duration::from_secs(s),
        )
    });
    while let Some(event) = match deadline {
        Some(d) => tokio::time::timeout_at(d, handle.events.recv())
            .await
            .unwrap_or(None),
        None => handle.events.recv().await,
    } {
        // headless policy: permission asks auto-deny (last option = deny)
        if let Event::Ask { id, questions } = &event {
            handle
                .commands
                .send(Command::Answer {
                    question: id.clone(),
                    choice: deny_choice(questions),
                })
                .await
                .map_err(|_| "engine closed during ask")?;
        }
        match &event {
            Event::TurnFinished { stop, usage } => {
                final_stop = Some(*stop);
                total_cost += usage.cost;
            }
            Event::Idle => {
                idle_seen = true;
                break;
            }
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
    if deadline.is_some() && !idle_seen {
        // the timeout fired: abort whatever is still running and report
        // the cap (the events channel may still carry the abort's tail)
        let _ = handle.commands.send(Command::Abort).await;
        eprintln!("ka: --max-time reached; run aborted");
        return Ok(ExitCode::from(EXIT_CAP));
    }
    // stream-json: the terminating result line (after the loop so cost
    // and stop reason are final)
    if print == "stream-json" {
        for line in sj.finish(final_stop) {
            use std::io::Write;
            writeln!(stdout, "{line}").map_err(|e| format!("stdout: {e}"))?;
        }
    }
    // text: the final answer only — to --out when given, else stdout
    if print == "text" && !final_text.is_empty() {
        if let Some(out) = &opts.out {
            std::fs::write(out, &final_text)
                .map_err(|e| format!("--out {}: {e}", out.display()))?;
        } else {
            use std::io::Write;
            writeln!(stdout, "{final_text}").map_err(|e| format!("stdout: {e}"))?;
        }
    }
    std::io::Write::flush(&mut stdout).map_err(|e| format!("stdout: {e}"))?;
    if let Some(warn) = opts.warn_cost {
        if total_cost > warn {
            eprintln!("ka: cost ${warn:.2} exceeded (session: ${total_cost:.4})");
        }
    }
    if let Some(max) = opts.max_cost {
        if total_cost > max {
            eprintln!("ka: --max-cost ${max:.2} exceeded (session: ${total_cost:.4})");
            return Ok(ExitCode::from(EXIT_CAP));
        }
    }
    // the loop only exits without Idle when the events channel closed
    // (engine task gone) — same "no final stop" condition
    exit_code_for(final_stop, idle_seen, idle_seen)
}

/// Apply the run-level overrides onto the merged config, before spawn:
/// effort/system-prompt/tool-allowlist shape the session, the cost
/// budget rides the existing spend guard, max_turns caps steps, and
/// offline drops the web hands entirely.
fn apply_run_opts(cfg: &mut Config, opts: &RunOpts) {
    if let Some(effort) = opts.effort {
        cfg.effort = Some(effort);
    }
    if let Some(text) = &opts.system_prompt {
        cfg.prompt.identity_inline = Some(text.clone());
    }
    if !opts.tools_allow.is_empty() {
        cfg.features.allow = opts.tools_allow.clone();
    }
    if let Some(budget) = opts.max_budget_usd {
        cfg.guards.spend_usd = Some(budget);
    }
    if let Some(turns) = opts.max_turns {
        cfg.max_steps = Some(turns);
    }
    if opts.ignore_rules {
        // `--ignore-rules`: [[rules]] tables and the rule-file
        // conventions both out (the conventions gate is set in main
        // before the runtime spawns); hardstops are untouched
        cfg.rules.clear();
    }
    if opts.prewalk {
        cfg.context.prewalk = Some(true);
    }
    if opts.offline {
        cfg.update.check = Some(ka_engine::config::UpdateCheck::Off);
        let spec = "web".parse::<ka_protocol::FeatureSpec>();
        if let Ok(spec) = spec {
            let s = spec.to_string();
            if !cfg.features.disable.contains(&s) {
                cfg.features.disable.push(s);
            }
        }
    }
}

/// `ka sandbox <cmd>`: run one command under the session's sandbox
/// policy — the same bwrap/firejail/landlock wrapper bash children
/// get, exposed for scripts and CI. Exit code passes through.
fn run_sandbox(fs: bool, command: &[String]) -> Result<ExitCode, String> {
    if command.is_empty() {
        return Err(
            "ka sandbox: a command is required (e.g. `ka sandbox -- cargo build`)".to_string(),
        );
    }
    let trust = trust_for_cwd(false);
    let mut cfg = load_config(
        &[],
        None,
        None,
        trusted_project_cwd(trust),
        &FeatureFlags::default(),
        false,
        None,
    )?;
    if fs {
        cfg.sandbox.mode = Some("fs".into());
    }
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let policy = ka_sandbox::policy_from_config(&cfg.sandbox.to_policy_config(), &cwd)?;
    if matches!(policy, ka_sandbox::Policy::Off) {
        return Err(
            "ka sandbox: the sandbox is off — set [sandbox] mode = \"fs\" or pass --fs".to_string(),
        );
    }
    let line = command.join(" ");
    let argv = ka_sandbox::wrap_command(&policy, &line, &cwd)?;
    let (program, args) = argv.split_first().ok_or("empty sandbox argv")?;
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .map_err(|e| format!("spawn {program}: {e}"))?;
    Ok(status
        .code()
        .and_then(|c| u8::try_from(c).ok())
        .map(ExitCode::from)
        .unwrap_or(ExitCode::FAILURE))
}

/// One blocking-ish HTTP/1.1 request against `ka serve` (queue's
/// client; Connection: close, bounded by a 10 s timeout).
async fn http_json_request(
    addr: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> Result<(u16, String), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let sock = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .map_err(|_| format!("connect {addr}: timed out"))?
    .map_err(|e| format!("connect {addr}: {e}"))?;
    let (mut reader, mut writer) = sock.into_split();
    let body = body.unwrap_or("");
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{auth}Content-Length: {}\r\nAccept: */*\r\n\r\n{body}",
        body.len()
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        writer.write_all(req.as_bytes()),
    )
    .await
    .map_err(|_| "write: timed out")?
    .map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        reader.read_to_end(&mut buf),
    )
    .await
    .map_err(|_| "read: timed out")?
    .map_err(|e| format!("read: {e}"))?;
    let raw = String::from_utf8_lossy(&buf);
    let (head, resp_body) = raw
        .split_once("\r\n\r\n")
        .ok_or_else(|| "malformed HTTP response".to_string())?;
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| "malformed HTTP status".to_string())?;
    Ok((status, resp_body.to_string()))
}

/// `ka queue --session <id> --message <text>`: the write path against a
/// live `ka serve` — resolve the session by id prefix, memorable name
/// or title substring (unique match required), then POST the prompt.
async fn run_queue(
    session: &str,
    message: &str,
    addr: &str,
    token: Option<&str>,
) -> Result<ExitCode, String> {
    let token = token.map(String::from).or_else(|| {
        std::env::var("KA_SERVE_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
    });
    let (status, body) =
        http_json_request(addr, "GET", "/sessions", token.as_deref(), None).await?;
    if status != 200 {
        return Err(format!("queue: HTTP {status} listing sessions: {body}"));
    }
    let value: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("sessions: invalid JSON: {e}"))?;
    let list = value.as_array().ok_or("sessions: unexpected shape")?;
    let mut hits: Vec<(String, String)> = Vec::new();
    for s in list {
        let id = s.get("id").and_then(|v| v.as_str()).unwrap_or_default();
        let title = s.get("title").and_then(|v| v.as_str()).unwrap_or_default();
        let name = s.get("name").and_then(|v| v.as_str()).unwrap_or_default();
        let hit = id.starts_with(session)
            || (!name.is_empty() && name == session)
            || (!title.is_empty() && title.contains(session));
        if hit {
            hits.push((id.to_string(), format!("{title} [{id}]")));
        }
    }
    match hits.len() {
        0 => return Err(format!("queue: no live session matches {session:?}")),
        1 => {}
        _ => {
            let shown: Vec<String> = hits.iter().take(5).map(|(_, l)| l.clone()).collect();
            return Err(format!(
                "queue: ambiguous session {session:?} — {} candidate(s): {}",
                hits.len(),
                shown.join(", ")
            ));
        }
    }
    let (id, label) = &hits[0];
    let payload = serde_json::json!({ "text": message }).to_string();
    let (status, body) = http_json_request(
        addr,
        "POST",
        &format!("/sessions/{id}/prompt"),
        token.as_deref(),
        Some(&payload),
    )
    .await?;
    if status != 200 {
        return Err(format!("queue: HTTP {status}: {body}"));
    }
    println!("queued into {label}");
    Ok(ExitCode::SUCCESS)
}

/// CLI-15 · `ka run --rpc`: bidirectional JSONL protocol over stdio.
/// stdin carries one Command per line in the same serde shape the
/// NDJSON output uses (e.g. `{"Prompt":{"text":"..."}}`,
/// `{"Answer":{"question":"…","choice":0}}`, `{"Abort":null}`); stdout
/// carries the Event stream (one JSON object per line). Asks are NOT
/// auto-denied in this mode — the host decides. Ends when stdin closes
/// and the engine settles.
async fn rpc_loop(handle: ka_engine::EngineHandle) -> Result<ExitCode, String> {
    use tokio::io::AsyncWriteExt;
    use tokio::sync::mpsc;
    let ka_engine::EngineHandle {
        commands,
        mut events,
    } = handle;
    let (lines_tx, mut lines_rx) = mpsc::channel::<String>(64);
    tokio::task::spawn_blocking(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if lines_tx.blocking_send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let mut stdout = tokio::io::stdout();
    let mut final_stop: Option<Stop> = None;
    let mut idle_seen = false;
    let mut stdin_open = true;
    loop {
        tokio::select! {
            line = lines_rx.recv(), if stdin_open => {
                match line {
                    Some(l) => {
                        let t = l.trim();
                        if t.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Command>(t) {
                            Ok(c) => {
                                if commands.send(c).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => eprintln!("ka rpc: bad command line: {e}"),
                        }
                    }
                    None => stdin_open = false,
                }
            }
            event = events.recv() => {
                let Some(event) = event else { break };
                if let Event::TurnFinished { stop, .. } = &event {
                    final_stop = Some(*stop);
                }
                if let Event::Idle = &event {
                    idle_seen = true;
                }
                let line = to_line(&event).map_err(|e| format!("serialize event: {e}"))?;
                stdout
                    .write_all(line.as_bytes())
                    .await
                    .map_err(|e| format!("stdout: {e}"))?;
                stdout.flush().await.map_err(|e| format!("stdout: {e}"))?;
                if let Event::Idle = &event {
                    if !stdin_open {
                        break;
                    }
                }
            }
        }
    }
    exit_code_for(final_stop, idle_seen, idle_seen)
}

/// Terminal exit status of a headless run. A missing final stop with no
/// Idle marker means the engine died mid-turn (events channel returned
/// None) — that must fail the run (exit 1 via the dispatcher's error
/// path), not exit 0, or CI pipelines sail on broken runs. The
/// stream-json writer already treats the same condition as an error.
fn exit_code_for(
    final_stop: Option<Stop>,
    idle_seen: bool,
    stream_ended_cleanly: bool,
) -> Result<ExitCode, String> {
    match final_stop {
        Some(Stop::Aborted) => Ok(ExitCode::from(2)),
        Some(Stop::Error) => Ok(ExitCode::from(1)),
        Some(_) => Ok(ExitCode::SUCCESS),
        None if idle_seen && stream_ended_cleanly => Ok(ExitCode::SUCCESS),
        None => Err("engine ended before the turn finished".to_string()),
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
    // --restore: crash recovery — resume the newest strand and re-ask
    // its dangling user turn. Mutually exclusive with --session and -i.
    let restore_path = if cli.restore {
        if cli.session.is_some() {
            return Err("--restore and --session are mutually exclusive".to_string());
        }
        if cli.interactive_prompt.is_some() {
            return Err("--restore and -i are mutually exclusive".to_string());
        }
        Some(match ka_engine::read_waypoint() {
            Some((way_cwd, path)) if same_dir(&way_cwd, &cwd) && path.exists() => path,
            _ => {
                ka_strand::latest(&cwd)
                    .map_err(|e| format!("listing strands: {e}"))?
                    .ok_or("--restore: no strand to restore")?
                    .path
            }
        })
    } else {
        None
    };
    let choice = if let Some(id) = cli.session.clone() {
        resolve_session(&cwd, &id)?
    } else if let Some(path) = &restore_path {
        ka_engine::StrandChoice::Path(path.clone())
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

    let interactive = match &restore_path {
        Some(path) => {
            let strand = ka_strand::StrandFile::open(path)
                .map_err(|e| format!("--restore {path:?}: {e}"))?;
            Some(
                strand
                    .pending_user_prompt()
                    .ok_or("--restore: no interrupted turn to restore (the last turn completed)")?,
            )
        }
        None => cli.interactive_prompt.clone(),
    };
    let trust = trust_for_cwd(cli.trust);
    warn_untrusted_conventions(&cwd);
    if !cli.image.is_empty() && interactive.is_none() {
        return Err("--image needs a prompt: pass -i \"<prompt>\" with it".to_string());
    }
    let features = feature_flags(
        cli.disable.clone(),
        cli.enable.clone(),
        cli.sandbox.clone(),
        &cli.exclude_tools,
    )?;
    let mut cfg = load_config(
        &cli.configs,
        cli.model.clone(),
        cli.mode.clone(),
        trusted_project_cwd(trust),
        &features,
        cli.ignore_user_config,
        match cli.profile.as_deref() {
            Some(name) => {
                let path = profile_layer_path(name)?;
                if !path.exists() {
                    return Err(format!(
                        "--profile {name}: no profile file at {}",
                        path.display()
                    ));
                }
                Some(path)
            }
            None => None,
        },
    )?;
    let run_opts = RunOpts {
        images: image_parts(&cli.image)?,
        effort: parse_effort(cli.effort.as_deref())?,
        system_prompt: cli.system_prompt.clone(),
        tools_allow: cli.tools.clone(),
        offline: cli.offline,
        ignore_user_config: cli.ignore_user_config,
        ignore_rules: cli.ignore_rules,
        prewalk: cli.prewalk,
        ..RunOpts::default()
    };
    apply_run_opts(&mut cfg, &run_opts);
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
    if let Some(prompt) = interactive.clone() {
        commands
            .send(Command::Prompt {
                allowed_tools: None,
                model: None,
                text: prompt,
                schema: None,
                images: run_opts.images,
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
            trusted_project_cwd(trust),
            &features,
            cli.ignore_user_config,
            None,
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
    .await;
    // the agent stops owning the pane the moment the TUI returns — on a
    // clean exit *and* on an error, so a failed TUI never leaves Herdr
    // reporting this pane as working. The reporter flushes a release
    // (bounded by one report deadline).
    if let Some(mut reporter) = herdr.take() {
        reporter.finish();
    }
    let exit = exit.map_err(|e| format!("tui: {e}"))?;
    match exit {
        ka_term::tui::Exit::Quit | ka_term::tui::Exit::EngineEnded => Ok(ExitCode::SUCCESS),
        ka_term::tui::Exit::Update { session } => {
            // /update: run the updater in this terminal, then re-exec
            // attached to the same session — the atomic swap has
            // already landed on disk, so the exec picks up whichever
            // binary is current (new on success, old on failure)
            let trust = trust_for_cwd(false);
            let cfg = load_config(
                &[],
                None,
                None,
                trusted_project_cwd(trust),
                &FeatureFlags::default(),
                false,
                None,
            )?;
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

/// `ka gc`: reclaim spills/snapshots/empty dirs older than the age
/// threshold via the strand store's gc API; strands are only reported.
fn run_gc(dry_run: bool, max_age_days: u64) -> Result<ExitCode, String> {
    let root = ka_strand::data_dir();
    let spills_roots = [root.join("spills"), ka_engine::hands::spills_root()];
    let report = ka_strand::gc(&root, &spills_roots, max_age_days, dry_run)
        .map_err(|e| format!("gc: {e}"))?;
    let verb = if dry_run {
        "would reclaim"
    } else {
        "reclaimed"
    };
    println!(
        "gc ({}) [{}]: {} spill file(s), {} snapshot dir(s), {} empty dir(s) {}",
        if dry_run { "dry-run" } else { "apply" },
        root.display(),
        report.spills_removed,
        report.snapshots_removed,
        report.empty_dirs_removed,
        verb
    );
    if report.strand_bytes_reported > 0 {
        println!(
            "strands (reported only — `ka session delete` removes those): {:.1} MB",
            report.strand_bytes_reported as f64 / (1024.0 * 1024.0)
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `ka sessions`: list strands for this cwd with resolvable ids.
fn run_sessions_archived(json: bool, archived: bool) -> Result<ExitCode, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let strands = if archived {
        ka_strand::list_archived(&cwd)
    } else {
        ka_strand::list(&cwd).map_err(|e| format!("listing sessions: {e}"))?
    };
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
        if archived {
            println!("no archived sessions for {}", cwd.display());
        } else {
            println!("no sessions yet for {}", cwd.display());
        }
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
        // a set name rides beside the title: `fox · fix the pump`;
        // pinned sessions carry a ★ prefix
        let label = match &s.name {
            Some(n) => format!("{n} · {}", s.title),
            None => s.title.clone(),
        };
        let star = if s.pinned { "★ " } else { "" };
        println!(
            "{:<26} {:>5} {:>10}  {}{}",
            s.id,
            s.messages,
            format!("${:.4}", s.cost),
            star,
            label
        );
        if let Some(recap) = &s.recap {
            println!("    └ recap: {}", truncate_str(recap, 100));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Ellipsize a single line to `max` chars (newlines collapsed).
fn truncate_str(s: &str, max: usize) -> String {
    let mut line = s.lines().next().unwrap_or_default().to_string();
    if line.chars().count() > max {
        line = line.chars().take(max).collect::<String>() + "…";
    }
    line
}

/// `ka passport login|logout|status` — claude-plan subscription auth
/// lifecycle over the passport crate (feature `passport`).
#[cfg(feature = "passport")]
async fn run_passport(cmd: PassportCmd) -> Result<ExitCode, String> {
    use ka_passport::TokenSource;
    use ka_passport::claude::{ClaudePlanSource, VENDOR};
    let source = ClaudePlanSource::new();
    match cmd {
        PassportCmd::Login => {
            let tokens = source.login().await.map_err(|e| format!("login: {e}"))?;
            println!("logged in as {VENDOR} (expires unix {})", tokens.expires_at);
            let handle = ka_passport::claude::install();
            println!(
                "passport registered for vendors {:?} — claude-plan rows are live",
                handle.vendors()
            );
            Ok(ExitCode::SUCCESS)
        }
        PassportCmd::Logout => {
            source.logout().map_err(|e| format!("logout: {e}"))?;
            println!("logged out of {VENDOR}");
            Ok(ExitCode::SUCCESS)
        }
        PassportCmd::Status => {
            let st = source.status();
            if st.logged_in {
                println!(
                    "{VENDOR}: logged in (expires unix {}, {})",
                    st.expires_at.map(|e| e.to_string()).unwrap_or_default(),
                    if st.stale { "stale" } else { "fresh" }
                );
            } else {
                println!("{VENDOR}: not logged in");
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(not(feature = "passport"))]
async fn run_passport(_cmd: PassportCmd) -> Result<ExitCode, String> {
    Err("passport auth is not compiled in — rebuild with --features passport".to_string())
}

/// `ka session …` — rename / delete (cascade) / archive / unarchive,
/// resolving the id the same way `--session` does (live tree; unarchive
/// resolves inside the archive tree instead).
fn run_session_mgmt(cmd: SessionCmd) -> Result<ExitCode, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    match cmd {
        SessionCmd::Rename { id, title } => {
            let path = resolve_session_path(&cwd, &id, false)?;
            ka_strand::retitle(&path, &title).map_err(|e| format!("retitle: {e}"))?;
            println!("{} — renamed to {title:?}", path.display());
        }
        SessionCmd::Delete { id } => {
            let path = resolve_session_path(&cwd, &id, false)?;
            let removed = ka_strand::delete_cascade(&path).map_err(|e| format!("delete: {e}"))?;
            println!(
                "deleted {removed} session(s) under {}",
                path.parent().map(|p| p.display()).unwrap_or(path.display())
            );
        }
        SessionCmd::Archive { id } => {
            let path = resolve_session_path(&cwd, &id, false)?;
            let new = ka_strand::archive(&path, true).map_err(|e| format!("archive: {e}"))?;
            println!("archived → {}", new.display());
        }
        SessionCmd::Unpin { id } => {
            let path = resolve_session_path(&cwd, &id, false)?;
            ka_strand::set_pinned(&path, false).map_err(|e| format!("unpin: {e}"))?;
            println!("unpinned {}", path.display());
        }
        SessionCmd::Pin { id } => {
            let path = resolve_session_path(&cwd, &id, false)?;
            ka_strand::set_pinned(&path, true).map_err(|e| format!("pin: {e}"))?;
            println!("pinned ★ {}", path.display());
        }
        SessionCmd::Unarchive { id } => {
            let list = ka_strand::list_archived(&cwd);
            let hits: Vec<_> = list
                .iter()
                .filter(|s| {
                    s.id.starts_with(id.as_str())
                        || s.name.as_deref() == Some(id.as_str())
                        || (!id.is_empty() && s.title.contains(id.as_str()))
                })
                .collect();
            match hits.len() {
                0 => return Err(format!("no archived session matches {id:?}")),
                1 => {
                    let new = ka_strand::archive(&hits[0].path, false)
                        .map_err(|e| format!("unarchive: {e}"))?;
                    println!("unarchived → {}", new.display());
                }
                _ => {
                    let shown: Vec<String> = hits.iter().take(5).map(|s| s.id.clone()).collect();
                    return Err(format!(
                        "ambiguous id {id:?} — {} archived candidate(s): {}",
                        hits.len(),
                        shown.join(", ")
                    ));
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Resolve a session reference to its strand file. `archived` searches
/// the archive tree instead of the live one.
fn resolve_session_path(cwd: &Path, id: &str, archived: bool) -> Result<PathBuf, String> {
    if archived {
        let hits: Vec<_> = ka_strand::list_archived(cwd)
            .into_iter()
            .filter(|s| s.id.starts_with(id) || s.name.as_deref() == Some(id))
            .collect();
        return match hits.len() {
            0 => Err(format!("no archived session matches {id:?}")),
            1 => Ok(hits[0].path.clone()),
            _ => Err(format!("ambiguous id {id:?}")),
        };
    }
    match resolve_session(cwd, id)? {
        ka_engine::StrandChoice::Path(path) => Ok(path),
        _ => Err("session did not resolve to a strand file".to_string()),
    }
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
    let cfg = load_config(
        &[],
        None,
        None,
        trusted_project_cwd(trust),
        &FeatureFlags::default(),
        false,
        None,
    )?;
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
///
/// GIT-06: when foreign agent rule files exist (.cursorrules,
/// .cursor/rules/*.mdc, .github/copilot-instructions.md), their content
/// is imported into the generated AGENTS.md under clearly marked
/// `## Imported rules (…)` sections — the originals are never touched.
/// `--no-import` skips the merge entirely.
fn run_init(no_import: bool) -> Result<ExitCode, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let root = ka_engine::project_root(&cwd);
    let (target, imported) = init_agents_md(&root, !no_import)?;
    println!("wrote {}", target.display());
    println!("edit it to describe real conventions; ka reads it automatically");
    for src in imported {
        println!(
            "imported rules from {} (original left untouched)",
            src.display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Generate AGENTS.md at `root` (refusing to overwrite) and, when
/// `import_rules`, append the foreign rule sections. Returns the written
/// path and the source files whose content was imported.
fn init_agents_md(root: &Path, import_rules: bool) -> Result<(PathBuf, Vec<PathBuf>), String> {
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

    let mut imported = Vec::new();
    if import_rules {
        append_imported_rules(&mut body, root, &mut imported)?;
    }
    std::fs::write(&target, body).map_err(|e| format!("write: {e}"))?;
    Ok((target, imported))
}

/// Append the foreign rule sections onto `body`: cursor sources first
/// (.cursorrules, then .cursor/rules/*.mdc sorted by name), then
/// copilot (.github/copilot-instructions.md). Every source is marked
/// with its origin; the source files themselves are read-only inputs.
fn append_imported_rules(
    body: &mut String,
    root: &Path,
    imported: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let mut cursor_sources: Vec<PathBuf> = Vec::new();
    let cursorrules = root.join(".cursorrules");
    if cursorrules.is_file() {
        cursor_sources.push(cursorrules);
    }
    let rules_dir = root.join(".cursor").join("rules");
    if rules_dir.is_dir() {
        let mut mdc: Vec<PathBuf> = std::fs::read_dir(&rules_dir)
            .map_err(|e| format!("read {}: {e}", rules_dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "mdc"))
            .collect();
        mdc.sort();
        cursor_sources.extend(mdc);
    }
    append_rule_family(body, root, "cursor", &cursor_sources, imported)?;
    let copilot = root.join(".github").join("copilot-instructions.md");
    let copilot_sources = if copilot.is_file() {
        vec![copilot]
    } else {
        Vec::new()
    };
    append_rule_family(body, root, "copilot", &copilot_sources, imported)?;
    Ok(())
}

/// One `## Imported rules (<family>)` section listing every source and
/// embedding each file's content behind an origin marker. Sources whose
/// content is empty/whitespace are skipped entirely (no empty section).
fn append_rule_family(
    body: &mut String,
    root: &Path,
    family: &str,
    sources: &[PathBuf],
    imported: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let mut blocks: Vec<(String, String)> = Vec::new();
    for src in sources {
        let rel = src
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| src.to_string_lossy().into_owned());
        let content = std::fs::read_to_string(src).map_err(|e| format!("read {rel}: {e}"))?;
        let trimmed = content.trim();
        if trimmed.is_empty() {
            continue;
        }
        blocks.push((rel, trimmed.to_string()));
    }
    if blocks.is_empty() {
        return Ok(());
    }
    body.push_str(&format!("\n## Imported rules ({family})\n\n"));
    body.push_str(&format!(
        "Imported by `ka init` from the {family} rule file(s) below — \
         ka reads AGENTS.md, not the sources. The original files are \
         untouched: edit one side or the other, not both.\n"
    ));
    for (rel, content) in blocks {
        body.push_str(&format!(
            "\n<!-- from {rel} — imported by `ka init`; the original file is untouched -->\n\n{content}\n"
        ));
        imported.push(root.join(&rel));
    }
    Ok(())
}

/// `--var k=v` overrides for `ka recipe`: split on the first `=`, the
/// key must be non-empty. Values may contain `=`.
fn parse_var_overrides(args: &[String]) -> Result<Vec<(String, String)>, String> {
    args.iter()
        .map(|arg| {
            let (k, v) = arg
                .split_once('=')
                .ok_or_else(|| format!("--var expects k=v, got {arg:?}"))?;
            if k.is_empty() {
                return Err(format!("--var key must be non-empty, got {arg:?}"));
            }
            Ok((k.to_string(), v.to_string()))
        })
        .collect()
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
    let cfg = load_config(&[], None, None, None, &FeatureFlags::default(), false, None)?;
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
    jsonl: bool,
) -> Result<ExitCode, String> {
    if html && jsonl {
        return Err("--html and --jsonl are mutually exclusive".to_string());
    }
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
    if jsonl {
        let text = ka_strand::render_jsonl(&records).map_err(|e| format!("render jsonl: {e}"))?;
        return match out {
            Some(path) => {
                std::fs::write(&path, &text).map_err(|e| format!("{}: {e}", path.display()))?;
                println!("wrote {}", path.display());
                Ok(ExitCode::SUCCESS)
            }
            None => {
                print!("{text}");
                Ok(ExitCode::SUCCESS)
            }
        };
    }
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

    #[test]
    fn parse_mode_accepts_new_and_old_vocabularies() {
        assert!(matches!(
            parse_mode("dontask").unwrap(),
            ka_protocol::Mode::DontAsk
        ));
        assert!(matches!(
            parse_mode("dont-ask").unwrap(),
            ka_protocol::Mode::DontAsk
        ));
        assert!(parse_mode("dontknow").is_err());
        // the new canonical words
        for (word, want) in [
            ("ask", ka_protocol::Mode::Ask),
            ("accept_edits", ka_protocol::Mode::AcceptEdits),
            ("allow", ka_protocol::Mode::Allow),
            ("readonly", ka_protocol::Mode::Readonly),
        ] {
            assert_eq!(parse_mode(word), Ok(want), "{word}");
        }
        // every older spelling keeps parsing to the same tier
        for (word, want) in [
            ("guarded", ka_protocol::Mode::Ask),
            ("needs-approval", ka_protocol::Mode::Ask),
            ("needs_approval", ka_protocol::Mode::Ask),
            ("accept-edits", ka_protocol::Mode::AcceptEdits),
            ("accept edits", ka_protocol::Mode::AcceptEdits),
            ("free", ka_protocol::Mode::Allow),
            ("full-access", ka_protocol::Mode::Allow),
            ("full_access", ka_protocol::Mode::Allow),
            ("plan", ka_protocol::Mode::Readonly),
        ] {
            assert_eq!(parse_mode(word), Ok(want), "{word}");
        }
        assert!(parse_mode("yolo").is_err());
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

    /// Batch 1: the new session-shaping globals parse onto the CLI.
    #[test]
    fn session_shaping_globals_parse() {
        let cli = Cli::try_parse_from([
            "ka",
            "--effort",
            "high",
            "--offline",
            "--system-prompt",
            "be terse",
            "--tools",
            "read,edit,bash",
            "--ignore-rules",
            "--ephemeral",
            "-i",
            "go",
        ])
        .unwrap();
        assert_eq!(cli.effort.as_deref(), Some("high"));
        assert!(cli.offline);
        assert_eq!(cli.system_prompt.as_deref(), Some("be terse"));
        assert_eq!(cli.tools, vec!["read", "edit", "bash"]);
        assert!(cli.ignore_rules && cli.ephemeral);
        assert!(cli.command.is_none());
        let cli = Cli::try_parse_from([
            "ka",
            "run",
            "--max-turns",
            "4",
            "--max-budget-usd",
            "0.5",
            "--max-cost",
            "1",
            "--warn-cost",
            "0.25",
            "--max-time",
            "120",
            "-o",
            "out.txt",
            "--image",
            "a.png",
            "--image",
            "b.jpg",
            "do it",
        ])
        .unwrap();
        let Some(CliCommand::Run {
            max_turns,
            max_budget_usd,
            max_cost,
            warn_cost,
            max_time,
            out,
            image,
            ..
        }) = cli.command
        else {
            panic!("expected run");
        };
        assert_eq!(max_turns, Some(4));
        assert_eq!(max_budget_usd, Some(0.5));
        assert_eq!(max_cost, Some(1.0));
        assert_eq!(warn_cost, Some(0.25));
        assert_eq!(max_time, Some(120));
        assert_eq!(out.as_deref(), Some(std::path::Path::new("out.txt")));
        assert_eq!(image.len(), 2);
    }

    /// `--effort` rejects unknown levels with the documented spelling.
    #[test]
    fn parse_effort_matches_the_thinking_spelling() {
        assert_eq!(
            parse_effort(Some("max")).unwrap(),
            Some(ka_protocol::Effort::Max)
        );
        assert_eq!(parse_effort(None).unwrap(), None);
        assert!(parse_effort(Some("ultra")).is_err());
    }

    /// `--exclude-tools <name>` maps onto the `tool:<name>` disable
    /// grammar (same shape as `--disable`).
    #[test]
    fn exclude_tools_maps_onto_tool_disable_specs() {
        let flags = feature_flags(
            Vec::new(),
            Vec::new(),
            None,
            &["bash".into(), "web_search".into()],
        )
        .unwrap();
        let mut cfg = Config::default();
        flags.apply(&mut cfg);
        assert!(cfg.features.disable.contains(&"tool:bash".to_string()));
        assert!(
            cfg.features
                .disable
                .contains(&"tool:web_search".to_string())
        );
    }

    /// `--offline` composes: web hands disabled, update check off —
    /// nothing but the model call reaches the network.
    #[test]
    fn run_opts_offline_drop_the_web_and_the_update_check() {
        let mut cfg = Config::default();
        apply_run_opts(
            &mut cfg,
            &RunOpts {
                offline: true,
                ..RunOpts::default()
            },
        );
        assert!(cfg.features.disable.iter().any(|s| s == "web"));
        assert_eq!(cfg.update.check, Some(ka_engine::config::UpdateCheck::Off));
    }

    /// `--ignore-rules` clears `[[rules]]` for the run (the rule-file
    /// conventions gate is set in main; hardstops are config-free).
    #[test]
    fn run_opts_ignore_rules_clears_rule_tables() {
        let mut cfg =
            Config::parse_layer("[[rules]]\ntool = \"bash\"\nverdict = \"ask\"\n", "t").unwrap();
        assert_eq!(cfg.rules.len(), 1);
        apply_run_opts(
            &mut cfg,
            &RunOpts {
                ignore_rules: true,
                ..RunOpts::default()
            },
        );
        assert!(cfg.rules.is_empty());
    }

    /// `ka config set` key table: typed values round-trip through the
    /// pure applier and the reader answers from the resolved config.
    #[test]
    fn config_set_get_roundtrip_on_the_pure_table() {
        let mut cfg = Config::default();
        apply_config_key(&mut cfg, "model", "anthropic/claude-sonnet-5@high").unwrap();
        apply_config_key(&mut cfg, "tui.bell", "false").unwrap();
        apply_config_key(&mut cfg, "guards.spend_usd", "2.5").unwrap();
        apply_config_key(&mut cfg, "features.disable", "agents, mcp:github").unwrap();
        apply_config_key(&mut cfg, "tools.mcp.discovery", "lazy").unwrap();
        apply_config_key(&mut cfg, "tui.mouse", "native").unwrap();
        assert_eq!(
            config_get(&cfg, "model").unwrap(),
            "anthropic/claude-sonnet-5@high"
        );
        assert_eq!(config_get(&cfg, "tui.bell").unwrap(), "false");
        assert_eq!(config_get(&cfg, "guards.spend_usd").unwrap(), "2.5");
        assert_eq!(
            config_get(&cfg, "features.disable").unwrap(),
            "agents,mcp:github"
        );
        assert_eq!(config_get(&cfg, "tools.mcp.discovery").unwrap(), "lazy");
        assert_eq!(config_get(&cfg, "tui.mouse").unwrap(), "native");
        assert!(apply_config_key(&mut cfg, "tui.bell", "maybe").is_err());
        assert!(apply_config_key(&mut cfg, "tui.mouse", "sideways").is_err());
        assert!(apply_config_key(&mut cfg, "[[rules]]", "x").is_err());
        assert!(config_get(&cfg, "nope.key").is_err());
    }

    /// `--image` builds attachment parts: magic-byte sniff, 5 MB cap.
    #[test]
    fn image_parts_sniff_and_cap() {
        let dir = std::env::temp_dir().join(format!("ka-cli-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a.png");
        let png_bytes: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        std::fs::write(&png, png_bytes).unwrap();
        let parts = image_parts(&[png]).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].media_type, "image/png");
        let txt = dir.join("b.txt");
        std::fs::write(&txt, b"not an image").unwrap();
        assert!(image_parts(&[txt]).is_err());
        let big = dir.join("c.png");
        std::fs::write(&big, vec![0u8; 6 * 1024 * 1024]).unwrap();
        assert!(image_parts(&[big]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod audit_fix_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn temp_overlay(contents: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "ka-audit-overlay-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn partial_overlay_file_keeps_embedded_fields() {
        // a user overlay naming only `context` must not brick the row:
        // no missing-`wire` startup error, and the embedded wire /
        // base_url / api_key_env / flags survive the merge.
        let path = temp_overlay("[dialects.\"anthropic/claude-sonnet-5\"]\ncontext = 123\n");
        let catalog = load_catalog(std::slice::from_ref(&path)).unwrap();
        std::fs::remove_file(&path).ok();
        let d = catalog.get("anthropic/claude-sonnet-5").unwrap();
        assert_eq!(d.context, 123);
        assert_eq!(d.wire, ka_dialect::Wire::AnthropicMessages);
        assert_eq!(d.base_url.as_deref(), Some("https://api.anthropic.com"));
        assert_eq!(d.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(d.flags.reasoning_field.as_deref(), Some("thinking"));
    }

    #[test]
    fn engine_death_mid_turn_fails() {
        // engine task gone: no final stop, no Idle → error, not exit 0
        assert!(exit_code_for(None, false, true).is_err());
        // clean idle shutdown without a TurnFinished (e.g. resumed
        // session already idle) is still success
        assert_eq!(exit_code_for(None, true, true).unwrap(), ExitCode::SUCCESS);
        // mapped stop reasons unchanged
        assert_eq!(
            exit_code_for(Some(Stop::Error), true, true).unwrap(),
            ExitCode::from(1)
        );
        assert_eq!(
            exit_code_for(Some(Stop::Aborted), true, true).unwrap(),
            ExitCode::from(2)
        );
        assert_eq!(
            exit_code_for(Some(Stop::Done), true, true).unwrap(),
            ExitCode::SUCCESS
        );
    }

    #[test]
    fn deny_choice_picks_last_option() {
        let ask = |opts: &[&str]| ka_protocol::AskQuestion {
            text: "proceed?".to_string(),
            options: opts.iter().map(|s| s.to_string()).collect(),
            detail: None,
        };
        assert_eq!(deny_choice(&[ask(&["allow once", "always", "deny"])]), 2);
        assert_eq!(deny_choice(&[ask(&["allow", "deny"])]), 1);
        // degenerate ask: fall back to the historical hardcoded 2
        assert_eq!(deny_choice(&[]), 2);
    }
}

#[cfg(test)]
mod config_surface_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// CFG-02/`ka config set`: the key table and `apply_config_key`
    /// cannot drift — every listed key applies (a value-type error is
    /// fine; the probe is deliberately not a valid number), and nothing
    /// outside the table is ever applicable.
    #[test]
    fn known_config_keys_cover_the_apply_table() {
        for key in KNOWN_CONFIG_KEYS {
            let mut cfg = Config::default();
            let err = apply_config_key(&mut cfg, key, "\u{0}probe");
            let unknown = err.as_ref().is_err_and(|e| e.contains("unknown key"));
            assert!(!unknown, "KNOWN_CONFIG_KEYS drifted: {key} is not applied");
        }
        let mut cfg = Config::default();
        assert!(
            apply_config_key(&mut cfg, "totally.bogus", "x")
                .is_err_and(|e| e.contains("unknown key"))
        );
    }

    /// CFG-02: `KA_<KEY>` env names map onto keys with `__` → dot.
    #[test]
    fn env_key_spelling_maps_double_underscore_to_dot() {
        assert_eq!("TUI__MOUSE".to_lowercase().replace("__", "."), "tui.mouse");
        assert!(known_config_key("tui.mouse"));
        // single underscores stay underscores: runtime vars like
        // KA_DEBUG_SETTLE land on unknown keys and are ignored
        assert!(!known_config_key("debug.settle"));
    }

    /// `--profile` names name a file: charset is enforced.
    #[test]
    fn profile_names_are_file_safe() {
        assert!(profile_layer_path("fast-models").is_ok());
        assert!(profile_layer_path("ci_2").is_ok());
        assert!(profile_layer_path("../evil").is_err());
        assert!(profile_layer_path("").is_err());
    }

    /// Batch 2 CLI surface parses.
    #[test]
    fn structural_cli_parses() {
        let cli = Cli::try_parse_from([
            "ka",
            "--profile",
            "fast",
            "--add-dir",
            "/tmp/extra",
            "run",
            "--rpc",
            "ignored",
        ])
        .unwrap();
        assert_eq!(cli.profile.as_deref(), Some("fast"));
        assert_eq!(cli.add_dirs.len(), 1);
        let Some(CliCommand::Run { rpc, .. }) = cli.command else {
            panic!("expected run");
        };
        assert!(rpc);

        let cli =
            Cli::try_parse_from(["ka", "sandbox", "--fs", "cargo", "build", "--release"]).unwrap();
        let Some(CliCommand::Sandbox { fs, command }) = cli.command else {
            panic!("expected sandbox");
        };
        assert!(fs);
        assert_eq!(command, vec!["cargo", "build", "--release"]);

        let cli =
            Cli::try_parse_from(["ka", "queue", "--session", "abc", "--message", "ping"]).unwrap();
        let Some(CliCommand::Queue {
            session, message, ..
        }) = cli.command
        else {
            panic!("expected queue");
        };
        assert_eq!(session, "abc");
        assert_eq!(message, "ping");
    }
}

/// GIT-06 · `ka init` foreign-rule import: cursor/copilot rule files
/// land as clearly marked sections in the generated AGENTS.md, the
/// originals untouched; `--no-import` skips the merge.
#[cfg(test)]
mod init_import_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ka-init-{}-{n}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn generated(path: &std::path::Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn cursorrules_land_as_a_marked_imported_section() {
        let root = temp_root("cursor");
        std::fs::write(
            root.join(".cursorrules"),
            "Always use spaces.\nNo emoji in code.\n",
        )
        .unwrap();
        let (target, imported) = init_agents_md(&root, true).unwrap();
        assert_eq!(imported, vec![root.join(".cursorrules")]);
        let md = generated(&target);
        assert!(md.contains("## Imported rules (cursor)"), "{md}");
        assert!(md.contains("Always use spaces."), "{md}");
        assert!(md.contains("<!-- from .cursorrules"), "{md}");
        // the starter body is still there, imports come after it
        assert!(md.find("# AGENTS.md").unwrap() < md.find("## Imported rules").unwrap());
        // the original is byte-identical
        assert_eq!(
            std::fs::read_to_string(root.join(".cursorrules")).unwrap(),
            "Always use spaces.\nNo emoji in code.\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cursor_rules_dir_and_copilot_get_their_own_sections() {
        let root = temp_root("families");
        std::fs::create_dir_all(root.join(".cursor/rules")).unwrap();
        std::fs::write(root.join(".cursor/rules/a.mdc"), "Rule A.\n").unwrap();
        std::fs::write(root.join(".cursor/rules/b.mdc"), "Rule B.\n").unwrap();
        std::fs::create_dir_all(root.join(".github")).unwrap();
        std::fs::write(
            root.join(".github/copilot-instructions.md"),
            "Keep PRs small.\n",
        )
        .unwrap();
        let (target, imported) = init_agents_md(&root, true).unwrap();
        let md = generated(&target);
        assert!(md.contains("## Imported rules (cursor)"), "{md}");
        assert!(md.contains("## Imported rules (copilot)"), "{md}");
        // .mdc files sorted by name, each origin-marked
        let a = md.find("<!-- from .cursor/rules/a.mdc").unwrap();
        let b = md.find("<!-- from .cursor/rules/b.mdc").unwrap();
        assert!(a < b, "mdc sources in sorted order");
        assert!(md.contains("Rule B."));
        assert!(md.contains("Keep PRs small."));
        assert_eq!(imported.len(), 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_import_flag_and_missing_files_mean_no_section() {
        let root = temp_root("noimport");
        std::fs::write(root.join(".cursorrules"), "Always use spaces.\n").unwrap();
        let (target, imported) = init_agents_md(&root, false).unwrap();
        assert!(imported.is_empty());
        assert!(!generated(&target).contains("## Imported rules"));

        let plain = temp_root("plain");
        let (target, imported) = init_agents_md(&plain, true).unwrap();
        assert!(imported.is_empty());
        assert!(!generated(&target).contains("## Imported rules"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&plain);
    }

    #[test]
    fn empty_foreign_files_produce_no_section() {
        let root = temp_root("empty");
        std::fs::write(root.join(".cursorrules"), "   \n").unwrap();
        let (target, imported) = init_agents_md(&root, true).unwrap();
        assert!(imported.is_empty());
        assert!(!generated(&target).contains("## Imported rules"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn existing_agents_md_still_refuses() {
        let root = temp_root("exists");
        std::fs::write(root.join("AGENTS.md"), "# mine\n").unwrap();
        let err = init_agents_md(&root, true).unwrap_err();
        assert!(err.contains("refusing to overwrite"), "{err}");
        assert_eq!(generated(&root.join("AGENTS.md")), "# mine\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn var_overrides_split_on_the_first_equals() {
        let parsed = parse_var_overrides(&[
            "k=v".to_string(),
            "eq=a=b".to_string(),
            "empty=".to_string(),
        ])
        .unwrap();
        assert_eq!(parsed[0], ("k".to_string(), "v".to_string()));
        assert_eq!(parsed[1], ("eq".to_string(), "a=b".to_string()));
        assert_eq!(parsed[2], ("empty".to_string(), String::new()));
        assert!(parse_var_overrides(&["nonsense".to_string()]).is_err());
        assert!(parse_var_overrides(&["=v".to_string()]).is_err());
    }
}
