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

        # 10. Narrow terminal column truncation (ASCII + CJK multibyte).
        os.write(fd, b"\x05\x15")
        os.write(fd, f"enable -f {so_path} bash_autosuggest && bash_autosuggest\n".encode())
        read_until_idle(fd, idle=0.2)
        set_winsize(fd, 24, 14)
        os.write(fd, b"ap")
        out, _ = read_until_idle(fd, idle=0.2)
        # '$ ' (2) + 'ap' (2) = col 4; cols=14 -> avail=9 -> 't upgrade'
        assert b"\x1b[38;5;242mt upgrade\x1b[0m" in out, f"Narrow truncation failed: {out!r}"
        print("[PASS] Narrow terminal ASCII column truncation")

        os.write(fd, b"\x05\x15")
        read_until_idle(fd, idle=0.15)
        set_winsize(fd, 24, 20)
        os.write(fd, "echo 中文测试".encode("utf-8"))
        out, _ = read_until_idle(fd, idle=0.2)
        # '$ ' (2) + 'echo 中文测试' (5 + 8 = 13) = 15 cols (19 UTF-8 bytes); cols=20 -> avail=4 -> '内容'
        assert "\x1b[38;5;242m内容\x1b[0m".encode("utf-8") in out, (
            f"Narrow CJK column truncation failed: {out!r}"
        )
        print("[PASS] Narrow terminal CJK column truncation")

        # 11. Rapid double Alt+f accepts two distinct words without duplicating.
        os.write(fd, b"\x05\x15")
        read_until_idle(fd, idle=0.15)
        set_winsize(fd, 24, 80)
        os.write(fd, b"agy")
        read_until_idle(fd, idle=0.15)
        os.write(fd, b"\x1bf\x1bf")
        out, _ = read_until_idle(fd, idle=0.2)
        assert b" whoami" in out and b" --verbose" in out, f"Rapid double Alt+f failed: {out!r}"
        assert b" whoami whoami" not in out, f"Rapid double Alt+f duplicated word: {out!r}"
        print("[PASS] Rapid double Alt+f accepts consecutive words cleanly")
    finally:
        cleanup(pid, fd, hist_path)


def test_vi_mode_and_style(so_path: str) -> None:
    history = ["echo hello_vi_mode", "git status --short"]
    pid, fd, hist_path = spawn_bash(so_path, history, cols=80)
    try:
        # Custom BASH_AUTOSUGGEST_STYLE + Vi editing mode.
        os.write(
            fd,
            b"set -o vi; export BASH_AUTOSUGGEST_STYLE='90'; bash_autosuggest; bash_autosuggest status\n",
        )
        out, _ = read_until_idle(fd, idle=0.25)
        assert b'style="90"' in out, f"Custom style not picked up: {out!r}"

        # Ghost text renders with custom style in vi insert mode and accepts via Right Arrow.
        os.write(fd, b"echo h")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b"\x1b[90mello_vi_mode\x1b[0m" in out, f"Vi insert ghost failed: {out!r}"

        # Switching to vi command mode via ESC clears ghost text cleanly (after keyseq-timeout).
        os.write(fd, b"\x1b")
        out, _ = read_until_idle(fd, idle=0.7)
        assert b"\x1b[K" in out and b"\x1b[90m" not in out, (
            f"Vi ESC to command mode did not clear ghost: {out!r}"
        )

        # Back to insert mode on a new line and accept full suggestion with Right Arrow.
        os.write(fd, b"\x03")
        read_until_idle(fd, idle=0.15)
        os.write(fd, b"git s")
        read_until_idle(fd, idle=0.15)
        os.write(fd, b"\x1b[C")
        out, _ = read_until_idle(fd, idle=0.2)
        assert b"tatus --short" in out, f"Vi Right Arrow accept failed: {out!r}"
        print("[PASS] Vi mode & custom BASH_AUTOSUGGEST_STYLE")
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


def test_syntax_highlighting(so_path: str) -> None:
    history = ["echo hello world", "definitely_not_a_cmd arg"]
    pid, fd, hist_path = spawn_bash(so_path, history, cols=80)
    try:
        # 1. Valid builtin ('echo'), flag ('-n'), string ('"hi $USER"'), pipe, comment.
        os.write(fd, b'echo -n "hi $USER" | cat # note')
        out, _ = read_until_idle(fd, idle=0.2)
        assert b"\x1b[32mecho\x1b[0m" in out, f"Builtin 'echo' not green: {out!r}"
        assert b"\x1b[36m-n\x1b[0m" in out, f"Option '-n' not cyan: {out!r}"
        assert b'\x1b[33m"hi \x1b[0m\x1b[36m$USER\x1b[0m\x1b[33m"\x1b[0m' in out, (
            f"String/variable highlighting failed: {out!r}"
        )
        assert b"\x1b[35m|\x1b[0m" in out, f"Pipe operator not magenta: {out!r}"
        assert b"\x1b[32mcat\x1b[0m" in out, f"External PATH command 'cat' not green: {out!r}"
        assert b"\x1b[90m# note\x1b[0m" in out, f"Comment not dim gray: {out!r}"
        print("[PASS] Syntax highlighting: builtin, PATH cmd, option, string, var, pipe, comment")

        # Clear line.
        os.write(fd, b"\x05\x15")
        read_until_idle(fd, idle=0.15)

        # 2. Invalid command highlighted red, existing path underlined.
        os.write(fd, f"definitely_not_a_cmd {hist_path}".encode())
        out, _ = read_until_idle(fd, idle=0.2)
        assert b"\x1b[31mdefinitely_not_a_cmd\x1b[0m" in out, (
            f"Invalid command not red: {out!r}"
        )
        assert f"\x1b[4m{hist_path}\x1b[0m".encode() in out, (
            f"Existing path not underlined: {out!r}"
        )
        print("[PASS] Syntax highlighting: invalid command (red) & existing path (underline)")

        # Clear line.
        os.write(fd, b"\x05\x15")
        read_until_idle(fd, idle=0.15)

        # 3. Runtime Bash alias & function detected in-process as valid (green).
        os.write(fd, b"alias my_test_alias='echo ok'; my_test_fn() { :; }\n")
        read_until_idle(fd, idle=0.2)

        os.write(fd, b"my_test_alias")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b"\x1b[32mmy_test_alias\x1b[0m" in out, f"Alias not green: {out!r}"

        os.write(fd, b"\x05\x15")
        read_until_idle(fd, idle=0.15)

        os.write(fd, b"my_test_fn")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b"\x1b[32mmy_test_fn\x1b[0m" in out, f"Function not green: {out!r}"
        print("[PASS] Syntax highlighting: live Bash aliases & shell functions")

        # Clear line.
        os.write(fd, b"\x05\x15")
        read_until_idle(fd, idle=0.15)

        # 4. Cursor in middle of line still highlights cleanly, and wrapped lines work.
        os.write(fd, b"echo foo")
        read_until_idle(fd, idle=0.15)
        os.write(fd, b"\x1b[D\x1b[D")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b"\x1b[32mecho\x1b[0m" in out, f"Mid-line cursor lost highlighting: {out!r}"
        print("[PASS] Syntax highlighting preserved when cursor is mid-line")

        os.write(fd, b"\x05\x15")
        read_until_idle(fd, idle=0.15)
        set_winsize(fd, 24, 14)
        os.write(fd, b"echo 1234567890")
        out, _ = read_until_idle(fd, idle=0.2)
        assert b"\x1b[32mecho\x1b[0m" in out and b"1234567890" in out, (
            f"Wrapped line highlighting failed: {out!r}"
        )
        print("[PASS] Syntax highlighting on wrapped multi-row lines")

        # 5. Runtime toggle: `bash_autosuggest highlight off` disables syntax colors while keeping ghost text.
        set_winsize(fd, 24, 80)
        os.write(fd, b"\x05\x15")
        os.write(fd, b"bash_autosuggest highlight off && bash_autosuggest status\n")
        out, _ = read_until_idle(fd, idle=0.25)
        assert b"highlight=off" in out, f"Expected highlight=off in status: {out!r}"

        os.write(fd, b"echo ")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b"\x1b[32mecho\x1b[0m" not in out, f"Highlight remained after highlight off: {out!r}"
        assert b"\x1b[38;5;242mhello world\x1b[0m" in out, (
            f"Ghost text should still work with highlight off: {out!r}"
        )

        # Re-enable highlight.
        os.write(fd, b"\x05\x15bash_autosuggest highlight on\n")
        read_until_idle(fd, idle=0.2)
        os.write(fd, b"echo ")
        out, _ = read_until_idle(fd, idle=0.15)
        assert b"\x1b[32mecho\x1b[0m" in out, f"Highlight did not resume after highlight on: {out!r}"
        print("[PASS] Runtime highlight off/on toggle preserves autosuggestions")
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
    test_vi_mode_and_style(so_path)
    test_dumb_term(so_path)
    test_syntax_highlighting(so_path)
    test_10k_history_stress(so_path)
    print("All PTY E2E tests passed!")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
