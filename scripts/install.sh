#!/usr/bin/env bash
set -euo pipefail

# One-command local installer for aish/aishd.
# - Builds release binaries
# - Installs user-level symlinks: aish, aishd, ai
# - Adds PATH block (idempotent) to ~/.zshrc and ~/.bashrc
# - Adds daemon autostart block (idempotent) to shell rc(s)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
FORCE=0
SHELL_MODE="auto"
BIN_DIR=""

usage() {
  cat <<'EOF'
Usage: scripts/install.sh [options]

Options:
  --bin-dir <path>      Install symlinks into this directory.
  --shell <auto|zsh|bash>
                        Which shell rc file gets daemon autostart (default: auto).
  --force               Allow replacing existing non-symlink ai/aish/aishd files.
  -h, --help            Show this help.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --bin-dir)
      BIN_DIR="${2:-}"
      shift 2
      ;;
    --shell)
      SHELL_MODE="${2:-}"
      shift 2
      ;;
    --force)
      FORCE=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    if [[ "$1" == "cargo" ]]; then
      cat >&2 <<'EOF'
missing required command: cargo
install Rust/Cargo first with:
  curl https://sh.rustup.rs -sSf | sh
EOF
      exit 2
    fi
    echo "missing required command: $1" >&2
    exit 2
  fi
}

choose_bin_dir() {
  local candidates=("$HOME/.local/bin" "$HOME/bin" "$HOME/local/bin")
  local d
  for d in "${candidates[@]}"; do
    if [[ ":$PATH:" == *":$d:"* ]]; then
      echo "$d"
      return
    fi
  done
  for d in "${candidates[@]}"; do
    if [[ -d "$d" ]]; then
      echo "$d"
      return
    fi
  done
  echo "$HOME/.local/bin"
}

ensure_link() {
  local src="$1"
  local dst="$2"
  if [[ -e "$dst" && ! -L "$dst" ]]; then
    if [[ "$FORCE" -eq 1 ]]; then
      rm -f "$dst"
    else
      echo "refusing to overwrite non-symlink: $dst (re-run with --force)" >&2
      exit 1
    fi
  fi
  ln -sfn "$src" "$dst"
}

ensure_block() {
  local file="$1"
  local begin_marker="$2"
  local block="$3"
  mkdir -p "$(dirname "$file")"
  touch "$file"
  if grep -Fq "$begin_marker" "$file"; then
    return
  fi
  {
    echo ""
    echo "$block"
  } >> "$file"
}

detect_primary_shell() {
  local shell_name
  shell_name="$(basename "${SHELL:-}")"
  case "$SHELL_MODE" in
    auto)
      case "$shell_name" in
        zsh) echo "zsh" ;;
        bash) echo "bash" ;;
        *) echo "both" ;;
      esac
      ;;
    zsh|bash)
      echo "$SHELL_MODE"
      ;;
    *)
      echo "invalid --shell value: $SHELL_MODE (expected auto|zsh|bash)" >&2
      exit 2
      ;;
  esac
}

main() {
  require_cmd cargo
  require_cmd ln

  if [[ -z "$BIN_DIR" ]]; then
    BIN_DIR="$(choose_bin_dir)"
  fi
  mkdir -p "$BIN_DIR"

  echo "building release binaries..."
  (cd "$REPO_ROOT" && cargo build --release -p aish -p aishd >/dev/null)

  local aish_src="$REPO_ROOT/target/release/aish"
  local aishd_src="$REPO_ROOT/target/release/aishd"
  if [[ ! -x "$aish_src" || ! -x "$aishd_src" ]]; then
    echo "expected release binaries at $aish_src and $aishd_src" >&2
    exit 1
  fi

  ensure_link "$aish_src" "$BIN_DIR/aish"
  ensure_link "$aishd_src" "$BIN_DIR/aishd"
  ensure_link "$BIN_DIR/aish" "$BIN_DIR/ai"

  local path_block_template path_block
  read -r -d '' path_block_template <<'EOF' || true
# >>> aish path >>>
if [ -d "__AISH_BIN_DIR__" ] && [[ ":$PATH:" != *":__AISH_BIN_DIR__:"* ]]; then
  export PATH="__AISH_BIN_DIR__:$PATH"
fi
# <<< aish path <<<
EOF
  path_block="${path_block_template//__AISH_BIN_DIR__/$BIN_DIR}"

  ensure_block "$HOME/.zshrc" "# >>> aish path >>>" "$path_block"
  ensure_block "$HOME/.bashrc" "# >>> aish path >>>" "$path_block"

  local autostart_template autostart_block
  read -r -d '' autostart_template <<'EOF' || true
# >>> aish daemon autostart >>>
case "$-" in
  *i*)
    if [ -x "__AISH_BIN_DIR__/ai" ]; then
      "__AISH_BIN_DIR__/ai" ensure-daemon --quiet >/dev/null 2>&1 || true
    fi
    ;;
esac
# <<< aish daemon autostart <<<
EOF
  autostart_block="${autostart_template//__AISH_BIN_DIR__/$BIN_DIR}"

  local shell_target
  shell_target="$(detect_primary_shell)"
  case "$shell_target" in
    zsh)
      ensure_block "$HOME/.zshrc" "# >>> aish daemon autostart >>>" "$autostart_block"
      ;;
    bash)
      ensure_block "$HOME/.bashrc" "# >>> aish daemon autostart >>>" "$autostart_block"
      ;;
    both)
      ensure_block "$HOME/.zshrc" "# >>> aish daemon autostart >>>" "$autostart_block"
      ensure_block "$HOME/.bashrc" "# >>> aish daemon autostart >>>" "$autostart_block"
      ;;
  esac

  echo "installed:"
  echo "  aish  -> $BIN_DIR/aish"
  echo "  aishd -> $BIN_DIR/aishd"
  echo "  ai    -> $BIN_DIR/ai"
  echo "updated shell rc blocks for PATH and daemon autostart"
  echo "restart your shell (or source rc) to pick up PATH changes"
}

main "$@"
