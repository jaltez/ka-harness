# Changelog

## 0.10.0 — 2026-11-24

### TUI
- Click a past user prompt (capture mouse mode) for a rewind popup: `⏎` runs the highlighted row · `r` rewind there · `e` rewind & edit · `f` fork from there (the same actions as the double-esc menu and `/tree`; `/focus` digest rows answer the same way).
- Composer popups (commands, `@` files) list up to 14 rows in a 96-column box instead of 7 in 64, and the mouse wheel moves the highlighted row; the ghost completes the highlighted command, since Enter runs it.
- `@` opens the file list the moment the sigil is typed and re-ranks it on every character with a fuzzy search over the whole path (directories included): `src/ma` finds `src/main.rs`, `tuisnap` finds `src/tui_snapshots.rs`.
- In capture mouse mode the composer hints are buttons for what they name (send, clear, history, commands); on the status line the mode chip opens the mode picker and the model label the model picker.
- While a turn runs, the todo list is pinned above the composer beside the streaming text; the strip button still opens the full list.
- Skills carry their origin root (`.claude`, `.agents`) and the model picker aligns its columns and grows for long ids.
- A frame harness renders 215 TUI scenes at three terminal sizes on every test run.

### Fixed
- Traps: an unknown or partial slash command no longer reaches the model (Enter runs the highlighted one); Escape clears a draft; a modified key never acts as its bare letter inside a modal (Ctrl+R no longer rewinds from the rewind menu, Ctrl+A no longer stages everything in `/git`); a click outside a panel keeps a typed api key, settings edit or save path; Enter on a filter that matches nothing no longer starts a new session.
- Keys: reverse search takes typed characters; Ctrl+C backs out of a modal first, and a second press during a turn quits; history keeps the half-typed draft and holds prompts only; sending returns the view to the tail; the mouse toggle left Ctrl+M (Enter on legacy terminals) for Alt+M; `--ephemeral` no longer prints a resume hint.
- Sizing: modals are sized against the area they are drawn in (rows no longer wrap two columns wide at 80 columns); every list keeps its selection on screen; doc pagers (changelog, hooks, permissions, doctor, debug, task detail) open at the top, page to the end, scroll with the line keys, and keep their markers inside the box.
- The permission ask shows the changed lines of the diff (never the file headers) and all of its options at 80x24.
- The status line drops whole segments instead of cutting the cost or the mode chip.
- Plain mode (the screen-reader view) clears its pane for a modal, wraps its rows, and says a verdict once.
- Slash commands echo as one dim line instead of a full prompt band.
- The 10 s background-task poll printed `▸ background tasks` and its rows into the transcript on every tick (twice at startup on a clean session).
- Transcript click and hover mapping with several prompts: the `/focus` row mapping was applied with `/focus` off, so tool cards in earlier turns answered the wrong click.
- `/update` in-session resume re-exec failed with ENOENT after the binary swap.

### Protocol
- `Event::Inventory.skills` carries `SkillEntry { name, source }` instead of bare names on the NDJSON/RPC stream. Deserialization accepts both shapes; a host that parsed `skills` as strings reads `.name` now.

## 0.8.0 — 2026-11-23

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
- `/vim` (INSERT/NORMAL/VISUAL over the draft, minimal motions/counts/text objects) · `/screen` alt-screen fullscreen with a clean dump to the scrollback on exit · ghost text in the composer (slash/paths, Tab accepts) · `/recap` + automatic recap when focus returns after >5 min · `/plain` screen-reader render · read-only `/hooks` //`/permissions` //`/doctor` panels · jobs/DAP pill in the status line · light Agent Hub in `/tasks` (per-subagent cost when Usage carries it). `/tan` deferred (delegate spawn refactor).

### Batch 7 — cheap session tools
- `checkpoint` hand (git snapshot on demand, restorable) · `cron` hand (session-scoped scheduled prompts; off in safe-mode) · `monitor` hand (detached command with pollable output + kill) · `tool_search` hand (keyword search over MCP catalogs + hands) · `generate_image` hand (gated: provider + key required). Feature toggles: `cron`/`monitor`/`generate_image`. PowerShell tool deferred (Windows not a current target).

### Batch 8 — rich readers & search
- Rich `read` backends: PDF (text extraction) and archive members (ZIP/JAR/APK/WHL + tar/tar.gz, CRC-verified, clear errors on encrypted/corrupt) via `archive_entry`; >32 KiB spills · `[[search]]` keyless duckduckgo and searxng backends with per-site scoping and first-success-wins fallback · TUI: multiple staged images become positional `[Image #N]` chips, `spill://` pointers register in `/spills`, ghost text for `/goal` and `/loop`. DOCX/PPTX/XLSX/EPUB/notebooks/SQLite documented out of scope v1; `code mode` deferred to its own spike cycle (footprint decision pending).

### Batch 9 — permissions
- Named permission profiles with one-level `extends`, activated by `[permissions] profile`; profile rules sort before `[[rules]]` · `ka rules check <command>` dry-runs your rules · `dontAsk` mode denies reviewable asks (hardstops still confirm) · `[permissions] prompt_policy = "deny"` for serve/CI · auto-review extended to write-tier asks (`auto_review_tier = "exec+write"`) · web-result prompt-injection screening (heuristic always, optional fast-role classifier) · `[shell]` env policy (all/core/none + keep/set).

### Batch 10 — extensibility
- Hook events: subagent_start/stop, pre/post model switch, post_compact, permission_request/denied · Hook handler types: HTTP endpoints (same JSON envelope) plus per-hook timeouts and async fire-and-forget · MCP per-server policy: enabled/disabled tool filters, approval modes (ask/auto/writes), per-tool timeouts and output caps · Enriched skills: `disable-model-invocation`, `paths` globs, `max_tokens` body cap, `!`cmd`` dynamic context · Enriched commands: `side` (side-question dispatch) and `shell` preamble · Output styles: `.ka/styles/*.md` + `/style` picker (session-scoped, built-in fallback).

### Batch 10b — MCP protocol & roles
- OAuth for HTTP MCP servers (dynamic client registration RFC 7591, CIMD, local callback, token cache with refresh in the data dir, `bearer_token_env` / `headers_helper` per server) · MCP elicitation routed to ka asks, sampling via the default model, `roots`, and server-initiated notifications applied live (tools/list_changed refreshes catalogs) · `[roles.<name>]` arbitrary roles addressable as `@name` · `[virtual_models.<name>]` round-robin/failover routers · `ka mcp-serve` read-only MCP stdio server (read/glob/grep/sessions_search/session_info; bash deliberately excluded).

### Batch 11 — models
- ka-passport (new opt-in crate + `passport` cargo feature): TokenSource trait per the ratified design stub, Claude Pro/Max OAuth (PKCE, loopback callback, keyring service `ka-passport`, proactive refresh) exposed as a claude-plan vendor (anthropic wire + oauth beta header) · Curated community providers: perplexity, nebius, sambanova, upstage · `[model] service_tier` passthrough on all three wires · Idle cache warming (`[context] cache_warming = "idle"`, ≥$0.05 turns) · `ka models --update` runtime catalog overlay from models.dev.

## 0.9.0 — 2026-11-23

### Batch 13 — deferred items (part 1)
- **Code mode** (`code` cargo feature + runtime toggle `tool:code`) — the model writes a JavaScript script that orchestrates ka tools and only the final result returns to the model: `ka.tools.call(name, argsJson)` routes every call through the full clearance gate (asks resolve to a denied-with-instruction error — pre-approve via rules/`[permissions] allow` and re-run), `ka.models.classify(text, labels)` rides the fast role; fresh QuickJS runtime per invocation (rquickjs 0.14, chosen over Boa by measured musl footprint: ~1.2 MB net vs a 9.5 MB binary alone), `[code] timeout_ms` (10 s) + `memory_mb` (16) limits, interrupt handler + tokio timeout belt, no fs/net/stdio bindings (fail-closed); default binary unchanged (7.25 MB, feature off).
- **`/tan <question>`** — tangential background question on a read-only child session: the answer arrives as a `tan · …` note while the conversation continues (never a turn); `/tan` lists live jobs, `/tan cancel [id]` cancels; `tan:N` status pill; delegate's background path refactored into a shared `arm_background` spawn (delegate semantics unchanged); off in safe-mode.
- **Agent teams (EXT-12)** — `team` hand over persistent named member sessions (star topology, parent routes): `team_spawn`/`team_send`/`team_status`/`team_read`/`team_dismiss`; members are full engines with their own strands (`<parent>/team/<name>` in `ka sessions`), multi-turn context accumulates per member, every turn lands as a `team · <name>` note, rows in `/tasks`; leaf posture (no delegate/mcp/hooks in members, asks auto-deny), `[delegate] team_max` (default 8, 0 disables), off in safe-mode.

### Batch 12 — automation & git
- `ka action init [--force]` — vendored GitHub workflow (manual dispatch + `ka` label on PRs, SHA-pinned checkout, release install with sha256 verify, readonly run posting a PR comment; only `KA_MODEL`/`KA_*_API_KEY` secrets) · `ka schedule add|list|remove [--install]` — crontab lines with a `# ka-schedule` marker (prints by default; escaped for cron+shell) · `ka recipe <name> [--var K=V] [--dry-run]` — strict `.ka/recipes/*.toml` (vars + steps) run as sequential prompts in one session · `ka worktree add|list|remove [--delete-branch]` · `ka init [--no-import]` merges cursor/copilot rule files into the generated AGENTS.md as marked sections · `[git] commit_role` (role-generated commit subject from the staged diff + turn, silent fallback) and `[git] trailers` · `/pr <ref> [fix]` — gh-based PR review preset (positioned comments, strict findings; fix mode implements and pushes, never force/rebase) · `/git` staging/commit panel (porcelain list, per-file diff, stage/commit; push/force/rebase/hard-reset deliberately absent).

### Batch 9b — sandbox network & platforms
- `[sandbox] net_allow_domains` / `net_deny_domains` — loopback HTTP/CONNECT proxy injected into sandboxed bash children: domain filter (wildcards, deny-first, IP literals explicit), single pinned resolution per host, private/loopback targets refused unless allowlisted (anti-DNS-rebinding), unix-socket targets always refused · enforcement covers proxy-honoring clients; landlock confines raw TCP to the proxy port; proxy env rides the backend (`--setenv`/`--env`/trampoline) · missing full-net grants under restricted policies · macOS seatbelt backend (`sandbox-exec -p`, profile generated by a pure tested function, deny-first SBPL, enumerated `/dev` parity with landlock) in the detection chain · Windows: no backend, `mode "fs"` fails closed with a platform-named error.
