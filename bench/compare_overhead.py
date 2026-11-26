#!/usr/bin/env python3
"""Interleaved A/B overhead benchmark comparing `ka` and `codex` (or `ka` standalone).

Measures harness overhead in hermetic per-side environments with zero model
latency against the local `/v1/responses` mock server (`mock_server.py`):

  1. Binary size on disk (bytes & MiB)
  2. `--version` cold start wall time (p50, p95, min, max, IQR) + peak RSS
  3. Headless 4-tool-call loop against local `/v1/responses` mock server
  4. 9,000-spill startup latency (for `ka`)

Outputs both a human-readable Markdown comparison table and a structured JSON
provenance artifact.

Usage:
  compare_overhead.py [--ka PATH] [--codex PATH | --no-codex]
                      [--runs 10] [--warmups 2] [--spills 9000]
                      [--out-json results.json] [--out-md report.md]
"""
import argparse
import datetime
import hashlib
import json
import math
import os
import platform
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.dirname(HERE)
NOW_MS = 1795521590416


def default_path_env():
    return os.environ.get("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def cpu_model():
    try:
        with open("/proc/cpuinfo") as f:
            for line in f:
                if line.lower().startswith("model name"):
                    return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor() or "unknown"


def percentile(sorted_vals, pct):
    if not sorted_vals:
        return None
    if len(sorted_vals) == 1:
        return sorted_vals[0]
    k = (len(sorted_vals) - 1) * (pct / 100.0)
    f = math.floor(k)
    c = math.ceil(k)
    if f == c:
        return sorted_vals[int(k)]
    return sorted_vals[f] + (sorted_vals[c] - sorted_vals[f]) * (k - f)


def summarize(samples):
    if not samples:
        return {"runs": 0, "p50": None, "p95": None, "min": None, "max": None, "iqr": None, "samples": []}
    s = sorted(samples)
    q1 = percentile(s, 25)
    q3 = percentile(s, 75)
    return {
        "runs": len(s),
        "p50": round(percentile(s, 50), 3),
        "p95": round(percentile(s, 95), 3),
        "min": round(s[0], 3),
        "max": round(s[-1], 3),
        "iqr": round(q3 - q1, 3),
        "samples": [round(x, 3) for x in samples],
    }


def parse_time_v_rss_kb(stderr_text):
    m = re.search(r"Maximum resident set size \(kbytes\):\s*(\d+)", stderr_text)
    return int(m.group(1)) if m else None


def run_timed(argv, cwd, env, timeout=60, use_time_v=True):
    """Run command and return (returncode, stdout, stderr, wall_ms, peak_rss_kb)."""
    time_bin = shutil.which("/usr/bin/time") or ("/usr/bin/time" if os.path.exists("/usr/bin/time") else None)
    actual_argv = [time_bin, "-v", *argv] if (use_time_v and time_bin) else argv

    t0 = time.perf_counter_ns()
    proc = subprocess.Popen(
        actual_argv,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    try:
        out_b, err_b = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        # the direct child is `/usr/bin/time`; kill the whole group or the
        # harness keeps the pipes open and communicate() blocks forever
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            out_b, err_b = proc.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            out_b, err_b = b"", b""
        wall_ms = (time.perf_counter_ns() - t0) / 1e6
        return 124, out_b.decode(errors="replace"), err_b.decode(errors="replace"), wall_ms, None

    wall_ms = (time.perf_counter_ns() - t0) / 1e6
    err_s = err_b.decode(errors="replace")
    rss_kb = parse_time_v_rss_kb(err_s) if (use_time_v and time_bin) else None
    return proc.returncode, out_b.decode(errors="replace"), err_s, wall_ms, rss_kb


def run_wall_only(argv, cwd, env, timeout=60):
    """Run command without `/usr/bin/time` wrapper so cold-start timing has zero wrapper overhead."""
    t0 = time.perf_counter_ns()
    proc = subprocess.Popen(
        argv,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    try:
        out_b, err_b = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            out_b, err_b = proc.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            out_b, err_b = b"", b""
        wall_ms = (time.perf_counter_ns() - t0) / 1e6
        return 124, out_b.decode(errors="replace"), err_b.decode(errors="replace"), wall_ms
    wall_ms = (time.perf_counter_ns() - t0) / 1e6
    return proc.returncode, out_b.decode(errors="replace"), err_b.decode(errors="replace"), wall_ms


def make_spills(state_dir, count):
    spills = os.path.join(state_dir, "ka", "spills")
    os.makedirs(spills, exist_ok=True)
    six_days_ms = 6 * 24 * 60 * 60 * 1000
    body = b"x" * 2048
    for i in range(count):
        stamp = NOW_MS - (i * six_days_ms // max(count, 1))
        name = f"job-{stamp}-{i}"
        with open(os.path.join(spills, name), "wb") as f:
            f.write(body)
        if i % 7 == 0:
            with open(os.path.join(spills, name + ".done"), "wb") as f:
                f.write(b"")
    with open(os.path.join(state_dir, "ka", "jobs.jsonl"), "wb"):
        pass


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def start_mock_server(port):
    proc = subprocess.Popen(
        [sys.executable, os.path.join(HERE, "mock_server.py"), str(port)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    deadline = time.monotonic() + 10.0
    while time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), 0.2):
                return proc
        except OSError:
            if proc.poll() is not None:
                raise RuntimeError("mock_server.py exited prematurely")
            time.sleep(0.05)
    proc.kill()
    raise RuntimeError("mock_server.py failed to bind")


def setup_side(work, name, port):
    root = os.path.join(work, name)
    dirs = {
        "HOME": os.path.join(root, "home"),
        "XDG_CONFIG_HOME": os.path.join(root, "config"),
        "XDG_STATE_HOME": os.path.join(root, "state"),
        "XDG_DATA_HOME": os.path.join(root, "data"),
        "KA_DATA_DIR": os.path.join(root, "data", "ka"),
        "CODEX_HOME": os.path.join(root, "codex"),
        "TMPDIR": os.path.join(root, "tmp"),
    }
    for d in dirs.values():
        os.makedirs(d, exist_ok=True)
    cwd = os.path.join(root, "cwd")
    os.makedirs(cwd, exist_ok=True)
    for i in range(1, 5):
        with open(os.path.join(cwd, f"f{i}.txt"), "w") as f:
            f.writelines(f"line {i}: filler content for file {i} {n}\n" for n in range(1, 41))

    env = dict(dirs)
    env["PATH"] = default_path_env()
    env["TERM"] = "xterm-256color"
    env["OPENAI_API_KEY"] = "sk-bench-mock"
    env["OPENAI_BASE_URL"] = f"http://127.0.0.1:{port}/v1"

    overlay = None
    if name == "ka":
        overlay = os.path.join(root, "dialects.toml")
        with open(overlay, "w") as f:
            f.write(
                '[dialects."bench/mock"]\n'
                'wire = "openai_responses"\n'
                f'base_url = "http://127.0.0.1:{port}/v1"\n'
                "context = 200000\n"
            )
    elif name == "codex":
        with open(os.path.join(dirs["CODEX_HOME"], "config.toml"), "w") as f:
            f.write(
                'model = "bench-mock"\n'
                'model_provider = "bench"\n'
                "\n"
                "[model_providers.bench]\n"
                'name = "bench"\n'
                f'base_url = "http://127.0.0.1:{port}/v1"\n'
                'wire_api = "responses"\n'
                'env_key = "OPENAI_API_KEY"\n'
            )

    return env, cwd, overlay


def binary_info(path, flags):
    real = os.path.realpath(path)
    size = os.path.getsize(real)
    ver_proc = subprocess.run([path, "--version"], capture_output=True, text=True, timeout=15)
    version = (ver_proc.stdout.strip() or ver_proc.stderr.strip()).splitlines()[0] if (ver_proc.stdout or ver_proc.stderr) else "unknown"
    return {
        "path": os.path.abspath(path),
        "realpath": real,
        "sha256": sha256_file(real),
        "version": version,
        "size_bytes": size,
        "size_mib": round(size / (1024 * 1024), 2),
        "flags": flags,
    }


def fmt_val(v, unit=""):
    if v is None:
        return "n/a"
    return f"{v:.2f}{unit}" if isinstance(v, float) else f"{v}{unit}"


def render_markdown(report):
    sides = list(report["binaries"].keys())
    has_codex = "codex" in sides
    lines = []
    lines.append("# Harness Overhead Comparison")
    lines.append("")
    h = report["host"]
    lines.append(
        f"- **Date (UTC):** {report['timestamp_utc']}  \n"
        f"- **Host:** {h['os']} {h['kernel']} ({h['arch']}) · {h['cpu_model']} ({h['cpu_count']} cores)  \n"
        f"- **Config:** {report['config']['runs']} interleaved runs, {report['config']['warmups']} warmups, {report['config']['spills']} spills"
    )
    lines.append("")
    for name, info in report["binaries"].items():
        lines.append(
            f"- **`{name}`:** `{info['version']}` · {info['size_mib']:.2f} MiB ({info['size_bytes']:,} B) · sha256 `{info['sha256'][:12]}`"
        )
    lines.append("")

    if has_codex:
        lines.append("| Metric | `ka` (p50) | `ka` (p95 / IQR) | `codex` (p50) | `codex` (p95 / IQR) | Ratio (`codex/ka`) |")
        lines.append("|---|---:|---:|---:|---:|---:|")
    else:
        lines.append("| Metric | `ka` (p50) | `ka` (p95) | `ka` (min–max) | `ka` (IQR) |")
        lines.append("|---|---:|---:|---:|---:|")

    def add_row(label, unit, ka_stat, cx_stat=None):
        ka_p50 = ka_stat.get("p50") if ka_stat else None
        ka_p95 = ka_stat.get("p95") if ka_stat else None
        ka_iqr = ka_stat.get("iqr") if ka_stat else None
        ka_min = ka_stat.get("min") if ka_stat else None
        ka_max = ka_stat.get("max") if ka_stat else None
        if has_codex:
            cx_p50 = cx_stat.get("p50") if cx_stat else None
            cx_p95 = cx_stat.get("p95") if cx_stat else None
            cx_iqr = cx_stat.get("iqr") if cx_stat else None
            ratio = f"{cx_p50 / ka_p50:.2f}x" if (ka_p50 and cx_p50 and ka_p50 > 0) else "n/a"
            lines.append(
                f"| {label} | {fmt_val(ka_p50, unit)} | {fmt_val(ka_p95, unit)} / {fmt_val(ka_iqr, unit)} "
                f"| {fmt_val(cx_p50, unit)} | {fmt_val(cx_p95, unit)} / {fmt_val(cx_iqr, unit)} | {ratio} |"
            )
        else:
            lines.append(
                f"| {label} | {fmt_val(ka_p50, unit)} | {fmt_val(ka_p95, unit)} "
                f"| {fmt_val(ka_min, unit)}–{fmt_val(ka_max, unit)} | {fmt_val(ka_iqr, unit)} |"
            )

    ka_sz = report["binaries"]["ka"]["size_mib"]
    if has_codex:
        cx_sz = report["binaries"]["codex"]["size_mib"]
        ratio = f"{cx_sz / ka_sz:.2f}x" if ka_sz > 0 else "n/a"
        lines.append(f"| Binary size | {ka_sz:.2f} MiB | — | {cx_sz:.2f} MiB | — | {ratio} |")
    else:
        lines.append(f"| Binary size | {ka_sz:.2f} MiB | — | — | — |")

    m = report["metrics"]
    add_row("`--version` cold start", " ms", m["version_wall_ms"].get("ka"), m["version_wall_ms"].get("codex"))
    add_row("`--version` peak RSS", " MiB", m["version_rss_mib"].get("ka"), m["version_rss_mib"].get("codex"))
    add_row("Headless 4-tool loop (`/v1/responses`)", " ms", m["tools4_wall_ms"].get("ka"), m["tools4_wall_ms"].get("codex"))
    add_row("Headless 4-tool peak RSS", " MiB", m["tools4_rss_mib"].get("ka"), m["tools4_rss_mib"].get("codex"))
    add_row(f"{report['config']['spills']}-spill startup (`ka`)", " ms", m["spills_wall_ms"].get("ka"), None)
    add_row(f"{report['config']['spills']}-spill peak RSS (`ka`)", " MiB", m["spills_rss_mib"].get("ka"), None)
    lines.append("")
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser(description="Interleaved A/B overhead benchmark: ka vs codex")
    ap.add_argument("--ka", default=None, help="Path to ka binary (default: target/release/ka or PATH)")
    ap.add_argument("--codex", default=None, help="Path to codex binary (default: auto-detect on PATH)")
    ap.add_argument("--no-codex", action="store_true", help="Run ka standalone even if codex is on PATH")
    ap.add_argument("--runs", type=int, default=10, help="Interleaved timed runs per metric (default: 10)")
    ap.add_argument("--warmups", type=int, default=2, help="Warmup runs per side before timing (default: 2)")
    ap.add_argument("--spills", type=int, default=9000, help="Spill file count for ka startup test (default: 9000)")
    ap.add_argument("--out-json", default=None, help="Write structured JSON provenance artifact here")
    ap.add_argument("--out-md", default=None, help="Write Markdown table report here")
    args = ap.parse_args()

    ka_path = args.ka
    if not ka_path:
        cand = os.path.join(REPO_ROOT, "target", "release", "ka")
        ka_path = cand if os.path.isfile(cand) else shutil.which("ka")
    if not ka_path or not os.path.exists(ka_path):
        print("compare_overhead: could not locate `ka` binary (pass --ka PATH)", file=sys.stderr)
        return 2
    ka_path = os.path.abspath(ka_path)

    codex_path = None
    if not args.no_codex:
        codex_path = args.codex or shutil.which("codex")
        if args.codex and not os.path.exists(args.codex):
            print(f"compare_overhead: --codex binary not found: {args.codex}", file=sys.stderr)
            return 2
        if codex_path:
            codex_path = os.path.abspath(codex_path)

    work = tempfile.mkdtemp(prefix="ka-compare-overhead-")
    port = free_port()
    server = None
    try:
        server = start_mock_server(port)
        ka_env, ka_cwd, ka_overlay = setup_side(work, "ka", port)
        ka_tools4_flags = [
            "run",
            "--dialects", ka_overlay,
            "--model", "bench/mock",
            "--no-discovery",
            "--print", "ndjson",
            "--ephemeral",
            "--ignore-user-config",
            "--ignore-rules",
            "--mode", "allow",
        ]
        sides = [{
            "name": "ka",
            "path": ka_path,
            "env": ka_env,
            "cwd": ka_cwd,
            "tools4_base": [ka_path, *ka_tools4_flags],
            "flags_doc": ka_tools4_flags,
        }]

        if codex_path:
            cx_env, cx_cwd, _ = setup_side(work, "codex", port)
            cx_tools4_flags = [
                "exec",
                "--json",
                "--ephemeral",
                "--ignore-rules",
                "--skip-git-repo-check",
                "--dangerously-bypass-approvals-and-sandbox",
                "-m", "bench-mock",
            ]
            sides.append({
                "name": "codex",
                "path": codex_path,
                "env": cx_env,
                "cwd": cx_cwd,
                "tools4_base": [codex_path, *cx_tools4_flags],
                "flags_doc": cx_tools4_flags,
            })

        # 1. Warmups
        for s in sides:
            for w in range(args.warmups):
                run_wall_only([s["path"], "--version"], s["cwd"], s["env"])
                run_wall_only(
                    [*s["tools4_base"], f"BENCH:TOOLS4 n=w{w} read the four files and summarize"],
                    s["cwd"], s["env"],
                )

        ver_wall = {s["name"]: [] for s in sides}
        ver_rss = {s["name"]: [] for s in sides}
        ver_err = {s["name"]: 0 for s in sides}
        t4_wall = {s["name"]: [] for s in sides}
        t4_rss = {s["name"]: [] for s in sides}
        t4_err = {s["name"]: 0 for s in sides}

        # 2. Interleaved --version cold start (wall without wrapper + peak RSS via /usr/bin/time -v)
        for i in range(args.runs):
            order = sides if i % 2 == 0 else sides[::-1]
            for s in order:
                rc, out, _, ms = run_wall_only([s["path"], "--version"], s["cwd"], s["env"])
                if rc == 0 and out.strip():
                    ver_wall[s["name"]].append(ms)
                else:
                    ver_err[s["name"]] += 1
                rc2, _, _, _, rss_kb = run_timed([s["path"], "--version"], s["cwd"], s["env"])
                if rc2 == 0 and rss_kb is not None:
                    ver_rss[s["name"]].append(rss_kb / 1024.0)

        # 3. Interleaved headless 4-tool-call loop against /v1/responses
        for i in range(args.runs):
            order = sides if i % 2 == 0 else sides[::-1]
            for s in order:
                marker = f"KQ7ZT{i + 1}X"
                prompt = f"BENCH:TOOLS4 n={i + 1} read the four files and summarize"
                rc, out, err, ms, rss_kb = run_timed([*s["tools4_base"], prompt], s["cwd"], s["env"], timeout=90)
                if rc == 0 and marker in out:
                    t4_wall[s["name"]].append(ms)
                    if rss_kb is not None:
                        t4_rss[s["name"]].append(rss_kb / 1024.0)
                else:
                    t4_err[s["name"]] += 1

        # 4. 9,000-spill startup latency (ka)
        spill_env, spill_cwd, _ = setup_side(work, "ka-spills", port)
        if args.spills > 0:
            make_spills(spill_env["XDG_STATE_HOME"], args.spills)
        spill_cmd = [
            ka_path, "run", "say hi in three words",
            "--offline", "--ephemeral", "--ignore-user-config", "--ignore-rules", "--no-discovery",
        ]
        for _ in range(args.warmups):
            run_wall_only(spill_cmd, spill_cwd, spill_env)
        spills_wall = []
        spills_rss = []
        spills_err = 0
        for _ in range(args.runs):
            rc, out, _, ms, rss_kb = run_timed(spill_cmd, spill_cwd, spill_env, timeout=60)
            if rc == 0 and "heard:" in out:
                spills_wall.append(ms)
                if rss_kb is not None:
                    spills_rss.append(rss_kb / 1024.0)
            else:
                spills_err += 1

        report = {
            "schema_version": 1,
            "timestamp_utc": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "host": {
                "os": platform.system(),
                "kernel": platform.release(),
                "arch": platform.machine(),
                "cpu_model": cpu_model(),
                "cpu_count": os.cpu_count(),
                "python": platform.python_version(),
            },
            "config": {
                "runs": args.runs,
                "warmups": args.warmups,
                "spills": args.spills,
                "interleaved": True,
                "wire": "openai_responses",
            },
            "binaries": {
                s["name"]: binary_info(s["path"], s["flags_doc"]) for s in sides
            },
            "metrics": {
                "version_wall_ms": {
                    s["name"]: {**summarize(ver_wall[s["name"]]), "errors": ver_err[s["name"]]}
                    for s in sides
                },
                "version_rss_mib": {
                    s["name"]: summarize(ver_rss[s["name"]])
                    for s in sides
                },
                "tools4_wall_ms": {
                    s["name"]: {**summarize(t4_wall[s["name"]]), "errors": t4_err[s["name"]]}
                    for s in sides
                },
                "tools4_rss_mib": {
                    s["name"]: summarize(t4_rss[s["name"]])
                    for s in sides
                },
                "spills_wall_ms": {
                    "ka": {**summarize(spills_wall), "errors": spills_err},
                },
                "spills_rss_mib": {
                    "ka": summarize(spills_rss),
                },
            },
        }

        md = render_markdown(report)
        print(md)
        if args.out_md:
            os.makedirs(os.path.dirname(os.path.abspath(args.out_md)), exist_ok=True)
            with open(args.out_md, "w") as f:
                f.write(md)
        if args.out_json:
            os.makedirs(os.path.dirname(os.path.abspath(args.out_json)), exist_ok=True)
            with open(args.out_json, "w") as f:
                json.dump(report, f, indent=2)
        total_errors = sum(
            m.get("errors", 0) for m in report.get("metrics", {}).values()
        )
        if total_errors:
            print(
                f"compare_overhead: {total_errors} timed trial(s) failed — "
                "`n/a` rows are failures, not free runs",
                file=sys.stderr,
            )
            return 3
        return 0
    finally:
        if server:
            server.terminate()
            try:
                server.wait(3)
            except subprocess.TimeoutExpired:
                server.kill()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
