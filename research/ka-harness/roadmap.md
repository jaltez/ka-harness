# Ka — Phased Roadmap (FINAL, post-grilling)

Locked decisions: **tokio · ratatui+crossterm · 2 wires (anthropic-messages, openai-chat) · exact-match edits + read-tracking · safety in Phase 2 · local models mandatory Phase 1 · strict TOML · Rust-regex-only search · pathfinder subagent mandatory Phase 6 · pure JSONL core with optional index crate later · keys-only core auth (subscription OAuth = future `ka-passport` crate) · read-only git awareness · SSE fixtures + optional live smoke · char-ratio token estimate with usage true-up · `ka update` from signed GitHub releases.**

Legend: **[M]** mandatory · **[O]** optional/deferrable. Names refer to the architecture doc (`architecture.md`).

## Phase 0 — Skeleton & Contracts
- **[M]** Workspace crates: `ka-agent` (engine), `ka-protocol` (Command/Event wire types), `ka-dialect` (providers), `ka-strand` (sessions), `ka-term` (TUI), `ka-cli` (bin)
- **[M]** Engine↔surface protocol: two mpsc queues; serde enums; NDJSON-serializable (headless reuses it verbatim)
- **[M]** Strict TOML config chain: defaults < `~/.config/ka/ka.toml` < `.ka/ka.toml` < env < flags; unknown keys = hard error with line numbers; generated JSON schema
- **[M]** `dialects.toml` model catalog as data: context window, max output, effort levels, pricing, modalities, dialect flags
- **[M]** CI: clippy `-D warnings`, forbid unwrap/expect in engine crates, release profile (thin-LTO, strip, panic=abort), cross-build musl
- **[O]** Release signing setup (ed25519 minisign-style keypair + CI artifact signing) — prerequisite for `ka update` in Phase 3
- **[O]** Perf baselines in CI (cold-start ms, idle RSS vs committed `baselines.json`) — gemini-cli practice, adopt early
- **[O]** `ka doctor`

**Exit:** `ka run "hi"` streams a canned event sequence through the real protocol.

## Phase 1 — Providers, Selection, Local Models
- **[M]** Wire adapters: `anthropic-messages`, `openai-chat` (SSE; reqwest+rustls, no provider SDKs)
- **[M]** Unified stream events: text/thinking/toolcall deltas, partial-JSON arg accumulator with repairing parser, stop-reason normalization
- **[M]** Dialect flag profile (~12 flags): message shaping, reasoning field, max-tokens field, sampling support, tool-choice downgrades; catalog defaults deep-merged with user overrides
- **[M]** Selectors `vendor/model:effort`; roles `default`/`fast`; persisted model-change records
- **[M]** Auth ladder: env > `.env` chain > keyring (optional dep); `!cmd` secret indirection; no secrets in config; **keys-only by design — subscription OAuth never links into core** (future `ka-passport` crate plugs a TokenSource)
- **[M]** Retry engine: classified errors, backoff+jitter, retry-after, overflow marked (never blind-retried)
- **[M]** Prompt-cache plumbing per wire + cache-hit accounting
- **[M]** **Local models:** discovery probes (Ollama/LM Studio/vLLM `/v1/models`), capability sniff (context, tools, vision), unbounded first-byte timeout, reasoning-replay dialect flag
- **[M]** Token accounting: char-ratio estimate (per-dialect `ratio`, default 4) for meters/digest triggers; every response's usage fields overwrite the estimate (true-up)
- **[M]** Wire test suites: recorded SSE fixtures per wire (incl. malformed-chunk/repair cases) replayed in CI via local socket + wiremock for headers/retries — no keys in CI
- **[O]** Usage/cost footer data (pricing already in catalog)
- **[O]** `cargo xtask live-smoke` — 5-minute real-key checks (anthropic + openai-compatible + ollama) before releases, never in CI
- **[O]** Fallback chains / credential rotation

**Exit:** streaming completion + tool-call round-trip against Anthropic, an OpenAI-compatible cloud provider, and a local Ollama model, all through one interface.

## Phase 2 — Loop, Core Tools, Hard Safety
- **[M]** Turn loop: prompt → stream → tool calls → parallel execution (ordered results) → feed back; stop/length/tools; max-steps cap; abort keeps partial work
- **[M]** Steer/queue input (mid-turn steering; wire tags stay `interject`/`defer`)
- **[M]** Tools: `read` (line/byte selectors, caps), `edit` (exact-match + read-before-edit + changed-since-read via ledger), `write`, `bash` (timeout, output caps + `spill://` files, process-tree kill, auto-background), `glob`, `grep` (Rust regex only, instructive error on unsupported constructs)
- **[M]** Git read-only awareness: engine snapshots repo state (branch, dirty file list) into strand context at turn start; `glob`/`grep` respect gitignore; **no commits, no undo — VCS stays the user's**
- **[M]** Clearance annotations on every tool (read/write/exec + read-only/idempotent)
- **[M]** Tool-result hygiene: truncation, spill pointers, empty-result elision marker
- **[M]** **Safety floor:** `guarded`/`free` modes; session "always allow"; bash compound-segment splitting + wrapper stripping + redirection-as-write; **hardstops** (unbypassable catastrophic list) prompting even in `free`
- **[O]** Loop detection (interaction-signature hash)
- **[O]** `ask` tool (lands with TUI in Phase 3)

**Exit:** dogfood on a real repo in `guarded` mode without hand-editing files.

## Phase 3 — Strands & Initial TUI
- **[M]** Strand store: append-only JSONL, header record, record kinds (message, model/effort change, digest, boundary, custom namespaced), id/parent tree + leaf pointer, split-to-new-file
- **[M]** Resume + interrupted-turn synthesis (dangling turns marked aborted)
- **[M]** ratatui TUI: streaming transcript, input editor + history, abort key, footer (tokens, cost, cache-hit %, context %), `ask` dialogs
- **[M]** Session picker (`ka`, `ka -c`)
- **[M]** `ka update`: fetch from GitHub releases, ed25519 signature verification before swap, opt-in channel tag (stable/edge); no auto-update. Startup availability check ratified 2026-10: one rate-limited (24 h) check when the TUI opens, `⤴` badge + transcript line, `[update] check = "off"` disables; never installs
- **[O]** Waypoints (per-terminal continue tokens)
- **[O]** Titles via `fast` role
- **[O]** Markdown export

**Exit:** daily driver for small tasks; resume after crash is clean.

## Phase 4 — Context Survival
- **[M]** Tool-output pruning (protected recent window, minimum-savings threshold)
- **[M]** Digest compaction: kept-tail budget, never cut inside a tool pair, split-turn handling, same-model option, `/compact [focus]`
- **[M]** Reserve-based trigger (max(16k, 15% window)) + overflow → digest-and-retry
- **[O]** Zero-LLM elision pass (old outputs → `spill://` pointers)
- **[O]** Context promotion (larger-window sibling switch)
- **[O]** Speculative digest (pre-threshold arm)
- **[O]** Full display history with digest dividers

**Exit:** multi-hour session with no manual context surgery.

## Phase 5 — Permissions as Data & Trust
- **[M]** Per-tool allow/ask/deny rules in config (compound bash patterns)
- **[M]** Project trust gate for `.ka/` local config/skills/hooks
- **[O]** One-way secret redaction (env scan + regex set) in tool results
- **[O]** `ka-sandbox` feature crate: landlock+seccomp (Linux) / seatbelt strings (macOS), fail-closed
- **[O]** Rule import from deny-list templates

**Exit:** `free` mode usable on trusted repos with a defensible floor.

## Phase 6 — Conventions, Extension, Pathfinder
- **[M]** AGENTS.md hierarchy (root→cwd, lazy per-directory) + `ka init`
- **[M]** Skills: SKILL.md standard, progressive disclosure
- **[M]** Hooks: shell commands, JSON envelope, exit-2 block (ecosystem-compatible contract), PreToolUse/PostToolUse first
- **[M]** **Pathfinder:** read-only subagent (child session, reduced toolset, summary-only return); spawn seam = the same engine, isolated strand
- **[O]** MCP client behind cargo feature (startup-gated, deferred tool discovery)
- **[O]** Custom slash commands (markdown, `$ARGUMENTS`)
- **[O]** Markdown-defined agents (model/tools frontmatter)
- **[O]** Claude-Code-compatible `--print` stream-json

**Exit:** ecosystem interop (reads others' skills/rules) + context protection via delegation.

## Phase 7 — Power Features (optional, ordered by pull)
1. Plan mode (read-only toolset, plan file, approval handoff, `plan` role)
2. Strand tree navigation + offshoot summaries
3. Snapshots/rewind (write-tree objects + patch records)
4. Background jobs (detached bash + job tools)
5. Worktree isolation for subagents
6. `ka-index` crate (SQLite/FTS cross-session search)
7. openai-responses third wire; provider-native digest hooks
8. ACP server (stdio) for editor embedding
9. HTTP/SSE server mode
10. Memory tiers (MEMORY.md)
11. Web search + URL reader tools
12. Structured-output mode (schema-constrained replies)

## Phase 8 — Daily-Driver Gap Closing (ratified 2026-09-14)

Grounded in `daily-driver-study.md` (fact-checked vs omp/pi/Claude Code/OpenCode/Codex/Gemini/Crush/goose/aider). Locked decisions: **DAP client in ka-engine as the third Content-Length JSON-RPC instance (after lsp.rs, mcp.rs) · adapter catalog is data (embedded TOML + `[debug.adapters]` overlay, mirroring `[lsp.commands]`) · runtime gates `[debug] enable` / `[lsp] write_through`, inert in `--safe-mode` · debug adapters join the allowed-children list · every addition re-measured via `xtask size`; anything >100 KB becomes a cargo feature · WorkspaceEdits apply only through the existing exact-match/ledger write path · keys-only auth reaffirmed (`ka-passport` stays future, Copilot device-flow only).**

Order: 8.1 → 8.2 → 8.3 → 8.4. 8.4 items are filler and never block.

### 8.1 LSP write-through — act through the server (shipped 2026-09)
- **[M]** Handle `workspace/applyEdit` reverse request in `lsp.rs` (preview + apply through the write path)
- **[M]** Write-tier hand `lsp_rename`: `textDocument/rename` → `WorkspaceEdit` → ledger-stamped apply with unified_diff preview + size caps
- **[M]** Write-tier hand `lsp_actions`: `textDocument/codeAction` (+`codeAction/resolve`) → `applyEdit` / `workspace/executeCommand`
- **[M]** File moves consult `workspace/willRenameFiles` (ripple edits applied first) + `didRenameFiles`; moved open files `didClose`d
- **[M]** Config `[lsp] write_through = true` (strict-TOML schema test); refuse edits on drifted open files
- **[O]** `textDocument/formatting` hand; `prepareRename` validity precheck
- Tests: fixture LSP server (rename/action/willRename/applyEdit paths) + feature contracts + README together

### 8.2 DAP — the probe (shipped 2026-09)
- **[M]** DAP client + debug hand + adapter catalog + `[debug]` gate — **shipped** (16 actions; embedded catalog + `[debug.adapters]` overlay; shared Content-Length codec extracted to `wire.rs`, consumed by `lsp.rs` + `dap.rs`; real-adapter dogfood vs debugpy 1.8.22 passes: launch → entry stop → breakpoints → stack/vars/eval → exit)
- **[M]** Clearance: control-flow actions (launch/attach/step/continue/terminate) = Exec; inspection (stack/scopes/variables/evaluate/threads) = Read
- **[M]** TUI roster (tasks-style session list) — **shipped 2026-09**: `/debug` overlay (`Command::DebugRoster` → pager modal: session headers, per-file breakpoint lines, console tail), plus async breakpoint-stop Notes — one `debug d1 stopped at …` note per stop while no hand wait holds the `hand_waiting` latch (200-char cap; best-effort: a stop racing a wait's exit or landing during `start`'s handshake can still echo in the tool result)
- **[O]** cargo feature `dap` if `xtask size` says >100 KB — moot at 6.42 MB total
- Tests: fake DAP adapter fixture (handshake, breakpoint, stop-read loop) + **real debugpy dogfood** (launch → entry stop → bp → stack/vars/eval → exit) + **real lldb-dap dogfood** (Ubuntu's `lldb-vscode-14`, the pre-rename binary: entry stop → verified line-2 bp → exit 0); catalog schema contract
- Notes: launch is fire-and-forget (debugpy defers its response to `configurationDone`); the client never echoes the adapter's `initialized` event (debugpy starts the debuggee on it, which would race breakpoint setup)

### 8.3 Delegation 2.0 — contracts, steering, merge-back (shipped 2026-09)
- **[M]** Agent frontmatter `output:` (JSON schema); child final message validated through the existing structured-output path; one instructive retry
- **[M]** `tasks send <id> <text>` — steer running background agents via their steer queues
- **[M]** Sibling messaging: engine-mediated roster injected into spawned context; `tasks inbox`; messages to finished agents surface as notes (no revival in v1)
- **[M]** `tasks merge <id>` — clean-only patch apply from the surviving `ka-<name>-<uuid>` worktree branch; on conflict surface the `.patch` path and stop; parent-mode gated (accept-edits/free, else Ask)
- **[M]** TUI `/tasks` roster w/ status/cost + transcript pager — **shipped 2026-09**: `/tasks` opens a picker modal (Enter sends `Command::TaskDetail`, the full uncapped result pages in a scrollable modal; job/dap rows listed, non-pickable)
- Declined: CoW isolation backends (fuse/overlayfs daemons violate one-process children-only; git worktrees are the Claude Code-shipped subset)

### 8.4 Convenience set (filler)
- **[M]** `ka skill install|list|remove` — git URL/path → user skills dir, trust-gated, no npm registry; agentskills.io spec-field alignment (`license`, `compatibility`, `metadata`, `allowed-tools`) — **shipped 2026-09** (install/list/remove + tolerant frontmatter)
- **[M]** HTML export: `/export --html` + `ka export --html` — self-contained `include_str!` template, zero deps, tool-call cards + agent sections — **shipped 2026-09** (`ka export --html`; TUI flag pending)
- **[M]** Compaction ladder formalized: prune (spill) → shake (deterministic; now also truncates stale tool args) → digest — **shake shipped 2026-09**; post-digest re-read of ≤5 ledger-hot files **still open** (needs a record_ids/persistence invariant pass before it can land safely)
- **[O]** SDK-story doc: crates.io workspace, `--print stream-json`, `ka serve` SSE, ACP — one page tying the surfaces together
- Declined: snapcompact (study §4.7), live collab/share relay, npm tarball installs

**Exit:** heavy interactive coding on ka — refactor-through-server, attach-a-debugger, fan-out with contracts and merge-back — without reaching for another harness, at ≤10 MB musl.

## Phase 9 — Approval & Extensibility (ratified 2026-09-29, shipped 2026-09-29)

Grounded in `feature-research-2026-09.md` (fresh 10-harness sweep + interview; the 2026-08-23 corpus and `daily-driver-study.md` remain the per-tool references). Locked decisions: **sandbox denials return structured missing-grant data and convert to one-shot asks — fail-closed preserved, no model in the safety path · the hook protocol is extended, not replaced (same JSON envelope, same exit codes) · rule import is one-shot and explicit, no live-read compat surface · `auto_review` is opt-in, auto-allow-only, never auto-denies, always transcript-logged · install generalizes with no registry and no manifests · multi-client is observe-only v1 — single writer, server-enforced · ka-passport remains a documented stub (trait + data shapes, no crate, no providers).**

Order: 9.1 → 9.6 in sequence; 9.7 is filler and never blocks.

### 9.1 Sandbox-expansion modal (flagship) — shipped 2026-09
- **[M]** `ka-sandbox` denials carry structured missing-grant records (write paths, network hosts, env vars) instead of a flat refusal — grant data, not prose — **shipped 2026-09** as *pre-flight* grant computation (deterministic, from command analysis — redirect targets, network-tool table, env assignments — never from failure output; a post-mortem channel would have been heuristics)
- **[M]** Engine converts a grant-bearing denial into a one-shot ask with the existing ask-modal verbs: this run / always (session → project layer) / deny — **shipped**: grants ride exactly the approved call (`sandbox_pending`); "always" persists write paths to `[sandbox] allow_write` (project layer) + live policy + session memory; network/env grants are session/run-scoped by design; one ask per command signature; guarded mode folds the grant summary into the single permission ask. Note: writes beneath broad dirs (`/`, `$HOME`, `/etc`…) are never offered — fail-closed stays
- **[M]** Fail-closed preserved: hardstops, protected paths, and non-grant-computable denials never expand — the ask is always shown, nothing pre-granted
- Tests: sandbox denial fixture → exact grant diff; "always" persists the expected rule; README + feature contract together — **shipped** (`sandbox_expansion_grant_ask_allow_deny_always`; landlock's execution proof rides the sandbox crate's argv contracts because the trampoline points at `current_exe`, i.e. the test binary)
- Declined: LLM in this loop (see 9.4 — the reviewer trails, never leads)

### 9.2 Hook powers — shipped 2026-09
- **[M]** Lifecycle events: `session_start`, `session_end`, `turn_end`, `user_prompt_submit`, `pre_compact` — same stdin JSON envelope, same exit-code contract (exit 2 = block where blockable)
- **[M]** `pre_tool_use` stdout steering extended with `updated_input`: shallow-merge patch of tool arguments (crush/claude shape)
- **[M]** Hook `allow` verdict pre-approves and skips the permission prompt; every pre-approve lands as a visible transcript record
- **[M]** Unchanged rails: inert in `--safe-mode`, hooks never bypass hardstops
- Tests: hook fixture matrix — block / patch / pre-approve / no-verdict × each new event + safe-mode inertness contract — **shipped** (`hook_updated_input_patches_arguments`, `hook_pre_approve_skips_the_ask_but_not_hardstops`, `lifecycle_hooks_fire_across_the_engine_loop`; the tool-hook envelope gained `cwd`, additive)
- Declined: file-watch hooks (overlaps the watch-mode non-goal)

### 9.3 `ka import claude` — shipped 2026-09
- **[M]** One-shot parse of Claude Code `settings.json` permissions (allow/ask/deny arrays; `Bash(...)`/`Edit(...)`/`Read(...)` pattern forms) → ka `[[rules]]`
- **[M]** Printed diff before writing; target layer choice (project `.ka/ka.toml` vs user config); nothing written unconfirmed
- **[O]** codex `config.toml` and gemini `settings.json` formats later
- Tests: fixture `settings.json` → expected rules TOML; untranslatable patterns reported, never dropped silently — **shipped** (claude `prefix:*` → ka `prefix*`, `WebFetch(domain:x)` → web_fetch domain pattern, `mcp__s__t` → `s.t`; deny→ask→allow emission order preserves claude's deny-wins semantics under ka's first-match)

### 9.4 Safety trail — shipped 2026-09
- **[M]** `[guards] auto_review` opt-in: the `fast`-role model reviews exec-tier asks; high-confidence obvious ones are auto-allowed, everything else falls through to the human; never auto-denies; every auto-allow is a transcript audit record; inert in `--safe-mode`
- **[M]** `doom_loop` rule domain: the existing loop-signature guard surfaced as configurable ask/deny over repeated identical calls
- Tests: reviewer fixture (confident → allow + audit record, non-confident → human ask); precedence vs hardstops; loop (doom_loop) ask on repeated signature

### 9.5 `ka install` — shipped 2026-09
- **[M]** `skill install` generalized: skills, agents, commands, rules from git URL or path; same trust gate; no registry, no manifests, no version machinery
- **[M]** `ka skill install` stays as alias; `ka install list|remove` manage all kinds
- Tests: install matrix per resource kind + trust-gate contract (untrusted project dir refuses)

### 9.6 Observe-only multi-client v1 — shipped 2026-09
- **[M]** `ka attach <id-prefix>`: read-only SSE client of a live `ka serve` session (reuses `GET /sessions/{id}/events` + `Last-Event-ID` reconnect); renders the live transcript with normal TUI chrome
- **[M]** Presence: serve tracks per-session SSE subscribers; attach footer shows `busy · N attached` — **shipped** via a per-session ring buffer (512 events, so late subscribers get full history incl. the startup Replay) + `tokio::sync::broadcast` fan-out + a synthetic `Event::Presence` (additive on the wire); the writer's-TUI chip is deferred to shared-write work — an in-process writer TUI never sees serve-side presence by construction. Single-writer is server-enforced trivially: observers have no write route (prompting stays `POST /sessions/{id}/prompt`); disconnect detection watches the socket's read half so presence drops immediately, not at the next keepalive
- **[M]** Single writer, server-enforced: attach has no input path to the turn machine; prompt attempts are refused
- Tests: two-client SSE fixture (identical replay), presence counter, attach write-attempt rejection — **shipped** (`concurrent_observers_presence_and_listings`; `GET /sessions` + `GET /sessions/{id}` carry presence/title; attach resolves id-prefix → title-substring → newest)
- Declined for now: shared write access, permission-queue fan-out, engine relocation into serve (crush-parity workspace)
- Re-ratification: the 8.4 decline of "live collab/share relay" covered write-sharing; observe-only attach is read-side only and in scope (same pattern as the 2026-09-13 git-mutation re-ratification)

### 9.7 Convenience set (filler) — shipped 2026-09
- **[M]** Command frontmatter: `argument-hint`, `allowed-tools`, `model` on `.ka/commands/*.md` (claude parity), surfaced in the `/` popup
- **[M]** `ka -i "<prompt>"`: one headless prompt, then the TUI opens resumed on that strand (gemini parity)
- **[O]** `ka completions <shell>` (bash/zsh/fish) — size-checked via `xtask size`; hand-rolled tables preferred over clap_complete if the dep breaks the budget
- **[O]** ka-passport design stub: `TokenSource` trait + auth-ladder plug point + data shapes documented in `sdk.md`/architecture — no crate, no providers — **shipped** (`sdk.md` §6: trait sketch, `auth = "passport"` catalog key, keyring service, one-provider-first order). `-i` parses onto the global CLI and sends its Prompt before the TUI opens (the engine's event channel buffers the turn); completions are generated from the live clap parser at runtime — subcommands and flags can never drift (data over code, no clap_complete dep)

**Exit:** approval fatigue measurably reduced on trusted repos — sandbox denials become one-shot grant asks, exec asks are pre-reviewed when opted in, hooks lint/patch/pre-approve, Claude Code switchers import in one command — and a second terminal can watch a session live, at ≤ 10 MB musl with zero new dependencies. **Met 2026-09-29: zero new workspace dependencies; `cargo xtask ci` green (fmt, clippy -D warnings incl. unwrap/expect denies, all contract suites).**

## Phase 10 — Runtime Feature Toggles (ratified 2026-09-30)

One spec grammar — `agents | skills | mcp | hooks | web | lsp | debug`, `mcp:<server>`, `skill:<name>`, `tool:<hand>`, `agent:<name>` — shared by four surfaces: `--disable`/`--enable` CLI flags, the strict `[features] disable` config table, the TUI's `/features` command + panel, and the strand's `Change` snapshot (restored on resume, like model/mode). Sandbox mode is a value knob (`/sandbox off|fs`, `--sandbox`, existing `[sandbox] mode`), not on/off grammar.

Design invariants:
- **Fail-closed disable**: hidden from the model-facing specs AND rejected by name at admit time (a hallucinated call cannot reach it); per-item forms hold inside the dynamic hands (delegate roster, lazy `mcp_call` server validation) so their def()/execute() stay truthful.
- **Disable = hide, not kill**: processes (MCP servers, LSP, debug) stay up until session end — killing MCP children would fight the reconnect watchdog; enabling never-started servers reuses the extracted bootstrap spawns (`ensure_mcp_server`, LSP/debug starts) and runs them live.
- **Mid-turn semantics**: the voice's command select applies the cheap hide/deny layer immediately (next model round-trip) and buffers the command; the engine replays spawn/strand/inventory side effects when the turn settles — "next prompts can't use them", never a mid-turn policy race.
- **Bare mode outranks everything**: `--safe-mode` refuses every enable and every sandbox widen; no toggle can re-widen a safe-mode session.
- **Local-trust commands**: `SetFeature`/`SetSandbox` ride the `Shell` trust invariant — serve/ACP never forward them (serve constructs a fixed `Command::Prompt` only, so the boundary is structural).
- **Snapshot semantics on the strand**: `Record::Change` carries the full disabled set + sandbox mode (presence replaces); specs that no longer parse in old strands skip with a note.

Shipped with:
- **[M]** Protocol: `FeatureSpec` (serde as the plain string), `Command::SetFeature`/`SetSandbox`, `Event::FeaturesChanged`, `Event::Inventory.disabled` (additive; the inventory re-emits after every change), `Command::SaveSettings.features` (the panel's `s` saves the default).
- **[M]** Config: `[features] disable` (strict; specs validated at bootstrap — unknown spec = hard error naming it); overlay + schema contracts extended; `every_documented_setting_survives_the_overlay` covers it.
- **[M]** Bug fix folded in: `[permissions] allow` was documented ("skip the ask") but never read at the gate — now an allow source at the same standing as a session "always" (protected paths and deny rules still outrank it).
- **[M]** TUI: `/features` panel (↑↓/⏎/`s`/esc; sections for features, sandbox, tools, MCP servers, skills, agents; a superset of ever-seen tools keeps hidden ones re-enableable), `/sandbox off|fs`, `⊘ N` status badge, transcript card only on the first inventory (re-emits are silent).
- Tests: `feature_toggle_contract.rs` (hide/deny/restore/resume, config baseline, per-tool, skills prompt block, hooks silenced, sandbox swap, MCP spawn-on-enable with the failure named), bare-mode refusal contract, strand snapshot replay, protocol serde fixtures, `parse_specs` validation, CLI flag merge + clap validation.

Known limitation — **fixed 2026-11-19:** `ka serve`/`acp` used to spawn with `Config::default()` and never load the config chain (the `[features]` baseline did not apply there). Both surfaces now load the standard chain per session — user < project (behind the non-interactive trust gate; a server never prompts) < `--config` files < env — and a strict-TOML error fails the request loudly (HTTP 500 / ACP rpc error) instead of silently defaulting.

**Exit:** every capability is switchable at start or mid-conversation with one grammar, strand-persisted and fail-closed, at zero new dependencies.

## Non-goals (explicit, permanent)
No in-process plugin runtime · no vector DB / semantic indexing · no browser or computer control · no image gen / TTS / voice · no enterprise/MDM/team/cloud tier · no telemetry beyond optional local logs · no eval kernels · **no subscription OAuth in core** (`ka-passport` crate may add it later).

Re-ratified 2026-09-13: **git mutation is no longer a non-goal** — `[git] auto_commit`, checkpoint/restore, and worktree-isolated delegates ship destructive-capable git paths behind explicit config/opt-in, while read-only awareness remains the default posture. The original "no git mutation" line was overtaken by shipped, gated features.

Re-reviewed 2026-09-29 (`feature-research-2026-09.md`): MCP OAuth, watch mode, recipes/schedules, package manifests, and shared-write multi-client workspaces were re-examined and **remain out**. Observe-only attach (`ka attach`, Phase 9.6) is re-ratified **in** — the prior "live collab/share relay" decline (8.4) covered write-sharing, which stays out.

## Footprint budget (enforced from Phase 0)
Single static binary ≤ 10MB (musl, stripped) · cold start ≤ 50ms · idle RSS ≤ 15MB · zero network at steady state · children only: user shell, stdio MCP (optional), git (optional).

Ratified 2026-09-14 with Phase 8: **debug adapters join the allowed-children list** (runtime-gated `[debug] enable`, inert in `--safe-mode`) — same trust class as stdio MCP servers.
