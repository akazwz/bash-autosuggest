#!/usr/bin/env sh
# One-line installer for bash-autosuggest:
#   curl -fsSL https://raw.githubusercontent.com/akazwz/bash-autosuggest/main/install.sh | sh
# Options:
#   VERSION=v0.1.0   Install a specific release tag (default: latest)
#   INSTALL_DIR=...  Target directory (default: $HOME/.local/lib)
#   --update-bashrc  Append the loader snippet to ~/.bashrc if not already present

set -eu

REPO="akazwz/bash-autosuggest"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/lib}"
TARGET_FILE="$INSTALL_DIR/bash_autosuggest.so"
UPDATE_BASHRC=0

for arg in "$@"; do
    case "$arg" in
        --update-bashrc) UPDATE_BASHRC=1 ;;
        -h|--help)
            cat <<'EOF'
Usage: install.sh [--update-bashrc]

Environment variables:
  VERSION      Release tag to install (default: latest)
  INSTALL_DIR  Install directory (default: $HOME/.local/lib)
EOF
            exit 0
            ;;
        *)
            printf 'bash-autosuggest: unknown option: %s\n' "$arg" >&2
            exit 1
            ;;
    esac
done

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

LOADER_SNIPPET="# Enable native Readline history autosuggestions (bash-autosuggest)
if [[ \$- == *i* && -f \"$TARGET_FILE\" ]]; then
    enable -f \"$TARGET_FILE\" bash_autosuggest 2>/dev/null && bash_autosuggest
fi"

if [ "$UPDATE_BASHRC" -eq 1 ]; then
    BASHRC="$HOME/.bashrc"
    if [ -f "$BASHRC" ] && grep -q 'bash_autosuggest' "$BASHRC"; then
        printf '~/.bashrc already references bash_autosuggest; skipped modifying it.\n'
    else
        printf '\n%s\n' "$LOADER_SNIPPET" >> "$BASHRC"
        printf 'Added loader snippet to %s\n' "$BASHRC"
    fi
    printf 'Start a new shell or run: exec bash\n'
else
    printf '\nAdd the following to your ~/.bashrc:\n\n%s\n\nThen open a new terminal or run: exec bash\n' "$LOADER_SNIPPET"
fi
