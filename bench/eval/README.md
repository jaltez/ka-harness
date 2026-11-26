# Ka vs Codex CLI — Curated Coding Task Eval Suite

A lean, reproducible, 15-task real-world coding benchmark for apples-to-apples
comparison between `ka` and OpenAI's `codex` CLI (or any other harness).

## Parity Protocol

To ensure fair comparisons that isolate **harness quality and efficiency** rather
than model or environment differences, every run enforces:

1. **Same pinned model & reasoning effort**: both harnesses target the exact same
   pinned model snapshot and reasoning effort level over the `openai_responses`
   wire (`--model openai/<model>@<effort>` for `ka`, `--codex-model <model>` for
   `codex`).
2. **Ephemeral, hermetic runs**: every `(task, trial, harness)` execution runs in
   a freshly initialized temporary git repository with isolated `HOME`,
   `XDG_CONFIG_HOME`, `KA_DATA_DIR`, `CODEX_HOME`, and `TMPDIR`. Both harnesses
   skip user config and permission rules (`--ephemeral --ignore-user-config
   --ignore-rules`).
3. **Randomized interleaved order**: the full `(task, trial, harness)` matrix is
   shuffled with a recorded PRNG seed (`--seed`) so neither harness systematically
   benefits from warm provider prompt caches or time-of-day load shifts.
4. **Multiple trials (`N >= 3`)**: default `--trials 3` captures variance and
   measures both `pass@1` and pass consistency across repeated attempts.
5. **Deterministic automated grading**: every task defines a `verify_cmd` that
   exits `0` only when the solution is complete and correct, and is proven to
   **fail on the buggy baseline** before any agent runs (`--dry-run`).
6. **Single shared price table**: `report.py` computes USD cost for both
   harnesses from one shared price table using raw token counts (`input`,
   `cached_input`, `output`, `reasoning`), eliminating provider-side pricing
   discrepancies.

## Task Categories (15 Tasks)

| # | ID | Lang | Category | Focus |
|---|---|---|---|---|
| 1 | `py-csv-dialect` | Python | Bugfix | RFC 4180 quoted-field & embedded-newline CSV parser |
| 2 | `py-lru-ttl` | Python | Bugfix / Edge-case | LRU cache with TTL expiry & capacity eviction |
| 3 | `py-semver-sort` | Python | Edge-case | SemVer 2.0 prerelease & numeric precedence sorting |
| 4 | `py-async-rate-limiter` | Python | Concurrency | Thread-safe token-bucket rate limiter race fix |
| 5 | `py-ini-refactor` | Python | Multi-file refactor | Extract shared config parser + fix interpolation |
| 6 | `py-json-patch` | Python | Bugfix | RFC 6902 JSON Patch `move`/`copy`/`test` + escaping |
| 7 | `py-log-perf` | Python | Perf optimization | Optimize O(N^2) log aggregator to linear time |
| 8 | `rs-ring-buffer` | Rust | Bugfix | Fixed-capacity ring buffer wraparound & iterator |
| 9 | `rs-glob-match` | Rust | Parser fix | Glob matcher `*`, `?`, `**`, and `[a-z]` classes |
| 10 | `rs-kv-wal` | Rust | Bugfix / Durability | Append-only KV WAL truncated-tail recovery |
| 11 | `rs-cli-flags` | Rust | Failing test repair | Multi-file CLI flag parser (`--key=val`, `--`, `-abc`) |
| 12 | `rs-parallel-merge` | Rust | Concurrency | Fix deadlock/race in bounded worker-pool aggregator |
| 13 | `sh-envsubst-safe` | Shell | Edge-case | Safe `.env` loader handling spaces, quotes, `#`, `=` |
| 14 | `cli-json-diff` | Python/JSON | CLI tool | Structural JSON diff CLI with exit codes & ignore paths |
| 15 | `cli-toml-merge` | Python/TOML | Multi-file refactor | Deep-merge TOML configs preserving arrays/override semantics |

## Usage

### 1. Validate all 15 task baselines (no API keys required)

```sh
python3 bench/eval/run.py --dry-run
```

Every task's `verify_cmd` must fail on the unmodified baseline repository.

### 2. Run the eval suite

```sh
python3 bench/eval/run.py \
  --harnesses ka,codex \
  --model openai/gpt-5.2@medium \
  --codex-model gpt-5.2 \
  --trials 3 \
  --tasks all \
  --out /tmp/eval-run-01
```

Run a subset of tasks or `ka` alone:

```sh
python3 bench/eval/run.py \
  --harnesses ka \
  --model openai/gpt-5.2@medium \
  --trials 3 \
  --tasks py-csv-dialect,rs-ring-buffer \
  --out /tmp/eval-smoke
```

### 3. Generate normalized JSON + Markdown report

```sh
python3 bench/eval/report.py \
  --in /tmp/eval-run-01 \
  --out-json /tmp/eval-run-01/report.json \
  --out-md /tmp/eval-run-01/report.md
```
