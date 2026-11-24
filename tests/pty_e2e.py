#!/usr/bin/env python3
"""Live PTY end-to-end tests for bash_autosuggest.so inside real /bin/bash."""

import fcntl
import os
import pty
import select
import struct
import sys
import tempfile
import termios
import time


def set_winsize(fd: int, rows: int, cols: int) -> None:
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def read_until_idle(fd: int, idle: float = 0.25, max_wait: float = 4.0) -> tuple[bytes, float]:
    buf = b""
    start = time.monotonic()
    last = start
    while time.monotonic() - start < max_wait:
        r, _, _ = select.select([fd], [], [], 0.03)
        if r:
            try:
                chunk = os.read(fd, 4096)
            except OSError:
                break
            if not chunk:
                break
            buf += chunk
            last = time.monotonic()
        elif time.monotonic() - last >= idle:
            break
    return buf, last - start


def spawn_bash(
    so_path: str, hist_lines: list[str], cols: int = 80, term: str = "xterm-256color"
) -> tuple[int, int, str]:
    hist_fd, hist_path = tempfile.mkstemp(prefix="bash_autosuggest_hist_")
    with os.fdopen(hist_fd, "w", encoding="utf-8") as f:
        for line in hist_lines:
            f.write(line + "\n")

    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = term
        os.environ["HISTFILE"] = hist_path
        os.environ["HISTSIZE"] = "20000"
        os.environ["HISTFILESIZE"] = "20000"
        os.environ["PS1"] = "$ "
        os.execvp("bash", ["bash", "--norc", "--noprofile", "-i"])

    set_winsize(fd, 24, cols)
    read_until_idle(fd, idle=0.3)

    setup = f"history -c; history -r {hist_path}; enable -f {so_path} bash_autosuggest && bash_autosuggest\n"
    os.write(fd, setup.encode())
    out, _ = read_until_idle(fd, idle=0.35)
    assert b"error" not in out.lower(), f"Failed to load builtin: {out!r}"
    return pid, fd, hist_path


def cleanup(pid: int, fd: int, hist_path: str) -> None:
    try:
        os.write(fd, b"\x03exit\n")
        time.sleep(0.1)
    except OSError:
        pass
    try:
        os.kill(pid, 9)
    except OSError:
        pass
    try:
        os.waitpid(pid, 0)
    except OSError:
        pass
    try:
        os.close(fd)
    except OSError:
        pass
    try:
        os.unlink(hist_path)
    except OSError:
        pass


def test_core_interactions(so_path: str) -> None:
    history = [
        "echo hello world",
        "apt upgrade -y",
        "agy whoami --verbose",
        "agy   ",  # trailing spaces must NOT create invisible ghost suggestions
        "echo 中文测试内容很长很长很长",
    ]

    pid, fd, hist_path = spawn_bash(so_path, history, cols=80)
    try:
        # 1. Typing 'a' suggests 'gy whoami --verbose' (skipping 'agy   ' trailing spaces after 'agy').
        os.write(fd, b"a")
        out, dt = read_until_idle(fd, idle=0.15)
        assert b"\x1b[38;5;242mgy\x1b[0m" in out, f"Missing ghost text on 'a': {out!r}"
        print(f"[PASS] ghost text on 'a' ({dt * 1000:.1f} ms)")

        # Accept 'gy' via Right Arrow -> now buffer is 'agy'; trailing-space entry 'agy   '
        # is skipped, so it immediately suggests ' whoami --verbose'!
        os.write(fd, b"\x1b[C")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b"gy" in out, f"Right Arrow did not insert 'gy': {out!r}"
        assert b"\x1b[38;5;242m whoami --verbose\x1b[0m" in out, (
            f"Trailing-space entry should be skipped in favor of ' whoami --verbose': {out!r}"
        )
        print("[PASS] Right Arrow accept + trailing-space history skipped")

        # 2. Alt+f accepts one word (' whoami'), then End accepts the rest (' --verbose').
        os.write(fd, b"\x1bf")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b" whoami" in out and b"\x1b[38;5;242m --verbose\x1b[0m" in out, (
            f"Alt+f word accept failed: {out!r}"
        )
        os.write(fd, b"\x1b[F")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b" --verbose" in out and b"\x1b[38;5;242m" not in out, (
            f"End key full accept failed: {out!r}"
        )
        print("[PASS] Alt+f partial accept + End key full accept")

        # Clear entire line.
        os.write(fd, b"\x05\x15")
        read_until_idle(fd, idle=0.15)

        # 3. Alt+Right partial word accept.
        os.write(fd, b"ap")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b"\x1b[38;5;242mt upgrade -y\x1b[0m" in out, f"Missing ghost on 'ap': {out!r}"
        os.write(fd, b"\x1b[1;3C")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b"t" in out and b"\x1b[38;5;242m upgrade -y\x1b[0m" in out, (
            f"Alt+Right failed: {out!r}"
        )
        print("[PASS] Alt+Right partial word accept")

        # 4. Left Arrow clears ghost cleanly.
        os.write(fd, b"\x1b[D")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b"\x1b[K" in out and b"\x1b[38;5;242m" not in out, (
            f"Left Arrow did not clear ghost: {out!r}"
        )
        print("[PASS] Left Arrow clears ghost cleanly")

        # Clear entire line.
        os.write(fd, b"\x05\x15")
        read_until_idle(fd, idle=0.15)

        # 5. Backspace updates and clears ghost cleanly.
        os.write(fd, b"ap")
        read_until_idle(fd, idle=0.15)
        os.write(fd, b"\x7f")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b"\x1b[38;5;242mgy\x1b[0m" in out, f"Ghost did not update on Backspace: {out!r}"
        os.write(fd, b"\x7f")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b"\x1b[K" in out and b"\x1b[38;5;242m" not in out, (
            f"Ghost remained on empty line: {out!r}"
        )
        print("[PASS] Backspace updates and clears ghost cleanly")

        # 6. Ctrl+R suppresses and clears ghost text.
        os.write(fd, b"a")
        read_until_idle(fd, idle=0.15)
        os.write(fd, b"\x12")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b"\x1b[K" in out and b"\x1b[38;5;242m" not in out, (
            f"Ctrl+R did not suppress ghost: {out!r}"
        )
        os.write(fd, b"\x07\x05\x15")
        read_until_idle(fd, idle=0.15)
        print("[PASS] Ctrl+R suppresses ghost cleanly")

        # 7. Tab clears ghost text cleanly.
        os.write(fd, b"echo")
        read_until_idle(fd, idle=0.15)
        os.write(fd, b"\t")
        out, _ = read_until_idle(fd, idle=0.2)
        assert b"\x1b[K" in out, f"Tab did not clear ghost: {out!r}"
        os.write(fd, b"\x05\x15")
        read_until_idle(fd, idle=0.15)
        print("[PASS] Tab clears ghost cleanly")

        # 8. Enter clears ghost text before executing.
        os.write(fd, b"echo")
        read_until_idle(fd, idle=0.15)
        os.write(fd, b"\r")
        out, _ = read_until_idle(fd, idle=0.2)
        assert b"\x1b[K" in out, f"Enter did not clear ghost: {out!r}"
        print("[PASS] Enter clears ghost before executing")

        # 9. Clean unload via enable -d.
        os.write(fd, b"bash_autosuggest status\n")
        out, _ = read_until_idle(fd, idle=0.2)
        assert b"bash_autosuggest: enabled" in out, f"Unexpected status: {out!r}"

        os.write(fd, b"enable -d bash_autosuggest\n")
        read_until_idle(fd, idle=0.2)
        os.write(fd, b"a")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b"\x1b[38;5;242m" not in out, f"Ghost appeared after unload: {out!r}"
        print("[PASS] Clean unload via 'enable -d'")

        # 10. Narrow terminal column truncation (including CJK).
        os.write(fd, b"\x05\x15")
        os.write(fd, f"enable -f {so_path} bash_autosuggest && bash_autosuggest\n".encode())
        read_until_idle(fd, idle=0.2)
        set_winsize(fd, 24, 14)
        os.write(fd, b"ap")
        out, _ = read_until_idle(fd, idle=0.2)
        # '$ ' (2) + 'ap' (2) = col 4; cols=14 -> avail=9 -> 't upgrade'
        assert b"\x1b[38;5;242mt upgrade\x1b[0m" in out, f"Narrow truncation failed: {out!r}"
        print("[PASS] Narrow terminal column truncation")
    finally:
        cleanup(pid, fd, hist_path)


def test_dumb_term(so_path: str) -> None:
    pid, fd, hist_path = spawn_bash(so_path, ["agy whoami"], term="dumb")
    try:
        os.write(fd, b"a")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b"\x1b[38;5;242m" not in out, f"Should not emit ANSI on TERM=dumb: {out!r}"
        print("[PASS] TERM=dumb safely skips ANSI ghost text")
    finally:
        cleanup(pid, fd, hist_path)


def test_10k_history_stress(so_path: str) -> None:
    history = ["agy target-at-very-beginning"] + [f"cmd_{i} --arg={i}" for i in range(10_000)]
    pid, fd, hist_path = spawn_bash(so_path, history, cols=80)
    try:
        os.write(fd, b"a")
        out, dt = read_until_idle(fd, idle=0.15)
        assert b"\x1b[38;5;242mgy target-at-very-beginning\x1b[0m" in out, (
            f"10k history scan failed: {out!r}"
        )
        assert dt < 0.05, f"10k history scan too slow: {dt * 1000:.1f} ms"
        print(f"[PASS] 10,000-entry worst-case history scan ({dt * 1000:.2f} ms)")
    finally:
        cleanup(pid, fd, hist_path)


def main() -> int:
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <path/to/libbash_autosuggest.so>", file=sys.stderr)
        return 2

    so_path = os.path.abspath(sys.argv[1])
    test_core_interactions(so_path)
    test_dumb_term(so_path)
    test_10k_history_stress(so_path)
    print("All PTY E2E tests passed!")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
