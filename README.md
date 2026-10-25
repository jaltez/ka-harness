# ka

A model-agnostic, very-low-footprint coding agent in Rust. One static binary, no daemons, no telemetry.

> ⚠️ **Under heavy development.** ka is experimental and moving fast — features, commands, config, and behavior may change or break without notice. Not ready for production use.

📖 **Full documentation: <https://ka-harness.dev/docs>** — this README sticks to the basics; each topic below links to its detailed page there.

## Install

```sh
cargo install ka-harness                 # from crates.io (≤ 0.2.11 shipped as `ka-agent`)
cargo install --path crates/ka-harness   # from a clone
ka doctor                                # sanity-check the environment
```

Details and requirements: [installation](https://ka-harness.dev/docs/getting-started/installation/).

## Quickstart

```sh
cd your-project
ka --model ollama/qwen3.5:9b        # TUI, fresh session — fully local works
ka -c                               # continue this terminal's last session
ka --session 3f9c2a81               # resume by id prefix or name
ka run "summarize the build error"  # one headless turn, NDJSON events on stdout
```

Essential keys: `Enter` send or steer mid-turn · `⇧Enter` newline (`Ctrl+J` fallback) · `Esc`/`Ctrl-C` abort · `/help` for the rest.


Markdown answers render with shaded code panels, tables, and mermaid diagrams drawn as box-drawing flowcharts, sequence and state diagrams, quadrant planes; anything the renderer does not cover keeps its source panel.

## Models

`vendor/model@effort` — e.g. `anthropic/claude-sonnet-5@high`, `ollama/qwen3.5:9b` (`@` because model ids may contain colons).

Three wire protocols (Anthropic messages, OpenAI chat, OpenAI Responses) cover essentially every endpoint, including all OpenAI-compatible ones. Ollama and LM Studio servers are auto-discovered; vLLM and llama.cpp endpoints are built in as keyless providers (selector-only) alongside a catalog of 500+ models shipped in the binary. `/model` opens the picker listing only configured providers (key present or keyless); `/model <vendor>` narrows it to one vendor, and any unlisted `vendor/model` selector can still be typed through. `ka models` and `ka providers` list from the terminal.

More: [selectors](https://ka-harness.dev/docs/models/selectors/) · [providers & keys](https://ka-harness.dev/docs/models/providers/) · [usage & pricing](https://ka-harness.dev/docs/models/usage/).

## Config

Strict TOML, layered: defaults → `~/.config/ka/ka.toml` → `.ka/ka.toml` (project, trust-gated) → env (`KA_MODEL`, `KA_MODE`) → flags. Unknown keys are hard errors with line numbers. The project layer lives at the project root (nearest `.git` ancestor), so sessions started in a subdirectory share one `.ka/`.

```toml
model = "ollama/qwen3.5:9b"
mode = "accept_edits"       # guarded | accept_edits | free | plan

[[rules]]                   # first match wins, before mode logic
tool = "bash"
pattern = "cargo *"         # glob on the call's primary argument
verdict = "allow"           # allow | ask | deny
```

Every key with its default: [ka.toml reference](https://ka-harness.dev/docs/reference/config/).

## Safety

- **Clearances** (read / write / exec) and four permission modes, `guarded` → `free`.
- **Unbypassable hardstops** — root rm, fork bombs, fetch-and-execute: they ask even in free mode; headless denies.
- **Read ledger** — edits refuse files the model has not read, or that changed since.
- **Secret redaction**, one-way, in every tool result.
- **Optional fs sandbox** for bash children (bwrap / firejail / landlock), fail-closed.
- **`ka --safe-mode`** — the troubleshooting floor: every customization off, built-ins kept.

Details — doom-loop rules, protected paths, project trust, runtime feature toggles: [safety model](https://ka-harness.dev/docs/concepts/safety/).

## Surfaces

| Surface | Use |
|---|---|
| `ka` | terminal UI |
| `ka run "…"` | headless one-turn, NDJSON events, exit 0/1/2 |
| `ka serve` | HTTP: prompt via POST, SSE event streams, read-only observers |
| `ka acp` | Agent Client Protocol for editors |

Also: `ka sessions` · `ka export` (markdown/HTML) · `ka rewind` · `ka attach` · `ka completions` · `ka init` · `ka update`.

## Extend

- **Skills** — SKILL.md directories, project or user scope ([docs](https://ka-harness.dev/docs/extending/skills/))
- **Subagents** — markdown agents with frontmatter, incl. background delegates ([docs](https://ka-harness.dev/docs/extending/subagents/))
- **Hooks** — lifecycle hooks that block, patch, or steer a turn ([docs](https://ka-harness.dev/docs/extending/hooks/))
- **MCP servers** — stdio and HTTP; tools appear as `<server>.<tool>` hands ([docs](https://ka-harness.dev/docs/extending/mcp/))

Opt-in integrations, all config tables: **LSP** (diagnostics, navigation, rename/format), **DAP debugging**, the **verify loop** (auto-lint / auto-test after edits), **runtime feature toggles** (`ka --disable agents,mcp:github,tool:bash` — fail-closed), and **Herdr** (`[tui] herdr = true` — reports the agent's `idle`/`working`/`blocked` state, its resume command, and exit to the pane that hosts it; inert outside Herdr). See the [config reference](https://ka-harness.dev/docs/reference/config/).

ka reads the ecosystem automatically: `AGENTS.md` (`CLAUDE.md` compatible) and `.agents/` / `.claude/` directories. Every session is an append-only JSONL [strand](https://ka-harness.dev/docs/concepts/strands/) — rewind, fork, export, replay. `/tree` shows the session's fork tree and branches from any past input: picking one forks the strand there (original stays intact) and the chat continues from that point. The [harness loop](https://ka-harness.dev/docs/concepts/harness/) reads the model's own todo list back and pushes on claims that don't hold up.

## Development

```sh
cargo xtask ci                # the CI gate: fmt --check + clippy -D warnings + tests
cargo build -p ka-harness     # debug build
cargo xtask dev -- models     # rebuild + run dev binary with args
cargo xtask size              # binary size vs the 10 MB contract
cargo xtask deps              # dependency-graph allowlist gate
cargo xtask link              # build release + symlink `kad` → target/release/ka
```

Stable owns the name `ka`; dev is always `kad`. Isolate dev runs with `KA_DATA_DIR=/tmp/ka-dev kad …`.

Design docs live in [`research/ka-harness/`](research/ka-harness/): [architecture](research/ka-harness/architecture.md), [roadmap](research/ka-harness/roadmap.md), and the [survey of 17 agents](research/README.md) that informed ka. `research/` is a survey and decision record, not product documentation — that is [the docs site](https://ka-harness.dev/docs/).

### Crates

| Crate | Role |
|---|---|
| `ka-protocol` | Command/Event wire enums (NDJSON-shaped) |
| `ka-engine` | turn machine, tools (Hands) + clearance gate, digests, ledger |
| `ka-dialect` | model catalog + wires (anthropic-messages, openai-chat, openai-responses) + local discovery |
| `ka-strand` | append-only JSONL session store, waypoints |
| `ka-term` | ratatui TUI |
| `ka-harness` | the published `ka` binary |
| `ka-index` / `ka-sandbox` | optional: FTS search (`index` feature), fs sandboxing |

Dependencies flow one way: every crate may depend on `ka-protocol`; only the surfaces (`ka-harness`, `ka-term`) depend on `ka-engine`. The engine never imports a surface; `ka-protocol` is pure serde types with no I/O. Overview: [architecture](https://ka-harness.dev/docs/concepts/architecture/).

## Footprint contract

CI-gated on every push: **≤ 10 MB** stripped musl binary (currently 6.25 MB) · cold start **≤ 50 ms** · idle RSS **≤ 15 MB** · no steady-state network (one rate-limited update check at startup; `[update] check = "off"` disables it). ~790 tests, `clippy -D warnings` clean. Any new dependency must justify itself.

## Ka and the future Ka Agent

`ka` (this repo) is the engine and the terminal experience. **Ka Agent** is a planned, separate product: an optional persistent agent — long-term memory, messaging channels, web UI, automations — that builds on ka's stable surfaces (`ka serve`, `ka run`, `ka acp`, the strand format, the crate APIs) or runs the `ka` binary as a subprocess. Never inside this workspace, never a dependency of the binary; CI gates the dependency graph so it stays that way. Decision record: [`research/ka-harness/agent-boundary.md`](research/ka-harness/agent-boundary.md).
