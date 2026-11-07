#!/usr/bin/env python3
"""Session benchmark for the `ka` TUI and headless surfaces.

Drives a release binary against the deterministic mock server
(mock_server.py) through a pty and measures the metrics that once
regressed against other agents:

  ready_ms          TUI spawn -> first frame on the pty
  p_tools4_ms       `ka run`, one prompt with 4 read tool calls (wall)
  sess5_cpu_ms      CPU for a 5-prompt interactive session
  sess5_peak_mb     peak RSS over that session
  long_cpu_p25/p75  per-prompt CPU at prompts 25/75 of a 75-prompt run
  long_wall_p75     per-prompt wall at prompt 75
  long_own_mb       private memory (smaps) after prompt 75
  write200k_ms      one write tool call carrying 200 KB
  stream60k_cpu_ms  CPU to stream a 60_000-char answer in the TUI
  idle_cpu_pct      CPU while the TUI sits idle (livelock guard)
  tty_writes_turn   write() syscalls to the tty for one streamed answer
                    (strace, skipped where unavailable)

Completion detection: the mock appends KQ7ZT<n>X to each final answer,
but ratatui emits minimal cell diffs — a marker differing from the
previous one by a digit renders as ONE changed cell, so byte-grepping
the pty stream misses it. Markers are matched against a virtual screen
model instead (Screen below).

Everything is isolated (KA_DATA_DIR/XDG_CONFIG_HOME/HOME/TMPDIR under a
temp dir); no real providers, keys, or user config are touched.

Usage: session_bench.py --ka <path-to-ka> [--out results.json]
Exit codes: 0 ok · 2 bad invocation · 3 a scenario failed.
"""
import argparse
import fcntl
import json
import os
import pty
import re
import select
import shutil
import signal
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import time

HERE = os.path.dirname(os.path.abspath(__file__))
TICK = os.sysconf("SC_CLK_TCK")


class Screen:
    """Minimal virtual terminal: absolute cursor moves + cell text."""

    def __init__(self, rows=50, cols=130):
        self.rows, self.cols = rows, cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.r = self.c = 0

    def feed(self, data):
        i, n = 0, len(data)
        while i < n:
            b = data[i]
            if b == 0x1B and i + 1 < n:
                if data[i + 1:i + 2] == b"[":
                    m = re.match(rb"\x1b\[([0-9;?]*)([A-Za-z])", data[i:])
                    if m:
                        self._csi(m.group(1), m.group(2))
                        i += m.end()
                        continue
                    i += 1
                    continue
                if data[i + 1:i + 2] == b"]":  # OSC ... BEL
                    end = data.find(b"\x07", i)
                    i = n if end < 0 else end + 1
                    continue
                i += 2
                continue
            if b == 0x0D:
                self.c = 0
            elif b == 0x0A:
                self.r = min(self.r + 1, self.rows - 1)
            elif 0x20 <= b < 0x7F:
                if self.r < self.rows and self.c < self.cols:
                    self.grid[self.r][self.c] = chr(b)
                self.c = min(self.c + 1, self.cols - 1)
            i += 1

    def _csi(self, params, cmd):
        p = params.decode(errors="replace")
        if cmd == b"H":
            parts = p.split(";") if p else []
            self.r = min(max(0, (int(parts[0]) - 1) if parts and parts[0] else 0),
                         self.rows - 1)
            self.c = min(max(0, (int(parts[1]) - 1) if len(parts) > 1 and parts[1] else 0),
                         self.cols - 1)
        elif cmd == b"K" and p in ("", "0"):
            for c in range(self.c, self.cols):
                self.grid[self.r][c] = " "
        elif cmd == b"J":
            self.grid = [[" "] * self.cols for _ in range(self.rows)]
            self.r = self.c = 0

    def contains(self, token):
        t = token if isinstance(token, str) else token.decode()
        lines = ["".join(row).rstrip() for row in self.grid]
        # join consecutive rows: a marker may wrap across the fold
        for a, b in zip(lines, lines[1:]):
            if t in (a + b):
                return True
        return any(t in line for line in lines)


def cpu_ns(pid):
    try:
        with open(f"/proc/{pid}/schedstat") as f:
            return int(f.read().split()[0])
    except OSError:
        try:
            with open(f"/proc/{pid}/stat") as f:
                parts = f.read().rsplit(")", 1)[1].split()
                return int((int(parts[11]) + int(parts[12])) * 1e9 / TICK)
        except OSError:
            return None


def mem_snapshot(pid):
    snap = {}
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith(("VmHWM", "VmRSS")):
                    k, v = line.split(":")
                    snap[k] = int(v.split()[0])  # kB
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for line in f:
                if line.startswith(("Pss:", "Private_Clean:", "Private_Dirty:")):
                    k, v = line.split(":")
                    snap[k] = int(v.split()[0])  # kB
    except OSError:
        pass
    return snap


class Bench:
    def __init__(self, ka, work):
        self.ka = os.path.abspath(ka)
        self.work = work
        self.port = self._free_port()
        self.server = None
        overlay = os.path.join(work, "dialects.toml")
        with open(overlay, "w") as f:
            f.write(f'[dialects."bench/mock"]\n'
                    f'wire = "openai_chat"\n'
                    f'base_url = "http://127.0.0.1:{self.port}/v1"\n'
                    f'context = 200000\n')
        self.overlay = overlay
        cfg = os.path.join(work, "config", "ka")
        os.makedirs(cfg, exist_ok=True)
        with open(os.path.join(cfg, "ka.toml"), "w") as f:
            f.write('model = "bench/mock"\nmode = "accept_edits"\n'
                    '\n[update]\ncheck = "off"\n')
        os.makedirs(os.path.join(work, "home"), exist_ok=True)
        os.makedirs(os.path.join(work, "tmp"), exist_ok=True)
        os.makedirs(os.path.join(work, "cwd"), exist_ok=True)
        for i in range(1, 5):
            with open(os.path.join(work, "cwd", f"f{i}.txt"), "w") as f:
                f.writelines(f"line {i}: filler content for file {i} {n}\n"
                             for n in range(1, 41))
        self.env = dict(os.environ, **{
            "KA_DATA_DIR": os.path.join(work, "data"),
            "XDG_CONFIG_HOME": os.path.join(work, "config"),
            "HOME": os.path.join(work, "home"),
            "TMPDIR": os.path.join(work, "tmp"),
            "TERM": "xterm-256color",
        })

    @staticmethod
    def _free_port():
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            return s.getsockname()[1]

    def start_server(self):
        self.server = subprocess.Popen(
            [sys.executable, os.path.join(HERE, "mock_server.py"), str(self.port)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            try:
                with socket.create_connection(("127.0.0.1", self.port), 0.2):
                    return
            except OSError:
                if self.server.poll() is not None:
                    raise RuntimeError("mock server died")
                time.sleep(0.05)
        raise RuntimeError("mock server never came up")

    def stop_server(self):
        if self.server:
            self.server.terminate()
            try:
                self.server.wait(3)
            except subprocess.TimeoutExpired:
                self.server.kill()

    # -- surfaces -------------------------------------------------------

    def headless(self, prompt):
        argv = [self.ka, "run", "--dialects", self.overlay,
                "--no-discovery", "--print", "ndjson", prompt]
        t0 = time.monotonic()
        proc = subprocess.Popen(argv, cwd=os.path.join(self.work, "cwd"),
                                env=self.env, stdout=subprocess.DEVNULL,
                                stderr=subprocess.PIPE)
        err = proc.stderr.read().decode()
        _, status, ru = os.wait4(proc.pid, 0)
        if status != 0:
            raise RuntimeError(f"ka run failed: {err}")
        return time.monotonic() - t0, ru.ru_utime + ru.ru_stime

    def tui(self):
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(os.path.join(self.work, "cwd"))
            os.execve(self.ka, [self.ka, "--dialects", self.overlay,
                                "--no-discovery"], self.env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        return pid, fd

    @staticmethod
    def drain(fd, t, screen, first_byte=None):
        end = time.monotonic() + t
        while time.monotonic() < end:
            r, _, _ = select.select([fd], [], [], 0.02)
            if r:
                data = os.read(fd, 262144)
                if not data:
                    break
                screen.feed(data)
                if first_byte is not None and first_byte[0] is None:
                    first_byte[0] = time.monotonic()
        return screen

    @staticmethod
    def quit_tui(pid, fd):
        try:
            os.write(fd, b"/quit\r")
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                r, _, _ = select.select([fd], [], [], 0.1)
                if r and not os.read(fd, 65536):
                    break
            os.kill(pid, signal.SIGTERM)
        except OSError:
            pass
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:
            pass
        os.close(fd)

    def wait_ready(self, pid, fd, timeout=10.0):
        screen = Screen()
        first = [None]
        t0 = time.monotonic()
        deadline = t0 + timeout
        while time.monotonic() < deadline:
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                data = os.read(fd, 65536)
                if not data:
                    break
                screen.feed(data)
                if first[0] is None:
                    first[0] = time.monotonic()
                    return first[0] - t0
        return None

    def prompt(self, pid, fd, screen, text, marker, timeout=60.0):
        cpu0 = cpu_ns(pid)
        t0 = time.monotonic()
        os.write(fd, text.encode() + b"\r")
        deadline = t0 + timeout
        while time.monotonic() < deadline:
            r, _, _ = select.select([fd], [], [], 0.02)
            if r:
                data = os.read(fd, 262144)
                if not data:
                    break
                screen.feed(data)
                if screen.contains(marker):
                    wall = time.monotonic() - t0
                    time.sleep(0.15)  # let the turn settle before sampling cpu
                    return wall, cpu_ns(pid) - cpu0
        raise TimeoutError(f"marker {marker} not seen")


def run_all(ka, verbose=True):
    work = tempfile.mkdtemp(prefix="ka-bench-")
    b = Bench(ka, work)
    out = {}
    try:
        b.start_server()
        say = print if verbose else (lambda *a, **k: None)

        # ready to type
        times = []
        for _ in range(5):
            pid, fd = b.tui()
            t = b.wait_ready(pid, fd)
            b.quit_tui(pid, fd)
            if t is None:
                raise TimeoutError("TUI never became ready")
            times.append(t)
        out["ready_ms"] = round(statistics.median(times) * 1000, 1)
        say(f"ready to type: {out['ready_ms']} ms")

        # -p: one prompt, 4 tool calls (+warmup)
        b.headless("warmup BENCH:TOOLS4 n=0")
        runs = [b.headless(f"BENCH:TOOLS4 n={i} read the four files and summarize")
                for i in range(1, 6)]
        out["p_tools4_ms"] = round(statistics.median(r[0] for r in runs) * 1000, 1)
        say(f"-p tools4: {out['p_tools4_ms']} ms")

        # interactive session: 5 prompts
        pid, fd = b.tui()
        b.wait_ready(pid, fd)
        screen = Screen()
        cpu0 = cpu_ns(pid)
        for i in range(1, 6):
            b.prompt(pid, fd, screen, f"BENCH:SESS n={i} analyze the build output",
                     f"KQ7ZT{i}X")
        out["sess5_cpu_ms"] = round((cpu_ns(pid) - cpu0) / 1e6, 1)
        out["sess5_peak_mb"] = round(mem_snapshot(pid).get("VmHWM", 0) / 1024, 1)
        b.quit_tui(pid, fd)
        say(f"session 5 prompts: {out['sess5_cpu_ms']} ms cpu, "
            f"{out['sess5_peak_mb']} MB peak")

        # long session: 75 prompts
        pid, fd = b.tui()
        b.wait_ready(pid, fd)
        screen = Screen()
        walls, cpus = {}, {}
        for i in range(1, 76):
            w, c = b.prompt(pid, fd, screen,
                            f"BENCH:LONG n={i} keep going on the refactor",
                            f"KQ7ZT{i}X")
            walls[i] = w
            cpus[i] = c
        snap = mem_snapshot(pid)
        b.quit_tui(pid, fd)

        def med_around(d, i):
            return statistics.median(d[j] for j in range(i - 2, min(i + 3, 76)))

        out["long_cpu_p25_ms"] = round(med_around(cpus, 25) / 1e6, 1)
        out["long_cpu_p75_ms"] = round(med_around(cpus, 75) / 1e6, 1)
        out["long_wall_p75_ms"] = round(med_around(walls, 75) * 1000, 1)
        priv = snap.get("Private_Clean", 0) + snap.get("Private_Dirty", 0)
        out["long_own_mb"] = round(priv / 1024, 1)
        say(f"long session: cpu p25 {out['long_cpu_p25_ms']} / "
            f"p75 {out['long_cpu_p75_ms']} ms, wall p75 "
            f"{out['long_wall_p75_ms']} ms, own {out['long_own_mb']} MB")

        # 200 KB write through a tool call (+warmup)
        b.headless("warmup BENCH:WRITE200K n=0 write the big file")
        runs = [b.headless(f"BENCH:WRITE200K n={i} write the big file")
                for i in range(1, 4)]
        out["write200k_ms"] = round(statistics.median(r[0] for r in runs) * 1000, 1)
        say(f"write 200KB via tool: {out['write200k_ms']} ms")

        # streaming a 60_000-char answer (rendering included)
        pid, fd = b.tui()
        b.wait_ready(pid, fd)
        screen = Screen()
        _, c = b.prompt(pid, fd, screen, "BENCH:STREAM60K n=1 tell me everything",
                        "KQ7ZT1X", timeout=120)
        out["stream60k_cpu_ms"] = round(c / 1e6, 1)
        b.quit_tui(pid, fd)
        say(f"stream 60k: {out['stream60k_cpu_ms']} ms cpu")

        # idle CPU (livelock guard)
        pid, fd = b.tui()
        b.wait_ready(pid, fd)
        time.sleep(0.5)
        c0 = cpu_ns(pid)
        time.sleep(3.0)
        c1 = cpu_ns(pid)
        out["idle_cpu_pct"] = round((c1 - c0) / 1e9 / 3.0 * 100, 2)
        b.quit_tui(pid, fd)
        say(f"idle cpu: {out['idle_cpu_pct']}%")

        # tty write() count per streamed answer (needs strace; optional)
        out["tty_writes_turn"] = None
        if shutil.which("strace"):
            pid, fd = b.tui()
            b.wait_ready(pid, fd)
            time.sleep(0.3)
            trace = os.path.join(work, "writes.trace")
            tr = subprocess.Popen(["strace", "-f", "-e", "trace=write",
                                   "-o", trace, "-p", str(pid)],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            time.sleep(0.4)
            try:
                screen = Screen()
                b.prompt(pid, fd, screen, "BENCH:SESS n=9 syscall count",
                         "KQ7ZT9X")
            finally:
                tr.send_signal(signal.SIGTERM)
                tr.wait()
            b.quit_tui(pid, fd)
            n = 0
            try:
                with open(trace, errors="replace") as f:
                    for line in f:
                        m = re.match(r"\d+ write\(1,", line)
                        if m and "EPERM" not in line and "ESRCH" not in line:
                            n += 1
                out["tty_writes_turn"] = n
            except OSError:
                pass
            say(f"tty writes per turn: {out['tty_writes_turn']}")
        else:
            say("tty writes per turn: skipped (no strace)")

        out["host"] = subprocess.run(["uname", "-sm"], capture_output=True,
                                     text=True).stdout.strip()
        return out
    finally:
        b.stop_server()
        shutil.rmtree(work, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ka", required=True, help="path to the ka binary")
    ap.add_argument("--out", help="write results JSON here")
    args = ap.parse_args()
    if not os.path.exists(args.ka):
        print(f"session_bench: no such binary: {args.ka}", file=sys.stderr)
        return 2
    try:
        results = run_all(args.ka)
    except (TimeoutError, RuntimeError) as e:
        print(f"session_bench: FAILED: {e}", file=sys.stderr)
        return 3
    print(json.dumps(results, indent=2))
    if args.out:
        with open(args.out, "w") as f:
            json.dump(results, f, indent=2)
    return 0


if __name__ == "__main__":
    sys.exit(main())
