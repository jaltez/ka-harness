# Changelog

## Unreleased — feature lots (2026-11-22)

### Batch 5 — customizable TUI
- Themes as data: `[tui] theme` + `/theme` picker (`.ka/themes/*.toml`, user dir; Nocturne fallback).
- Remappable keybindings: `[keybindings]` action-id → chord table (hard error on unknown action or doubled key); `/help` shows effective bindings.
- Segmented status line: `[tui] status_line = [...]` (model/mode/session/effort/context/cost/tokens/cache/toggles/update).
- Notification tuning: `[tui] notify_on = [...]`, `[tui] notify_unfocused` (focus reporting where the terminal supports it).
- Dynamic terminal title: `[tui] title` (default on).
- `/copy` block picker (copy or write to file) · `/search <text>` transcript search · `/changelog` · `/queue` follow-up queue view · `:emoji:` shortcodes on completion.

### Batch 4 — context surgery & automation
- `/diff` session cumulative diff (Myers, spill-backed) · `/compactfrom [N] [focus]` rewind+summary (also in the Rewind modal) · `ContextEdit` append-only message edits (protocol) · `/btw` side questions · `/goal` with budget and ◎ badge · `/loop [--every Ns]` with ↻ badge · `[context] prewalk` repo brief · `MemoryConsolidate` (MEMORY.md rewrite from inbox, undo-able).

### Batch 3 — sessions & context (cheap)
- Session pins (★) · branch summaries on rewind · `ka gc` · idle recaps · fuzzy session picker · `/shake [thinking|images]` · Ctrl+S draft stash · `/focus` · `/context` hints · `/autocompact N|off`.

### Batch 2 — structural CLI/config
- `--profile` · `--add-dir` · `ka queue` · `ka sandbox` runner · `ka import codex|gemini` (+ honest cursor guidance) · `ka import-session claude|codex` · `ka run --rpc` · `ka session rename|delete|archive|unarchive` · `KA_<KEY>` env layer (`__` = dot) · arrays merge across layers (specific-first).

### Batch 1 — CLI/config quick wins
- `--effort` · `--image` · `--offline` · `--system-prompt` · `--tools`/`--exclude-tools` · `--ignore-user-config` · `--ignore-rules` · `--ephemeral` · `ka run -o/--max-turns/--max-budget-usd/--max-cost/--warn-cost/--max-time` (exit 3 caps) · `ka config set/get/path` · `ka models <query>` · `ka export --jsonl` · `[context] prewalk` · `[prompt] identity_inline` · `[features] allow` · `[history]` · `[[model_pins]]` · `project_doc_fallback_filenames` + `AGENTS.override.md` · `#:schema` sidecar on save.

### Batch 0 — hygiene
- Overlay fix ([lsp]/[debug]) · strict `[tui] mouse`/`[tools.mcp] discovery` enums · doc↔code drift fixes · hermetic serve tests.

### Earlier batches (v0.7.2)
- See `research/ka-harness/` and the GitHub release notes.

### Batch 6 — interactive TUI
- `/vim` (INSERT/NORMAL/VISUAL sobre el draft, motions/counts/text objects mínimos) · `/screen` alt-screen fullscreen con dump limpio al scrollback · ghost text en el composer (slash/paths, Tab acepta) · `/recap` + recap automático al recuperar foco tras >5 min · `/plain` render para lectores de pantalla · paneles `/hooks` //`/permissions` //`/doctor` de solo lectura · pill de jobs/DAP en la status line · Agent Hub ligero en `/tasks` (coste por subagente cuando los Usage lo traen). `/tan` difiere (refactor del spawn de delegados).

### Batch 7 — cheap session tools
- `checkpoint` hand (git snapshot on demand, restorable) · `cron` hand (session-scoped scheduled prompts; off in safe-mode) · `monitor` hand (detached command with pollable output + kill) · `tool_search` hand (keyword search over MCP catalogs + hands) · `generate_image` hand (gated: provider + key required). Feature toggles: `cron`/`monitor`/`generate_image`. PowerShell tool deferred (Windows not a current target).

### Batch 9 — permissions
- Named permission profiles with one-level `extends`, activated by `[permissions] profile`; profile rules sort before `[[rules]]` · `ka rules check <command>` dry-runs your rules · `dontAsk` mode denies reviewable asks (hardstops still confirm) · `[permissions] prompt_policy = "deny"` for serve/CI · auto-review extended to write-tier asks (`auto_review_tier = "exec+write"`) · web-result prompt-injection screening (heuristic always, optional fast-role classifier) · `[shell]` env policy (all/core/none + keep/set).

### Batch 10 — extensibility
- Hook events: subagent_start/stop, pre/post model switch, post_compact, permission_request/denied · Hook handler types: HTTP endpoints (same JSON envelope) plus per-hook timeouts and async fire-and-forget · MCP per-server policy: enabled/disabled tool filters, approval modes (ask/auto/writes), per-tool timeouts and output caps · Enriched skills: `disable-model-invocation`, `paths` globs, `max_tokens` body cap, `!`cmd`` dynamic context · Enriched commands: `side` (side-question dispatch) and `shell` preamble · Output styles: `.ka/styles/*.md` + `/style` picker (session-scoped, built-in fallback).

### Batch 10b — MCP protocol & roles
- OAuth for HTTP MCP servers (dynamic client registration RFC 7591, CIMD, local callback, token cache with refresh in the data dir, `bearer_token_env` / `headers_helper` per server) · MCP elicitation routed to ka asks, sampling via the default model, `roots`, and server-initiated notifications applied live (tools/list_changed refreshes catalogs) · `[roles.<name>]` arbitrary roles addressable as `@name` · `[virtual_models.<name>]` round-robin/failover routers · `ka mcp-serve` read-only MCP stdio server (read/glob/grep/sessions_search/session_info; bash deliberately excluded).

### Batch 11 — models
- ka-passport (new opt-in crate + `passport` cargo feature): TokenSource trait per the ratified design stub, Claude Pro/Max OAuth (PKCE, loopback callback, keyring service `ka-passport`, proactive refresh) exposed as a claude-plan vendor (anthropic wire + oauth beta header) · Curated community providers: perplexity, nebius, sambanova, upstage · `[model] service_tier` passthrough on all three wires · Idle cache warming (`[context] cache_warming = "idle"`, ≥$0.05 turns) · `ka models --update` runtime catalog overlay from models.dev.

### Batch 12 — automation & git
- `ka action init [--force]` — vendored GitHub workflow (manual dispatch + `ka` label on PRs, SHA-pinned checkout, release install with sha256 verify, readonly run posting a PR comment; only `KA_MODEL`/`KA_*_API_KEY` secrets) · `ka schedule add|list|remove [--install]` — crontab lines with a `# ka-schedule` marker (prints by default; escaped for cron+shell) · `ka recipe <name> [--var K=V] [--dry-run]` — strict `.ka/recipes/*.toml` (vars + steps) run as sequential prompts in one session · `ka worktree add|list|remove [--delete-branch]` · `ka init [--no-import]` merges cursor/copilot rule files into the generated AGENTS.md as marked sections · `[git] commit_role` (role-generated commit subject from the staged diff + turn, silent fallback) and `[git] trailers` · `/pr <ref> [fix]` — gh-based PR review preset (positioned comments, strict findings; fix mode implements and pushes, never force/rebase) · `/git` staging/commit panel (porcelain list, per-file diff, stage/commit; push/force/rebase/hard-reset deliberately absent).

### Batch 9b — sandbox network & platforms
- `[sandbox] net_allow_domains` / `net_deny_domains` — loopback HTTP/CONNECT proxy injected into sandboxed bash children: domain filter (wildcards, deny-first, IP literals explicit), single pinned resolution per host, private/loopback targets refused unless allowlisted (anti-DNS-rebinding), unix-socket targets always refused · enforcement covers proxy-honoring clients; landlock confines raw TCP to the proxy port; proxy env rides the backend (`--setenv`/`--env`/trampoline) · missing full-net grants under restricted policies · macOS seatbelt backend (`sandbox-exec -p`, profile generated by a pure tested function, deny-first SBPL, enumerated `/dev` parity with landlock) in the detection chain · Windows: no backend, `mode "fs"` fails closed with a platform-named error.
