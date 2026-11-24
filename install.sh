#!/usr/bin/env sh
# One-line installer / uninstaller for bash-autosuggest:
#   Install:   curl -fsSL https://raw.githubusercontent.com/akazwz/bash-autosuggest/main/install.sh | sh
#   Uninstall: curl -fsSL https://raw.githubusercontent.com/akazwz/bash-autosuggest/main/install.sh | sh -s -- --uninstall
# Options:
#   VERSION=v0.1.0   Install a specific release tag (default: latest)
#   INSTALL_DIR=...  Target directory (default: $HOME/.local/lib)
#   --update-bashrc  Append a managed loader block to ~/.bashrc
#   --uninstall      Remove installed .so and any managed loader block in ~/.bashrc

set -eu

REPO="akazwz/bash-autosuggest"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/lib}"
TARGET_FILE="$INSTALL_DIR/bash_autosuggest.so"
BASHRC="$HOME/.bashrc"
MARK_START="# >>> bash-autosuggest >>>"
MARK_END="# <<< bash-autosuggest <<<"
UPDATE_BASHRC=0
UNINSTALL=0

for arg in "$@"; do
    case "$arg" in
        --update-bashrc) UPDATE_BASHRC=1 ;;
        --uninstall) UNINSTALL=1 ;;
        -h|--help)
            cat <<'EOF'
Usage: install.sh [--update-bashrc | --uninstall]

Options:
  --update-bashrc  Append a managed loader block to ~/.bashrc
  --uninstall      Remove bash_autosuggest.so and its managed block from ~/.bashrc

Environment variables:
  VERSION          Release tag to install (default: latest)
  INSTALL_DIR      Install directory (default: $HOME/.local/lib)
EOF
            exit 0
            ;;
        *)
            printf 'bash-autosuggest: unknown option: %s\n' "$arg" >&2
            exit 1
            ;;
    esac
done

remove_bashrc_block() {
    if [ ! -f "$BASHRC" ]; then
        return 0
    fi
    if grep -Fq "$MARK_START" "$BASHRC"; then
        tmp_rc="$(mktemp)"
        awk -v start="$MARK_START" -v end="$MARK_END" '
            $0 == start { skip = 1; next }
            $0 == end { skip = 0; next }
            !skip { print }
        ' "$BASHRC" > "$tmp_rc"
        mv -f "$tmp_rc" "$BASHRC"
        printf 'Removed managed bash-autosuggest block from %s\n' "$BASHRC"
    elif grep -q 'bash_autosuggest' "$BASHRC"; then
        printf 'Note: found manual bash_autosuggest lines in %s; please remove them manually.\n' "$BASHRC"
    fi
}

if [ "$UNINSTALL" -eq 1 ]; then
    if [ -f "$TARGET_FILE" ]; then
        rm -f "$TARGET_FILE"
        printf 'Removed %s\n' "$TARGET_FILE"
    else
        printf '%s not found (already removed).\n' "$TARGET_FILE"
    fi
    remove_bashrc_block
    printf 'Uninstall complete. Open a new terminal or run: exec bash\n'
    exit 0
fi

OS="$(uname -s)"
if [ "$OS" != "Linux" ]; then
    printf 'bash-autosuggest: only Linux is supported (detected: %s)\n' "$OS" >&2
    exit 1
fi

RAW_ARCH="$(uname -m)"
case "$RAW_ARCH" in
    x86_64|amd64) ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *)
        printf 'bash-autosuggest: unsupported architecture: %s (build from source with cargo)\n' "$RAW_ARCH" >&2
        exit 1
        ;;
esac

if ! command -v bash >/dev/null 2>&1; then
    printf 'bash-autosuggest: bash not found in PATH\n' >&2
    exit 1
fi

if ! bash -c 'help enable' 2>/dev/null | grep -q -- '-f filename'; then
    printf 'bash-autosuggest: your bash does not support loadable builtins (enable -f)\n' >&2
    exit 1
fi

ASSET="bash_autosuggest-linux-${ARCH}.so"
if [ -n "${VERSION:-}" ]; then
    BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
else
    BASE_URL="https://github.com/${REPO}/releases/latest/download"
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

download() {
    url="$1"
    dest="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$url" -o "$dest"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$dest" "$url"
    else
        printf 'bash-autosuggest: curl or wget is required\n' >&2
        exit 1
    fi
}

printf 'Downloading %s...\n' "$ASSET"
download "${BASE_URL}/${ASSET}" "${TMP_DIR}/${ASSET}"

if download "${BASE_URL}/SHA256SUMS" "${TMP_DIR}/SHA256SUMS" 2>/dev/null; then
    if command -v sha256sum >/dev/null 2>&1; then
        EXPECTED="$(awk -v f="$ASSET" '$2 == f || $2 == ("*" f) { print $1; exit }' "${TMP_DIR}/SHA256SUMS")"
        if [ -n "$EXPECTED" ]; then
            ACTUAL="$(sha256sum "${TMP_DIR}/${ASSET}" | awk '{ print $1 }')"
            if [ "$EXPECTED" != "$ACTUAL" ]; then
                printf 'bash-autosuggest: SHA256 mismatch!\n  expected: %s\n  actual:   %s\n' "$EXPECTED" "$ACTUAL" >&2
                exit 1
            fi
            printf 'SHA256 verified.\n'
        fi
    fi
fi

mkdir -p "$INSTALL_DIR"
chmod 755 "${TMP_DIR}/${ASSET}"
# Atomic rename so running Bash processes with the old .so mapped are never disturbed.
mv -f "${TMP_DIR}/${ASSET}" "$TARGET_FILE"

printf 'Installed to %s\n' "$TARGET_FILE"

LOADER_BLOCK="${MARK_START}
if [[ \$- == *i* && -f \"$TARGET_FILE\" ]]; then
    enable -f \"$TARGET_FILE\" bash_autosuggest 2>/dev/null && bash_autosuggest
fi
${MARK_END}"

if [ "$UPDATE_BASHRC" -eq 1 ]; then
    if [ -f "$BASHRC" ] && grep -q 'bash_autosuggest' "$BASHRC"; then
        printf '%s already references bash_autosuggest; skipped modifying it.\n' "$BASHRC"
    else
        printf '\n%s\n' "$LOADER_BLOCK" >> "$BASHRC"
        printf 'Added managed loader block to %s\n' "$BASHRC"
    fi
    printf 'Start a new shell or run: exec bash\n'
else
    printf '\nAdd the following to your ~/.bashrc:\n\n%s\n\nThen open a new terminal or run: exec bash\n' "$LOADER_BLOCK"
fi
