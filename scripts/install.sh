#!/bin/bash
# Build moco from this source tree and install it. Written for macOS (bash 3.2) and works on Linux.
# The binary targets macOS 10.7 (Lion) and runs on 10.7 through 10.14. See docs/14-macos-10.7.md.
#
#   scripts/install.sh                  build and install to ~/.local/bin/moco
#   scripts/install.sh --add-to-path    also put ~/.local/bin on your PATH (edits your shell profile, once)
#   scripts/install.sh --prefix /usr/local   install to /usr/local/bin (needs write access there; nothing is chmod'ed for you)
#   scripts/install.sh --uninstall      remove the binary (your sessions and keys are kept; --purge removes those too)
#
# Options: --prefix DIR  --target-dir DIR (build cache, default ~/.cache/moco/target)  --add-to-path  --yes (never ask)
#          --uninstall  --purge  --help
set -eu

PREFIX="${HOME}/.local"
TARGET_DIR="${MOCO_TARGET_DIR:-${HOME}/.cache/moco/target}"
ADD_PATH=0
ASSUME_YES=0
UNINSTALL=0
PURGE=0

say() { printf '%s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() { awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "$0"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) [ $# -ge 2 ] || die "--prefix needs a directory"; PREFIX="$2"; shift ;;
    --target-dir) [ $# -ge 2 ] || die "--target-dir needs a directory"; TARGET_DIR="$2"; shift ;;
    --add-to-path) ADD_PATH=1 ;;
    --yes|-y) ASSUME_YES=1 ;;
    --uninstall) UNINSTALL=1 ;;
    --purge) PURGE=1 ;;
    --help|-h) usage; exit 0 ;;
    *) die "unknown option $1 (try --help)" ;;
  esac
  shift
done

SRC="$(cd "$(dirname "$0")/.." && pwd)"
BIN_DIR="${PREFIX}/bin"
DEST="${BIN_DIR}/moco"
OS="$(uname -s)"

if [ "$OS" = "Darwin" ]; then
  APP_DIR="${MOCO_HOME:-${HOME}/Library/Application Support/moco}"
else
  APP_DIR="${MOCO_HOME:-${XDG_DATA_HOME:-${HOME}/.local/share}/moco}"
fi

ask() { # ask "question" -> 0 for yes
  [ "$ASSUME_YES" = 1 ] && return 0
  [ -t 0 ] || return 1
  printf '%s [y/N] ' "$1"
  read -r reply || return 1
  case "$reply" in y|Y|yes|YES) return 0 ;; *) return 1 ;; esac
}

profile_file() {
  case "${SHELL:-}" in
    */zsh) echo "${HOME}/.zprofile" ;;
    *) if [ -f "${HOME}/.bash_profile" ] || [ ! -f "${HOME}/.profile" ]; then echo "${HOME}/.bash_profile"; else echo "${HOME}/.profile"; fi ;;
  esac
}

on_path() { case ":${PATH}:" in *":${BIN_DIR}:"*) return 0 ;; *) return 1 ;; esac; }

# ---- uninstall --------------------------------------------------------------------------------

if [ "$UNINSTALL" = 1 ]; then
  step "Uninstalling"
  if [ -e "$DEST" ]; then rm -f "$DEST"; say "removed $DEST"; else say "$DEST is not installed"; fi
  if [ "$PURGE" = 1 ]; then
    if ask "Delete $APP_DIR (sessions, saved permissions and API keys)?"; then rm -rf "$APP_DIR"; say "removed $APP_DIR"; else say "kept $APP_DIR"; fi
  else
    say "kept your data in $APP_DIR (use --purge to remove it)"
  fi
  exit 0
fi

# ---- platform ---------------------------------------------------------------------------------

step "Checking this machine"
ARCH="$(uname -m)"
if [ "$OS" = "Darwin" ]; then
  VERSION="$(sw_vers -productVersion 2>/dev/null || echo 0)"
  say "macOS ${VERSION}, ${ARCH}"
  MAJOR="${VERSION%%.*}"; REST="${VERSION#*.}"; MINOR="${REST%%.*}"
  if [ "$MAJOR" = "10" ] && [ "${MINOR:-0}" -lt 7 ]; then
    die "moco needs macOS 10.7 or newer (this is ${VERSION})"
  fi
  if [ "$MAJOR" = "10" ] && [ "${MINOR:-0}" -lt 12 ]; then
    say "note: rustup and the system curl do not work on macOS ${VERSION}; if the build below fails, build on a newer Mac"
    say "      (this script there, or see docs/14-macos-10.7.md) and copy the binary over: it runs on 10.7 and newer"
  fi
  [ "$ARCH" = "x86_64" ] || say "note: moco is built and tested on Intel Macs; on ${ARCH} it runs through Rosetta only if you build for x86_64"
else
  say "${OS}, ${ARCH} (not macOS: the 10.7 deployment target and its check are skipped)"
fi

# A compiler and linker: on macOS they come with the Xcode command line tools.
if ! command -v cc >/dev/null 2>&1; then
  if [ "$OS" = "Darwin" ]; then
    die "no C compiler found. Install the command line tools with:  xcode-select --install   and run this script again."
  fi
  die "no C compiler found (install gcc or clang)"
fi

# ---- Rust -------------------------------------------------------------------------------------

CARGO_ENV="${CARGO_HOME:-${HOME}/.cargo}/env"
[ -f "$CARGO_ENV" ] && . "$CARGO_ENV"
if ! command -v cargo >/dev/null 2>&1; then
  step "Rust is not installed"
  say "moco is built with Rust (the toolchain goes into ~/.cargo and ~/.rustup; nothing else is touched)."
  if ask "Install Rust now with rustup (downloads https://sh.rustup.rs)?"; then
    command -v curl >/dev/null 2>&1 || die "curl is needed to download rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path
    . "$CARGO_ENV"
  else
    die "cargo not found. Install Rust from https://rustup.rs and run this script again."
  fi
fi
cargo --version >/dev/null 2>&1 || die "cargo is installed but does not work (with rustup, run:  rustup default stable)"
say "$(cargo --version)"

# ---- build ------------------------------------------------------------------------------------

step "Building moco (a few minutes the first time)"
say "build cache: ${TARGET_DIR}"
mkdir -p "$TARGET_DIR"
if [ "$OS" = "Darwin" ]; then export MACOSX_DEPLOYMENT_TARGET=10.7; fi
export CARGO_TARGET_DIR="$TARGET_DIR"
LOCKED=""
[ -f "${SRC}/Cargo.lock" ] && LOCKED="--locked"
( cd "$SRC" && cargo build --release -p moco $LOCKED )
BUILT="${TARGET_DIR}/release/moco"
[ -x "$BUILT" ] || die "the build finished but ${BUILT} is missing"

if [ "$OS" = "Darwin" ] && command -v otool >/dev/null 2>&1; then
  MINOS="$(otool -l "$BUILT" | awk '/LC_BUILD_VERSION/{f=1} f&&/minos/{print $2; exit} /LC_VERSION_MIN_MACOSX/{g=1} g&&/version/{print $2; exit}')"
  say "built for macOS ${MINOS:-unknown} and newer"
  case "${MINOS:-}" in
    10.7|10.6|10.5) ;;
    "") say "warning: could not read the deployment target of the binary" ;;
    *) die "the binary needs macOS ${MINOS}, which is newer than 10.7: it would not start on Lion" ;;
  esac
fi

# ---- install ----------------------------------------------------------------------------------

step "Installing to ${DEST}"
mkdir -p "$BIN_DIR" 2>/dev/null || die "cannot create ${BIN_DIR} (try a different --prefix)"
TMP="${DEST}.new.$$"
cp "$BUILT" "$TMP" || die "cannot write to ${BIN_DIR}"
chmod 755 "$TMP"
mv -f "$TMP" "$DEST"
"$DEST" --version >/dev/null 2>&1 || die "the installed binary does not run"
say "installed: $("$DEST" --version | head -1)"

# ---- PATH -------------------------------------------------------------------------------------

if on_path; then
  :
elif [ "$ADD_PATH" = 1 ]; then
  PROFILE="$(profile_file)"
  MARK="# added by moco install"
  if [ -f "$PROFILE" ] && grep -q "$MARK" "$PROFILE" 2>/dev/null; then
    say "${PROFILE} already has the PATH line"
  else
    printf '\n%s\nexport PATH="%s:$PATH"\n' "$MARK" "$BIN_DIR" >> "$PROFILE"
    say "added ${BIN_DIR} to your PATH in ${PROFILE}"
  fi
  say "open a new terminal window (or run:  export PATH=\"${BIN_DIR}:\$PATH\")"
else
  say ""
  say "${BIN_DIR} is not on your PATH. Either run this script again with --add-to-path, or add this line to $(profile_file):"
  say "  export PATH=\"${BIN_DIR}:\$PATH\""
fi

# ---- next steps -------------------------------------------------------------------------------

step "Next"
say "1. Give moco an API key (it is stored in ${APP_DIR}/auth.json, readable only by you):"
say "     moco auth anthropic        # or set ANTHROPIC_API_KEY; see 'moco models' for other providers"
say "2. Start it in a project directory:"
say "     cd ~/my-project && moco"
say "   Ctrl-P opens the command palette, /help lists the commands, moco --help the options."
say "Uninstall with:  ${SRC}/scripts/install.sh --uninstall"
