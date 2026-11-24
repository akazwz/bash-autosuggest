# bash-autosuggest

Native GNU Readline history autosuggestions for Bash (Debian / Linux), implemented in Rust as a zero-dependency Bash loadable builtin (`.so`).

- Single-layer native `/bin/bash` (no PTY wrapper, no 256-byte `bind -x` hijacking).
- Zero-allocation per-keystroke hot path with byte-level prefix short-circuiting directly over Bash's live in-memory `history_list()`.
- 100% native GNU Readline editing, UTF-8, `Tab` completion, `Ctrl+R`, and Agent compatibility.

## Keybindings

- `→` (Right Arrow), `End`, or `Ctrl+F` at end of line: accept the full grey suggestion.
- `Alt+→` or `Alt+f` at end of line: accept the next word of the suggestion.
- Any other cursor movement, Backspace, `Tab`, `Ctrl+R`, or `Enter`: clears ghost text cleanly and behaves identically to standard Bash.

## Development (`mise`)

```bash
mise run check      # fmt-check + clippy (warnings=deny) + unit tests + live PTY E2E tests
mise run install    # build release .so and install to ~/.local/lib/bash_autosuggest.so
mise run uninstall  # remove ~/.local/lib/bash_autosuggest.so
```

## Usage in `~/.bashrc`

```bash
if [[ $- == *i* && -f "$HOME/.local/lib/bash_autosuggest.so" ]]; then
    enable -f "$HOME/.local/lib/bash_autosuggest.so" bash_autosuggest 2>/dev/null && bash_autosuggest
fi
```

Optional color customization (ANSI SGR sequence, default `38;5;242`):

```bash
export BASH_AUTOSUGGEST_STYLE="38;5;242"
```

Runtime controls:

```bash
bash_autosuggest status
bash_autosuggest disable
bash_autosuggest enable
enable -d bash_autosuggest
```
