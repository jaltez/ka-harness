#!/usr/bin/env python3
"""Drive ka on a real pty at a given size and keep what the screen showed.

One step per argument after `--`: `wait:<regex>`, `type:<text>`,
`key:<name>` (Enter, Escape, Tab, Backspace, Up, Down, Left, Right,
Home, End, PageUp, PageDown, F1..F12, or `ctrl+<letter>`), `sleep:<s>`,
`shot:<label>`, `paste:<text>`, `resize:<cols>x<rows>`. Each `shot`
writes three files: `<label>.txt` (the screen as text), `<label>.ansi`
(raw bytes since the previous shot) and `<label>.png` (the screen with
its colours, when Pillow and a monospace font are present).

`--term` and `--colorterm` set what the terminal claims about colour:
`--colorterm truecolor` is a desktop terminal, `--colorterm none` is a
client that never announces more than its TERM. The screen model applies
what ka sends regardless, and the report counts the colours on screen
by the depth ka wrote them at, so a frame shows what ka emits.

usage: pty_shot.py --bin PATH --cols 46 --rows 14 --out DIR --name phone \\
             [--cwd DIR] [--env K=V ...] [--term xterm-256color]
             [--colorterm truecolor|none] [--ka-args "--flags for ka"] -- <step> [step ...]
"""
import argparse
import fcntl
import os
import pty
import re
import select
import struct
import termios
import time
import unicodedata

CELL_W, CELL_H = 9, 18
KEYS = {
    "Enter": b"\r", "Escape": b"\x1b", "Tab": b"\t", "Backspace": b"\x7f",
    "Up": b"\x1b[A", "Down": b"\x1b[B", "Right": b"\x1b[C", "Left": b"\x1b[D",
    "Home": b"\x1b[H", "End": b"\x1b[F", "PageUp": b"\x1b[5~", "PageDown": b"\x1b[6~",
    "Space": b" ", "Delete": b"\x1b[3~", "Insert": b"\x1b[2~",
}
for _i in range(1, 13):
    KEYS[f"F{_i}"] = f"\x1b[{_i + 11}~".encode()

# xterm's sixteen colours, for frames that have no true colour.
ANSI16 = [(0, 0, 0), (205, 0, 0), (0, 205, 0), (205, 205, 0), (0, 0, 238), (205, 0, 205),
          (0, 205, 205), (229, 229, 229), (127, 127, 127), (255, 0, 0), (0, 255, 0),
          (255, 255, 0), (92, 92, 255), (255, 0, 255), (0, 255, 255), (255, 255, 255)]


def key_bytes(name):
    if name in KEYS:
        return KEYS[name]
    if name.startswith("ctrl+"):
        return bytes([ord(name[5]) & 0x1f])
    if name.startswith("alt+"):
        return b"\x1b" + name[4:].encode()
    raise SystemExit(f"unknown key: {name}")


def resolve(col):
    """A screen colour as RGB, whatever depth it was written at."""
    if not col:
        return None
    depth, value = col
    if depth == "rgb":
        return value
    if depth == "16":
        return ANSI16[value % 16]
    if value < 16:
        return ANSI16[value]
    if value >= 232:
        g = 8 + (value - 232) * 10
        return (g, g, g)
    levels = [0, 95, 135, 175, 215, 255]
    return (levels[(value - 16) // 36], levels[((value - 16) // 6) % 6], levels[(value - 16) % 6])


class Cell:
    __slots__ = ("ch", "fg", "bg", "bold", "dim", "italic", "underline", "reverse")

    def __init__(self):
        self.ch = " "
        self.fg = None
        self.bg = None
        self.bold = self.dim = self.italic = self.underline = self.reverse = False


class Screen:
    """Enough of a terminal to replay ratatui's output: cursor moves,
    erases, UTF-8 text with wide characters, and the SGR state a
    written cell carries. Bytes it cannot make sense of yet are kept
    for the next read."""

    def __init__(self, cols, rows):
        self.resize(cols, rows)
        self.held = b""
        self.counts = {"rgb": 0, "256": 0, "16": 0}

    def resize(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.grid = [[Cell() for _ in range(cols)] for _ in range(rows)]
        self.x = self.y = 0
        self.fg = self.bg = None
        self.bold = self.dim = self.italic = self.underline = self.reverse = False

    def erase(self, cell):
        cell.__init__()
        cell.fg, cell.bg = self.fg, self.bg

    def put(self, ch):
        if self.y >= self.rows or self.x >= self.cols:
            return
        wide = unicodedata.east_asian_width(ch) in ("W", "F")
        cell = self.grid[self.y][self.x]
        cell.ch = ch
        cell.fg, cell.bg = self.fg, self.bg
        cell.bold, cell.dim, cell.italic = self.bold, self.dim, self.italic
        cell.underline, cell.reverse = self.underline, self.reverse
        self.x += 1
        if wide and self.x < self.cols:
            # the terminal gives a wide glyph the next cell too
            self.grid[self.y][self.x].ch = ""
            self.x += 1

    def feed(self, data):
        """Apply `data`; keep any trailing incomplete sequence."""
        data = self.held + data
        i, n = 0, len(data)
        while i < n:
            b = data[i]
            if b == 0x1b:
                j = self.escape(data, i)
                if j is None:
                    break
                i = j
            elif b >= 0x80:
                width = 4 if b >= 0xf0 else 3 if b >= 0xe0 else 2 if b >= 0xc2 else 0
                if width == 0:
                    i += 1
                    continue
                if i + width > n:
                    break
                try:
                    self.put(data[i:i + width].decode("utf-8"))
                except UnicodeDecodeError:
                    pass
                i += width
            else:
                if b == 0x0a:
                    self.y += 1
                    self.x = 0
                elif b == 0x0d:
                    self.x = 0
                elif b == 0x08:
                    self.x = max(0, self.x - 1)
                elif b == 0x09:
                    self.x = min(self.cols - 1, (self.x // 8 + 1) * 8)
                elif b >= 0x20:
                    self.put(chr(b))
                i += 1
        self.held = data[i:]
        self.y = min(self.y, self.rows - 1)
        self.x = min(self.x, self.cols - 1)

    def escape(self, data, i):
        """Handle the escape sequence at `i`; return the index after it,
        or None when the sequence is incomplete."""
        if i + 1 >= len(data):
            return None
        kind = data[i + 1]
        if kind in b"()#":
            return i + 3 if i + 2 < len(data) else None
        if kind in b"78=><MDEHc":
            return i + 2
        if kind in b"]^_PX":
            ends = [e for e in (data.find(b"\x07", i + 2), data.find(b"\x1b\\", i + 2)) if e != -1]
            if not ends and len(data) - i < 4096:
                return None
            first = min(ends) if ends else len(data) - 1
            return first + (1 if data[first] == 0x07 else 2)
        if kind != 0x5b:
            return i + 2
        m = re.match(rb"\x1b\[([0-9;?<>= ]*)([ -/]*)([@-~])", data[i:])
        if not m:
            return None if len(data) - i < 32 else i + 2
        params, intermediates, final = m.groups()
        if params[:1] in (b"?", b">", b"=") or intermediates:
            return i + m.end()
        nums = [int(p) if p else 0 for p in params.split(b";")] if params else [0]
        one = max(nums[0], 1)
        if final in (b"H", b"f"):
            self.y = min(max((nums[0] or 1) - 1, 0), self.rows - 1)
            self.x = min(max(((nums[1] if len(nums) > 1 else 1) or 1) - 1, 0), self.cols - 1)
        elif final == b"A":
            self.y = max(0, self.y - one)
        elif final == b"B":
            self.y = min(self.rows - 1, self.y + one)
        elif final == b"C":
            self.x = min(self.cols - 1, self.x + one)
        elif final == b"D":
            self.x = max(0, self.x - one)
        elif final == b"G":
            self.x = min(self.cols - 1, max((nums[0] or 1) - 1, 0))
        elif final == b"J" and nums[0] == 2:
            for row in self.grid:
                for cell in row:
                    self.erase(cell)
        elif final == b"K":
            for cell in self.grid[self.y][self.x:]:
                self.erase(cell)
        elif final == b"m":
            self.sgr(nums)
        return i + m.end()

    def sgr(self, nums):
        k = 0
        while k < len(nums):
            n = nums[k]
            if n == 0:
                self.fg = self.bg = None
                self.bold = self.dim = self.italic = self.underline = self.reverse = False
            elif n == 1:
                self.bold = True
            elif n == 2:
                self.dim = True
            elif n == 3:
                self.italic = True
            elif n == 4:
                self.underline = True
            elif n in (22, 23):
                self.bold = self.dim = False
            elif n == 27:
                self.reverse = True
            elif 30 <= n <= 37 or 90 <= n <= 97:
                self.fg, self.counts["16"] = ("16", n - 30 if n < 90 else n - 82), self.counts["16"] + 1
            elif 40 <= n <= 47 or 100 <= n <= 107:
                self.bg = ("16", n - 40 if n < 100 else n - 92)
            elif n in (38, 48) and k + 1 < len(nums):
                if nums[k + 1] == 5 and k + 2 < len(nums):
                    setattr(self, "fg" if n == 38 else "bg", ("256", nums[k + 2]))
                    self.counts["256"] += 1
                    k += 2
                elif nums[k + 1] == 2 and k + 4 < len(nums):
                    setattr(self, "fg" if n == 38 else "bg", ("rgb", tuple(nums[k + 2:k + 5])))
                    self.counts["rgb"] += 1
                    k += 4
            elif n == 39:
                self.fg = None
            elif n == 49:
                self.bg = None
            k += 1

    def text(self):
        return "\n".join("".join(c.ch for c in row).rstrip() for row in self.grid)

    def palette(self):
        """Distinct colours on screen by the depth ka wrote them at."""
        seen = {"rgb": set(), "256": set(), "16": set()}
        for row in self.grid:
            for c in row:
                for col in (c.fg, c.bg):
                    if col:
                        seen[col[0]].add(col[1])
        return {k: len(v) for k, v in seen.items()}

    def png(self, path, font_path):
        from PIL import Image, ImageDraw, ImageFont
        font = ImageFont.truetype(font_path, 14)
        ground = (10, 10, 14)
        img = Image.new("RGB", (self.cols * CELL_W, self.rows * CELL_H), ground)
        draw = ImageDraw.Draw(img)
        for y, row in enumerate(self.grid):
            for x, cell in enumerate(row):
                fg, bg = resolve(cell.fg), resolve(cell.bg)
                if cell.reverse:
                    fg, bg = bg, fg
                if cell.dim and fg:
                    fg = tuple((a + b) // 2 for a, b in zip(fg, bg or ground))
                px, py = x * CELL_W, y * CELL_H
                if bg:
                    draw.rectangle([px, py, px + CELL_W - 1, py + CELL_H - 1], fill=bg)
                if cell.ch and cell.ch != " ":
                    draw.text((px, py), cell.ch, fill=fg or (220, 220, 220), font=font)
                if cell.underline:
                    draw.line([px, py + CELL_H - 2, px + CELL_W - 1, py + CELL_H - 2],
                              fill=fg or (220, 220, 220))
        img.save(path)


def find_font():
    for root in ("/usr/share/fonts/truetype/dejavu", "/usr/share/fonts/truetype/liberation",
                 "/usr/share/fonts/truetype/noto", "/usr/share/fonts/truetype/freefont"):
        for name in ("DejaVuSansMono.ttf", "LiberationMono-Regular.ttf", "NotoSansMono-Regular.ttf",
                     "FreeMono.ttf"):
            path = os.path.join(root, name)
            if os.path.exists(path):
                return path
    return None


def run(args):
    os.makedirs(args.out, exist_ok=True)
    args.bin = os.path.abspath(args.bin)
    if not os.access(args.bin, os.X_OK):
        raise SystemExit(f"pty_shot: not an executable: {args.bin}")
    home = os.path.join(args.out, "_home")
    env = {
        "HOME": home,
        "XDG_CONFIG_HOME": os.path.join(home, ".config"),
        "XDG_STATE_HOME": os.path.join(home, ".local", "state"),
        "XDG_DATA_HOME": os.path.join(home, ".local", "share"),
        "KA_DATA_DIR": os.path.join(home, ".local", "share", "ka"),
        "TMPDIR": os.path.join(home, "tmp"),
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "TERM": args.term,
    }
    for d in (env["HOME"], env["XDG_CONFIG_HOME"], env["XDG_STATE_HOME"], env["XDG_DATA_HOME"], env["TMPDIR"]):
        os.makedirs(d, exist_ok=True)
    if args.colorterm and args.colorterm != "none":
        env["COLORTERM"] = args.colorterm
    for kv in args.env:
        k, _, v = kv.partition("=")
        env[k] = v
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(args.cwd)
        os.execve(args.bin, [args.bin, *args.ka_args.split()], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", args.rows, args.cols, 0, 0))
    screen = Screen(args.cols, args.rows)
    font = find_font()
    raw = bytearray()
    report = []
    state = {"produced": False}

    def pump(timeout):
        if not select.select([fd], [], [], timeout)[0]:
            return False
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            return False
        if not chunk:
            return False
        state["produced"] = True
        raw.extend(chunk)
        screen.feed(chunk)
        return True

    def settle(seconds=0.5):
        quiet = time.monotonic() + seconds
        while time.monotonic() < quiet:
            if pump(0.1):
                quiet = min(quiet, time.monotonic() + 0.3)

    def shot(label):
        settle()
        base = os.path.join(args.out, f"{args.name}-{label}")
        with open(base + ".txt", "w") as f:
            f.write(screen.text() + "\n")
        with open(base + ".ansi", "wb") as f:
            f.write(bytes(raw))
        raw.clear()
        if font:
            screen.png(base + ".png", font)
        report.append((label, screen.palette(), dict(screen.counts)))

    try:
        for step in args.steps:
            kind, _, arg = step.partition(":")
            if kind == "wait":
                deadline = time.monotonic() + 20
                while time.monotonic() < deadline:
                    pump(0.2)
                    if re.search(arg.encode(), bytes(raw)):
                        break
            elif kind == "sleep":
                end = time.monotonic() + float(arg)
                while time.monotonic() < end:
                    pump(min(0.1, max(0.0, end - time.monotonic())))
            elif kind == "type":
                os.write(fd, arg.encode())
            elif kind == "paste":
                os.write(fd, b"\x1b[200~" + arg.encode() + b"\x1b[201~")
            elif kind == "key":
                os.write(fd, key_bytes(arg))
            elif kind == "resize":
                cols, rows = (int(v) for v in arg.split("x"))
                fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
                screen.resize(cols, rows)
            elif kind == "shot":
                shot(arg)
            else:
                raise SystemExit(f"unknown step: {step}")
    finally:
        try:
            os.kill(pid, 9)
        except OSError:
            pass
        os.waitpid(pid, 0)
        os.close(fd)
    if not state["produced"]:
        raise SystemExit(f"pty_shot: {args.bin} wrote nothing: check the path and the flags")
    for label, pal, counts in report:
        print(f"{label}: on screen {pal}, SGR so far {counts}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--cols", type=int, required=True)
    ap.add_argument("--rows", type=int, required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--name", required=True)
    ap.add_argument("--cwd", default="/tmp")
    ap.add_argument("--env", action="append", default=[])
    ap.add_argument("--term", default="xterm-256color")
    ap.add_argument("--colorterm", default="truecolor")
    ap.add_argument("--ka-args", default="", help="ka's own flags, as one quoted string")
    ap.add_argument("steps", nargs="*")
    args = ap.parse_args()
    run(args)


if __name__ == "__main__":
    main()
