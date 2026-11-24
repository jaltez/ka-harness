# Session bench

Per-release performance gate for the interactive surfaces. Measures the
`ka` binary end-to-end — TUI through a real pty, headless `ka run` —
against a deterministic local mock provider, and fails when a metric
drifts past its ceiling in [`baselines.json`](../baselines.json).

```sh
cargo build --release -p ka-harness
cargo xtask bench            # measure + gate (version p50, RSS, session)
cargo xtask bench --update   # re-record baselines on THIS machine
cargo xtask bench --quick    # version p50 + RSS only
cargo xtask bench --bin path/to/artifact   # gate any binary (e.g. musl)
```

Run `--update` on the machine that cuts releases: baselines are
per-machine, gates only fail on upward drift (`base * 1.3` plus a small
absolute floor so tiny metrics aren't jitter-gated).

## What it measures

| metric | meaning |
|---|---|
| `ready_ms` | TUI spawn → first frame on the pty |
| `p_tools4_ms` | one headless prompt with 4 read tool calls (wall) |
| `sess5_cpu_ms` | CPU for a 5-prompt interactive session |
| `sess5_peak_mb` | peak RSS over that session |
| `long_cpu_p25_ms` / `long_cpu_p75_ms` | per-prompt CPU at prompts 25/75 of a 75-prompt session (flatness = no history-growth regression) |
| `long_wall_p75_ms` | per-prompt wall at prompt 75 |
| `long_own_mb` | private memory (smaps rollup) after prompt 75 |
| `write200k_ms` | one write tool call carrying 200 KB |
| `stream60k_cpu_ms` | CPU to render a 60_000-char streamed answer |
| `idle_cpu_pct` | CPU while the TUI sits idle — **livelock guard** (a permanently-ready select arm spins here) |
| `tty_writes_turn` | `write()` syscalls to the tty for one streamed answer — **syscall-storm guard** (needs `strace`; skipped elsewhere) |

The last two are the regression guards for the two classes of bug this
bench caught in the wild: a closed-channel select arm that spins the
event loop at 100% CPU, and per-event drawing that turned one streamed
answer into ~1,400 unbatched pty writes.

## How it works

- `mock_server.py` — OpenAI-chat-compatible SSE mock. Scenarios are
  routed by a tag in the prompt (`BENCH:TOOLS4`, `BENCH:WRITE200K`,
  `BENCH:STREAM60K`); zero model latency, so numbers measure the
  harness, not the provider. Each final answer ends with a
  `KQ7ZT<n>X` marker.
- `session_bench.py` — the driver. Isolates `KA_DATA_DIR`,
  `XDG_CONFIG_HOME`, `HOME`, `TMPDIR` under a temp dir; no real
  providers, keys, or user config are touched. CPU comes from
  `/proc/<pid>/schedstat` (ns resolution), memory from `status` +
  `smaps_rollup`.
- Completion detection uses a **virtual screen model**, not byte
  grepping: ratatui emits minimal cell diffs, so a marker differing
  from the previous turn's by one digit renders as a single changed
  cell and never appears contiguously in the byte stream.

## Context

The metric set mirrors a benchmark suite run against other terminal
agents (Pi-Bolt 1.1.2, octet 0.10.3, Apple M5 — their numbers aren't
reproducible from this repo, so only ka's side is gated). After the
render-path fixes (buffered frame flush + draw coalescing), ka measured
on WSL2 x64: ready 7.7 ms · `-p` with 4 tool calls 14.5 ms · 5-prompt
session 91.5 ms CPU / 9.1 MB peak · per-prompt CPU at p75 20.5 ms ·
own memory 8.8 MB · 200 KB tool write 20 ms · 60k-char stream 24.8 ms
CPU · idle 0.0% · 64 tty writes per turn.

## Startup bench

`startup_bench.py` measures what a user waits for before any work
happens. It takes two builds at once and interleaves their runs, so a
keep-or-revert call never rests on one machine's drift:

```sh
python3 bench/startup_bench.py --side before=target/release/ka --side after=/path/to/ka
```

It prints one JSON line: `ka --version`, a headless turn with the
offline canned speaker (wall and CPU), the time to the TUI's first
frame, the syscall counts behind them, and each binary's size. Every
side runs in its own hermetic home. Its state directory holds 9,000 job
spill files unless `--spills 0` says otherwise, because a machine with
a week of history is the case a start has to survive. `--repo` puts
each side inside a small git repository (`--repo-files N` sets its
size), and `--tui-discovery` starts the TUI the way a user passing
only `--offline` gets it. A run whose output lacks what the path must
print counts as an error, never as a fast run.

The headless turn carries 60 ms of pacing by the canned speaker, a
constant across builds. Compare builds with it, and never read it as
the cost of a real turn.
