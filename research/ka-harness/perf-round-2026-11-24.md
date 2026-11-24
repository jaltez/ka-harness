# Optimization round — 2026-11-24

One bounded round over v0.10.1 (`e4db81d`), asked for as: find where ka legitimately spends time or bytes, in boot, loops, reading, writing and streaming data, structures and patterns, and where the binary carries size. No tricks. Five commits landed on `main`, none pushed. Decision log: [`.audit/perf-2026-11-24.tsv`](../../.audit/perf-2026-11-24.tsv).

## Method

- **Measure first.** `strace` on one hermetic headless turn found the first defect before any source was read: 9,109 `lstat` calls under the state directory. Three read-only scouts then mapped boot, data reads and writes, and hot loops, each claim with a `file:line`.
- **One frozen harness.** [`bench/startup_bench.py`](../../bench/startup_bench.py) runs two builds interleaved in hermetic homes with 9,000 spill files in the state directory, and checks every run's output. Its sensitivity was proved before it was frozen: the same binary takes 104 ms in an empty state and 129 ms with 9,000 spill files, with the ranges apart. The existing `bench/session_bench.py` covers turns with tool calls and streaming.
- **The rule, written before the first attempt.** A change is kept only when the medians of an interleaved run separate past the two sides' ranges, and the syscall counts agree with the mechanism. Wall time alone never decides.
- **The stop predicate.** Each area ends at four measured attempts, or when no hypothesis with a plausible user-visible win is left, with the gate green.

All numbers are from this machine (WSL2, 8 cores, load 1.2 to 2.6), release musl builds.

## Result, first build against last build

Session benchmark, 5 alternating runs per build. "After" was lower in 5 of 5 paired runs on every row here.

| Metric | Before | After | Change |
|---|---|---|---|
| Headless turn with 4 tool calls | 64.5 ms (61.9 to 91.6) | 33.5 ms (33.3 to 34.5) | −48% |
| One 200 KB write tool call | 68.4 ms (66.3 to 70.4) | 40.9 ms (39.2 to 47.0) | −40% |
| TUI first frame | 22.5 ms (20.4 to 23.0) | 12.5 ms (12.1 to 13.6) | −44% |
| Long session, wall at prompt 75 | 48.9 ms (46.3 to 77.9) | 39.3 ms (35.6 to 62.5) | −20% |
| Peak memory, 5-prompt session | 9.9 MB | 9.4 MB | −0.5 MB |
| CPU per prompt, streaming CPU, idle CPU, terminal writes per turn | | | no measurable difference |

Startup harness, 15 interleaved runs per build, 9,000 spill files, TUI started with `--offline` only.

| Metric | Before | After | Change |
|---|---|---|---|
| Headless turn, wall | 134.5 ms (128.1 to 141.5) | 89.3 ms (87.6 to 92.1) | −34% |
| Headless turn, CPU | 50.7 ms (47.2 to 57.9) | 16.0 ms (14.3 to 19.0) | −68% |
| TUI first frame | 39.0 ms (34.5 to 46.3) | 16.5 ms (14.7 to 20.4) | −58% |
| `lstat` calls in a headless turn | 10,286 | 0 | |
| System calls in a headless turn | 12,240 | 1,403 | −89% |
| `ka --version` | 1.63 ms | 1.55 ms | no measurable difference |
| Binary | 8,366,064 bytes | 8,361,968 bytes | −4,096 bytes |

The headless turn includes 60 ms of pacing by the canned speaker, so its CPU is the number to read.

## What changed

Each is one commit, in this order.

1. **`acd9b4b` The spill cleanup leaves the startup path.** Every engine start listed the spill directory and statted each entry to age it. On this machine that directory held 9,109 files. The scan now runs on a background thread, at most once a day per machine, dated by a stamp file beside the directory. Headless turn: 10,286 `lstat` calls to 0, CPU 48.0 ms to 22.9 ms.
2. **`f4d4996` A turn's records reach the session file in one write and one sync.** Each record used to open the file and sync on its own, so a turn paid one sync per record at the 5 to 18 ms the filesystem's journal takes. The byte ceiling is still decided record by record, and records past it stay in memory, in order. Headless turn with 4 tool calls: 63.9 ms to 42.9 ms across both halves of the change.
3. **`c5a2257` The catalog is built from the parsed table.** Every process parsed the 208 KB model table, merged it, printed it back to TOML and parsed that again. Headless turn CPU 24.0 ms to 15.5 ms, first frame 25.3 ms to 16.4 ms, binary 8,192 bytes smaller.
4. **`5dd924d` The TUI honours `--offline` for endpoint discovery.** The headless path already did, and the flag's help text promises it. A server listens on port 11434 on this machine, so every `ka --offline` start made a real HTTP request before its first frame. First frame 29.3 ms to 16.0 ms.
5. **`690bc8c` The side question asks git for the branch alone.** It captured a full snapshot and threw away the status walk. In a repository of 20,000 files `git status` takes 18.2 ms and the branch command 1.6 ms. This is a call-level measurement: the path needs a fast model role, which the harness has no model for.

## What was tried and rejected

- **One git command per snapshot instead of two** (`status --porcelain --branch`). Process count fell from 25 `execve` calls to 13, but the turn moved 1.2 ms in a small repository and 0.8 ms in one of 20,000 files, inside the ranges. It saves only the cheap command, and it needs a parser for git's header format. Reverted.
- **Optimisation level `s` instead of `z`.** 9,234,416 bytes, 872 KB larger.
- **Optimisation level `3`.** 12,122,096 bytes, over the 10 MB contract.
- **The command-line parser without colour and typo suggestions.** 16 KB smaller, 0.2%, and `ka --verson` stops answering "did you mean `--version`". Not worth it.

The catalog change grew the binary by 48 KB in its first version: a second typed reader for the same types was compiled in beside the old one. Routing every catalog construction through one step removed the old reader and turned that into a net reduction.

## Examined and left alone

- **Streaming redraw.** Two deep copies of the rendered rows per 80 ms tick are real (`tui.rs`, `render_live_md` and `live_region_rows`). The whole path costs about 37 ms of CPU for a 60,000-character answer, so even removing the copies cannot produce a user-visible win.
- **Resume's repeated reads.** The target session is read two or three times. The largest session file here is 0.9 MB.
- **The snapshot manifest.** Parsed whole at every engine start. The largest here is 1 KB.
- **`ka index`.** An O(n²) purge and one commit per file. The `index` feature is not in the default build.
- **Dependency features.** Already curated in earlier rounds: no multi-thread runtime scheduler, regex Unicode tables trimmed, the HTTP client without defaults, the script engine behind an off-by-default feature.

## Findings that need a decision

- **The spill directory grows without bound inside its window.** Every backgrounded tool output creates a spill file, and its completion marker is a second file. About 1,800 files a day on this machine, 130 MB over five days, with 5,205 files carrying no completion marker and an empty job ledger. The retention is seven days, so the directory will settle near 13,000 files. `ka gc` reclaims expired spills on request.
- **The `@` mention walk ignores `.gitignore`.** `walk_files` collects every entry, sorts the list and keeps the first 500. In this repository it visits 27,220 entries. The 500th kept entry is `research/repos/aider/aider/website/assets/audio/.../07-43.mp3`, so an ignored vendored tree uses up the list and the design notes under `research/ka-harness/` cannot be mentioned. The list also includes the private signing key in the repository root. Which files should be mentionable is a product decision: honouring `.gitignore` (the `ignore` crate is already in the binary) or taking the 500 breadth-first would each fix both the speed and the reach.
- **The CI quick gate has almost no margin.** It takes the median of five `ka --version` runs against a baseline from v0.6.0. Three runs of one commit measured 2.6, 3.0 and 3.2 ms against a 3.08 ms limit. This round makes that path no slower. Refreshing the baseline is the release machine's call.

## Best ideas if pushed further

1. **Tool schemas as data.** Each hand derives its JSON schema at runtime. Static schema text would drop `schemars` and its generated code. Size, unmeasured.
2. **A compact embedded catalog.** The 208 KB model table is stored as TOML text and parsed on every start. A pre-serialised compact form would cut both. Needs a build step and a format to version.
3. **The TUI's per-frame row lookup.** `Transcript::row(i)` walks from the first entry for each visible row, O(visible × entries) per frame. Small at today's session lengths.
4. **The per-round history clone.** Each model round deep-clones the whole history, then the wire copies it again. Linear in history per round, and small next to a network round trip.

## Mistakes made along the way

- Two patches landed on the wrong lines because the line numbers came from an unnumbered read or from before a reformat. One overwrote the line that writes a batch to the session file. The engine suite caught it at once: nine tests failed, among them "strand persists and resumes".
- A diagnostic syscall count ran from the repository's directory instead of the harness's empty one, so git did real work and the two sides were not comparable.
- An estimate of 20 ms for a change measured 0.8 ms. The estimate counted a code path that needs a fast model role and never runs in the harness.
- Re-adding a dependency feature after a trim let cargo resolve a Windows dependency to a different version. The committed lockfile was restored by hand.

## Running it again

```sh
cargo build -p ka-harness --release --target x86_64-unknown-linux-musl
python3 bench/startup_bench.py --side before=<old binary> --side after=<new binary> --tui-discovery --syscalls
python3 bench/session_bench.py --ka <binary> --out results.json
```
