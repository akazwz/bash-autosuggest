# bash-autosuggest

Zero-lag, Fish/Zsh-style inline history autosuggestions for **native Bash** — implemented in Rust as a tiny, zero-dependency Bash loadable builtin (`.so`) that hooks GNU Readline directly.

- **Sub-millisecond latency**: zero heap allocations on the per-keystroke hot path, with byte-level prefix short-circuiting directly over Bash's in-memory `history_list()`. Scanning 10,000 history entries in the worst case takes **~0.09 ms**.
- **100% native Bash**: no PTY wrapper, no 256-byte `bind -x` hijacking, no shell switching. Native `Tab` completion, `bash-completion`, `Ctrl+R`, UTF-8/CJK editing, `tmux`, scripts, and AI coding agents (`claude`, `codex`, `agy`, etc.) work completely untouched.
- **Tiny & dependency-free**: ~340 KB stripped `.so`, **0 external Rust crates**, builds offline in < 0.5 s.

---

## How It Works

Existing approaches to Bash autosuggestions trade off either performance or compatibility:

```text
ble.sh          : bind -x hijacks all 256 bytes -> pure-Bash reimplementation of a line editor
PTY wrappers    : terminal <-> PTY proxy <-> bash -> extra layer (risks tmux / TUI quirks)
Zsh / Fish      : great UX, but not POSIX/Bash compatible for scripts & Bash-oriented tooling
```

`bash-autosuggest` uses Bash's official **`enable -f` loadable builtin** interface to hook GNU Readline inside the Bash process itself:

```text
Keyboard -> /bin/bash (native GNU Readline handles editing, Tab, Ctrl+R, UTF-8)
                |
                +-- rl_redisplay_function hook
                |     -> reads Bash's live in-memory history_list()
                |     -> byte-prefix short-circuit match (0 allocations)
                |     -> computes remaining screen columns via _rl_last_c_pos
                |     -> draws grey ghost suffix after cursor
                |
                +-- Right Arrow / End / Ctrl+F / Alt+Right / Alt+f (when at EOL)
                      -> inserts suffix natively via rl_insert_text()
```

### Comparison

| Aspect | `bash-autosuggest` | `ble.sh` | Zsh / Fish |
| :--- | :--- | :--- | :--- |
| **Bash compatibility** | **100% native Bash** | 100% native Bash | Different shell |
| **Architecture** | In-process `.so` Readline hook | Pure Bash script (~1 MB) | Native shell |
| **10k-entry history scan** | **~0.09 ms** | Tens to hundreds of ms | < 1 ms |
| **Extra startup overhead** | **~0 ms** | Hundreds of ms to 1 s+ | Varies |
| **Binary / dependencies** | **~340 KB `.so`, 0 crates** | ~1 MB scripts + caches | Full shell + plugins |
| **History sync** | Direct `history_list()` read | Internal history mirror | Native |
| **Scope** | Inline history suggestions only | Full syntax highlight + menus | Full shell UX |

---

## Installation

### 1. One-line install (Linux `x86_64` / `aarch64`, no Rust required)

Downloads the latest prebuilt `.so` from GitHub Releases, verifies `SHA256SUMS`, and installs atomically to `~/.local/lib/bash_autosuggest.so`:

```bash
curl -fsSL https://raw.githubusercontent.com/akazwz/bash-autosuggest/main/install.sh | sh
```

To also append the loader snippet to `~/.bashrc` automatically:

```bash
curl -fsSL https://raw.githubusercontent.com/akazwz/bash-autosuggest/main/install.sh | sh -s -- --update-bashrc
```

### 2. Debian / Ubuntu `.deb` package

Download the `.deb` for `amd64` or `arm64` from [GitHub Releases](https://github.com/akazwz/bash-autosuggest/releases/latest) and install via `apt`:

```bash
sudo apt install ./bash-autosuggest_*_amd64.deb
```

Then enable it in `~/.bashrc`:

```bash
if [[ $- == *i* && -f /usr/lib/bash/bash_autosuggest.so ]]; then
    enable -f /usr/lib/bash/bash_autosuggest.so bash_autosuggest 2>/dev/null && bash_autosuggest
fi
```

### 3. Build from source (zero crates, < 1 s compile)

Requires Rust 1.100+ (managed via `mise` or `rustup`):

```bash
git clone https://github.com/akazwz/bash-autosuggest.git
cd bash-autosuggest
mise run install   # or: cargo build --release
```

Then add to the end of `~/.bashrc`:

```bash
if [[ $- == *i* && -f "$HOME/.local/lib/bash_autosuggest.so" ]]; then
    enable -f "$HOME/.local/lib/bash_autosuggest.so" bash_autosuggest 2>/dev/null && bash_autosuggest
fi
```

Open a new terminal or run `exec bash`.

---

## Keybindings & Usage

| Key | Behavior (at end of line with ghost text) | Otherwise |
| :--- | :--- | :--- |
| `→` / `End` / `Ctrl+F` | Accept full suggestion | Normal `forward-char` / `end-of-line` |
| `Alt+→` / `Alt+f` | Accept next word | Normal `forward-word` |
| `←` / `Backspace` / `Tab` / `Ctrl+R` / `Enter` | Clears ghost text cleanly | Standard Readline behavior |

### Configuration & Runtime Controls

Customize the ghost text ANSI SGR style (default `38;5;242`):

```bash
export BASH_AUTOSUGGEST_STYLE="38;5;242"
```

Runtime builtin commands:

```bash
bash_autosuggest status    # show enabled state, style, and loaded history count
bash_autosuggest disable   # restore original Readline hooks and keybindings
bash_autosuggest enable    # re-enable hooks
enable -d bash_autosuggest # cleanly unload the shared library
```

---

## Requirements & Non-Goals

- **Requirements**: Linux with Bash 5.x built with loadable builtin support (`enable -f`) and exported GNU Readline symbols (true by default on Debian 12/13, Ubuntu 22.04+, Fedora, Arch, RHEL 9+). Automatically disables itself on `TERM=dumb`.
- **Non-Goals**: Real-time syntax highlighting and interactive popup completion menus. Keeping scope strictly focused on history autosuggestions is what keeps this plugin sub-millisecond and conflict-free.

## Development

```bash
mise run check      # fmt-check + clippy (warnings=deny) + unit tests + live PTY E2E tests
mise run install    # build release .so and install to ~/.local/lib/bash_autosuggest.so
mise run uninstall  # remove ~/.local/lib/bash_autosuggest.so
```

## License

[MIT](LICENSE)
