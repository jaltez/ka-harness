# Ka as a platform: the programmatic surfaces

Every ka surface drives the same engine through the same wire contract — `ka-protocol`'s
`Command`/`Event` NDJSON enums; surfaces differ only in transport. For building on ka
rather than in the TUI.

## 1. Workspace crates

`crates/*`, one version pinned in the root `Cargo.toml`. Dependencies point one way,
nothing depends upward: `ka-protocol` and `ka-sandbox` have no ka deps; `ka-dialect` and
`ka-strand` sit on `ka-protocol`; `ka-index` on `ka-strand`; `ka-engine` on
protocol+dialect+strand+sandbox; `ka-term` and the `ka` binary (`ka-harness`) on
`ka-engine` and the rest.
A future separate product — Ka Agent (persistent, channel-facing) — builds
on these surfaces from the outside; boundaries and enforcement:
`agent-boundary.md`.

| Crate | Public surface (lib.rs) |
|---|---|
| `ka-protocol` | The contract: `Command`/`Event` (serde-tagged `"type"`, snake_case), id newtypes `StrandId`/`RecordId`/`AskId`, payloads (`Usage`, `ImagePart`, `ContextMeter`, `AskQuestion`, `ReplayedMessage`/`ReplayedCall`, `McpSummary`, `TodoItem`, `ContextPart`), scalars (`Effort`, `Mode`, `Stop`, `ErrorClass`, `DeltaKind`), `to_line`/`from_line` NDJSON helpers. Deps: serde, serde_json, schemars — nothing else. |
| `ka-engine` | The turn machine: `spawn_full(config, catalog, strand) -> EngineHandle`, where `EngineHandle` is two queue ends — `commands: mpsc::Sender<Command>`, `events: mpsc::Receiver<Event>`. Call inside a tokio runtime, send `Command::Prompt`, drain events. `StrandChoice::{New, Latest, Path}` picks the session. Also `spawn`/`spawn_with`/`spawn_with_speaker` (scripted speaker, contract tests, no network), `project_root`, `config::Config`, `hands`/`mcp`/`lsp`/`dap`/`checkpoint`/`trust`/`conventions`/`agents`. |
| `ka-dialect` | Model catalog and wires: `Catalog::embedded()` (+`overlay`, `get`), `Dialect`, `Wire::{openai_chat, openai_responses, anthropic_messages}`, `parse_selector`, local-endpoint `discovery` (Ollama/LM Studio/OpenAI-compatible), the `Speaker` trait, `speaker_for(wire)`. |
| `ka-strand` | Session store — one session = one append-only JSONL file: `StrandFile`/`StrandWriter`, `Record` (serde tag `"record"`), `list`/`latest`/`resolve_id` (id-prefix addressing)/`read`, `render_markdown`, `data_dir`/`strand_dir`. |
| `ka-term` | Terminal primitives: raw-mode `Terminal` guard, `Key` decode, `markdown`/`palette`/`tui` render modules. Only needed to build a TUI. |
| `ka-sandbox` | `[sandbox] mode = "fs"` filesystem policy: `SandboxConfig`, `policy_from_config`, `wrap_command`. Enforcement order bubblewrap → firejail → in-kernel landlock (via the hidden `ka ka-sandbox-exec` trampoline); no tool available fails closed. |
| `ka-index` | SQLite FTS5 (bundled) full-text search over strands: `open_db`, `default_db_path`, `rebuild`, `search`. Optional — behind `ka-harness`'s `index` feature. |

## 2. Headless: `ka run`

`ka run [PROMPT]` (stdin when omitted; empty prompt is an error). Flags:
`--model`, `--mode guarded|accept-edits|free|plan` (aliases `needs-approval`,
`full-access`), `--config F` / `--dialects F` (extra layers, repeatable),
`--no-discovery`, `-c` (continue newest strand, waypoint-aware), `--session ID`
(id prefix or file path), `--trust`, `--schema PATH` (JSON schema file; the
reply must satisfy it — structured output), `--review` (read-only review of
the working tree, forces plan mode), `--print text|ndjson|stream-json` —
text (default) emits the final answer only, non-retryable errors also to
stderr. Permission asks are auto-denied headless (last option). Exit codes:
`0` finished, `1` `Stop::Error` (or setup failure; message on stderr), `2`
`Stop::Aborted`.

- `--print ndjson`: one line per `Event`, exactly the ka-protocol shapes. A
  consumer must handle: `replay` (resumed history), `session_info`, `title`,
  `turn_started`, `delta` (`text`/`thought`/`call`), `call_started`,
  `call_finished`, `call_output`, `ask`, `turn_finished` (`stop`, `usage`),
  `context_meter`, `effort_changed`, `mode_changed`, `model_changed`,
  `digest_started`, `digest_finished`, `idle`, `inventory`, `todos`, `note`,
  `shell_output`, `tasks`, `context_breakdown`, `error`. `idle` terminates
  the stream.
- `--print stream-json`: Claude-Code-shaped lines —
  `{"type":"system","subtype":"init","session":...,"model":...}`; assistant
  text and `tool_use` blocks (text deltas buffer and flush when a tool starts
  or the turn ends); `{"type":"user",...[{"type":"tool_result","tool_use_id":
  ...,"content":...,"is_error":...}]}` per tool output; terminal
  `{"type":"result","subtype":"success|error|aborted","is_error":...,
  "total_cost_usd":...}`. Inventory/meter/note events are dropped.

## 3. HTTP/SSE: `ka serve [--addr 127.0.0.1:8417] [--token T]`

Hand-rolled HTTP/1.1, one connection per request (1 MB / 15 s request caps).
`--token` requires `Authorization: Bearer T` on every route (401 otherwise).

- `GET /health` → `{"ok":true}`.
- `POST /sessions` → spawns an engine → `{"id":"sN"}`. Optional body
  `{"resume":"latest"}` or `{"resume":"<strand id/prefix>"}` (same resolution
  as `ka --session`; bad reference → 400). Sessions run `Config::default()`
  with the embedded catalog in the server's cwd — no config layers, overlays,
  or discovery.
- `POST /sessions/{id}/prompt` `{"text":...,"schema":...}` → `{"ok":true}`;
  the turn runs async.
- `GET /sessions/{id}/events` → SSE: each ka `Event` as `id: <seq>` +
  `data: <NDJSON>`, `: keepalive` comments every 15 s, ends at `idle`. The
  event channel is single-consumer — a second concurrent GET gets an inline
  error line; when a stream ends the receiver is restored so reconnect works.
  `Last-Event-ID: N` is honored only on such a reconnect (a fresh stream
  always delivers the backlog from the start), acknowledged with
  `data: {"type":"replay","resumed_after":N}`.

## 4. ACP: `ka acp`

Agent Client Protocol over line-delimited JSON-RPC 2.0 on stdin/stdout (logging
to stderr only). Client connects, then per session:

- `initialize` → `{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"authMethods":[]}`.
- `session/new` `{params.cwd}` → `{"sessionId":"sN"}` (fresh engine).
- `session/load` `{sessionId, cwd}` — resumes an in-memory session (exact or
  prefix id) or a strand from disk (same prefix resolution as `ka --session`);
  the bootstrap `replay` is forwarded as message-chunk updates before the
  result. Unknown id → -32602.
- `session/prompt` `{sessionId, prompt}` (content array or bare string) →
  `session/update` notifications during the turn (`agent_message_chunk`,
  `agent_thought_chunk`, `tool_call`, `tool_call_update`), then result
  `{"stopReason":"end_turn|max_tokens|cancelled|refusal"}` (from
  `Stop::Done|Length|Aborted|Error`). No structured output on this path.
- Permission asks surface as `session/request_permission` requests (`optionId`
  `"0"`=allow-once, `"1"`=allow-always, `"2"`=reject); a missing or unparseable
  answer denies (never falls back to always-allow).
- `session/cancel` aborts mid-turn. Any other method → -32601.

## 5. Batch seams: `ka export`, `ka sessions`

- `ka export [-o OUT] [--session ID] [--html]` — a strand as markdown (stdout
  by default; `ID` is an id prefix, default newest for cwd, waypoint
  preferred). `--html` renders a self-contained offline page (no `-o`: writes
  `<strand-file-stem>.html` in cwd). Same renderer the TUI's `/export` uses.
- `ka sessions [--json]` — strands for this cwd as a table, or a JSON array of
  `{id, ts, title, messages, path, cost, tokens}`; the ids feed `ka --session`,
  `ka run --session`, serve `resume`, ACP `session/load`. (`ka index`/`ka
  search` query the FTS5 index over the same store.)

## Choosing a surface

Embed in a Rust program → the crates (`spawn_full`, send `Command`, receive
`Event`). Script/CI one-shot → `ka run --print ndjson` (or `stream-json`);
the exit code carries the stop reason. Long-lived service → `ka serve` SSE.
Editor integration → `ka acp`. MCP host → `ka mcp-serve` (read-only hands as
MCP tools over stdio). Batch reporting/auditing → `ka sessions --json`
+ `ka export`.

## Versioning

All workspace crates carry one version (`0.2.10`), pinned through `workspace.dependencies`, and
publish together in lockstep — a surface and its protocol change in the same release. Pre-1.0:
shapes may change between minor versions.

## 6. ka-passport — shipped (batch 11, 2026-11-22; design ratified 2026-09-29)

Subscription OAuth (Claude Pro/Max first) landed as the `ka-passport` crate behind
the ka-harness cargo feature `passport` (default off, per ratification). The seam
is exactly the one this stub fixed:

```rust
/// The TokenSource abstraction core ka-dialect accepts (shipped shape).
pub trait TokenSource: Send + Sync {
    /// Bearer token for the vendor's API, refreshing when stale.
    fn token(&self, vendor: &str) -> Result<String, AuthError>;
    /// Which vendors this source can serve (e.g. ["claude-plan"]).
    fn vendors(&self) -> &[String];
}
```

Current state:

- `ClaudePlanSource` implements the full Claude Pro/Max OAuth flow (PKCE S256,
  loopback callback, token exchange + proactive refresh against
  `console.anthropic.com/v1/oauth/token`); tokens live in the OS keyring under
  service `ka-passport` (injectable `TokenStore` for tests).
- Registration: `ka_passport::claude::install()` registers the source into the
  ka-dialect auth registry (consulted after env/dotenv, before keyring, for
  served vendors). The `claude-plan` vendor rides the `anthropic_messages` wire
  with `Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20` (no
  `x-api-key`).
- CLI: `ka passport login|logout|status`. Safe-mode keeps passport auth
  available (auth is not a customization). Long-tail providers (ChatGPT plans,
  Copilot, …) follow the same `TokenSource` shape.
