#!/bin/sh
# apkscope installer for Linux and macOS.
#
# Downloads the right binary from GitHub Releases, verifies its SHA-256 and
# installs it to a bin directory on your PATH.
#
# Usage:
#   ./scripts/install.sh          # latest version
#   ./scripts/install.sh 0.9.1    # specific version
#
# Environment overrides (for forks, mirrors, custom locations):
#   APKSCOPE_REPO=suffixing/apkscope        # GitHub owner/name to download from
#   APKSCOPE_BASE_URL=https://github.com    # mirror / GitHub Enterprise host
#   APKSCOPE_INSTALL_DIR=/usr/local/bin     # where to put the binary
set -eu

REPO="${APKSCOPE_REPO:-suffixing/apkscope}"
BASE="${APKSCOPE_BASE_URL:-https://github.com}"
DEST="${APKSCOPE_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${1:-}"

usage() {
  sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
  exit 1
}

case "${1:-}" in
  -h|--help) usage ;;
esac

# --- output helpers -------------------------------------------------------------
# Colors are used only when writing to a terminal; NO_COLOR (no-color.org) and
# TERM=dumb disable them, and non-tty output falls back to plain ASCII markers
# so piped logs stay clean. NO_COLOR bans color, not symbols, so a terminal
# with NO_COLOR still gets the unicode tick.
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != "dumb" ]; then
  bold=$(printf '\033[1m'); dim=$(printf '\033[2m')
  green=$(printf '\033[32m'); yellow=$(printf '\033[33m'); red=$(printf '\033[31m')
  reset=$(printf '\033[0m')
else
  bold=""; dim=""; green=""; yellow=""; red=""; reset=""
fi
if [ -t 1 ] && [ "${TERM:-}" != "dumb" ]; then
  tick="✓"
else
  tick="+"
fi

field() { printf '  %s%-11s%s %s\n' "$dim" "$1" "$reset" "$2"; }
ok()    { printf '  %s%s%s %s\n' "$green" "$tick" "$reset" "$1"; }
step()  { printf '  %s ...\n' "$1"; }
note()  { printf '%snote:%s %s\n' "$dim" "$reset" "$1" >&2; }
warn()  { printf '%swarning:%s %s\n' "$yellow" "$reset" "$1" >&2; }
die()   { printf '%serror:%s %s\n' "$red" "$reset" "$1" >&2; exit 1; }

# human-readable byte count (POSIX arithmetic only, so no locale surprises)
hsize() {
  if [ "$1" -ge 1048576 ]; then
    printf '%d.%d MB' $(( $1 / 1048576 )) $(( ($1 % 1048576) / 104858 ))
  elif [ "$1" -ge 1024 ]; then
    printf '%d KB' $(( $1 / 1024 ))
  else
    printf '%d B' "$1"
  fi
}

# show a path under $HOME as ~/... when possible
homepath() {
  case "$1" in
    "$HOME"?*) printf '~%s' "${1#"$HOME"}" ;;
    *) printf '%s' "$1" ;;
  esac
}

# --- detect platform ------------------------------------------------------------
os=$(uname -s)
case "$os" in
  Linux)  os=linux ;;
  Darwin) os=macos ;;
  *) die "unsupported OS '$os' (this installer covers Linux and macOS; on Windows use scripts/install.ps1 or grab apkscope_windows_x86_64.exe from the Releases page)" ;;
esac

arch=$(uname -m)
case "$arch" in
  x86_64|amd64)  arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) die "unsupported CPU architecture '$arch'" ;;
esac

# The Linux binaries are glibc builds with the viewer; a musl libc (Alpine,
# Void musl, ...) cannot load them. There the installer takes the static
# CLI + MCP binary instead — which has no viewer: a fully static binary
# cannot dlopen() the desktop's X11/Wayland/GL libraries, so no window is
# possible on musl. Say so up front instead of installing a binary that
# would fail to start.
if [ "$os" = linux ]; then
  case "$(ldd --version 2>&1 | head -n1)" in
    *musl*) is_musl=1 ;;
    *) if [ -e "/lib/ld-musl-$arch.so.1" ]; then is_musl=1; else is_musl=0; fi ;;
  esac
else
  is_musl=0
fi

asset="apkscope_${os}_${arch}"
if [ "$is_musl" -eq 1 ]; then
  asset="apkscope_${os}_musl_${arch}"
  note "musl libc detected: installing the static CLI + MCP build (apkscope_linux_musl_${arch}) — it has no viewer (a fully static binary cannot load the desktop's X11/Wayland/GL libraries); the glibc binary and the .deb/AppImage packages have it"
fi

# --- resolve the download URL ---------------------------------------------------
if [ -n "$VERSION" ]; then
  case "$VERSION" in v*) ;; *) VERSION="v$VERSION" ;; esac   # accept 0.9.1 or v0.9.1
  base_url="$BASE/$REPO/releases/download/$VERSION"
else
  base_url="$BASE/$REPO/releases/latest/download"
fi

# --- downloader -------------------------------------------------------------------
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q "$1" -O "$2"; }
else
  die "need curl or wget to download"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

printf '\n%s  apkscope installer%s\n\n' "$bold" "$reset"
field "platform"    "$os $arch"
field "version"     "${VERSION:-latest}"
field "destination" "$(homepath "$DEST")"
printf '\n'

step "downloading $asset"
if ! fetch "$base_url/$asset" "$tmp/apkscope"; then
  die "could not download $asset (no release for this platform yet? check $BASE/$REPO/releases)"
fi
size=$(wc -c < "$tmp/apkscope" | tr -cd '0-9')
ok "downloaded ($(hsize "$size"))"

# --- verify checksum (best effort: warn, don't block, if checksums absent) ------
if fetch "$base_url/checksums.txt" "$tmp/checksums.txt"; then
  want=$(awk -v f="$asset" '$0 ~ f"$" {print $1; exit}' "$tmp/checksums.txt")
  if [ -n "$want" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
      got=$(sha256sum "$tmp/apkscope" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
      got=$(shasum -a 256 "$tmp/apkscope" | awk '{print $1}')
    else
      got=""
      warn "no sha256 tool found, skipping checksum verification"
    fi
    if [ -n "$got" ]; then
      if [ "$got" != "$want" ]; then
        die "SHA-256 mismatch for $asset
  expected $want
  got      $got"
      fi
      ok "checksum verified (sha256)"
    fi
  fi
else
  warn "checksums.txt not found for this release, skipping verification"
fi

# --- sanity: we must have received an executable, not an error page --------------
if ! file "$tmp/apkscope" 2>/dev/null | grep -Eq 'ELF|Mach-O'; then
  if [ "$size" -lt 1000000 ]; then
    die "downloaded file is not a binary (too small / wrong type)"
  fi
fi

# --- install ---------------------------------------------------------------------
mkdir -p "$DEST"
if ! cp "$tmp/apkscope" "$DEST/apkscope" 2>/dev/null; then
  die "cannot write to $DEST (try: sudo APKSCOPE_INSTALL_DIR=$DEST ./scripts/install.sh, or set APKSCOPE_INSTALL_DIR to a directory you own)"
fi
chmod 0755 "$DEST/apkscope"
ok "installed $(homepath "$DEST")/apkscope"

# --- add to PATH automatically (idempotent, shell-aware) --------------------------
# If $DEST isn't on $PATH, try to add it to the user's shell rc file so future
# sessions pick it up. The current shell still won't see it (PATH is cached at
# session start), so we also print the command to run in the current session.
now_hint=""
case ":$PATH:" in
  *":$DEST:"*)
    ok "PATH already includes $(homepath "$DEST")"
    ;;
  *)
    line="export PATH=\"$DEST:\$PATH\""
    added=""
    case "${SHELL:-}" in
      */fish)
        # The script runs under /bin/sh even when the user's login shell is
        # fish (curl | sh always uses sh), so fish_add_path isn't on $PATH
        # here. Spawn fish to run it — that writes the universal var to
        # ~/.config/fish/fish_variables, which all future fish sessions read.
        if command -v fish >/dev/null 2>&1; then
          if fish -c "fish_add_path '$DEST'" 2>/dev/null; then
            added="fish"
          fi
        fi
        ;;
      */zsh)
        rc="$HOME/.zshrc"
        if [ -w "$HOME" ] && { [ ! -e "$rc" ] || [ -w "$rc" ]; } && ! grep -qF "$line" "$rc" 2>/dev/null; then
          printf '\n# added by apkscope installer\n%s\n' "$line" >> "$rc"
          added="zsh"
        fi
        ;;
      */bash)
        # ~/.bash_profile on macOS, ~/.bashrc on Linux (most distros)
        rc="$HOME/.bashrc"
        [ "$(uname -s)" = "Darwin" ] && rc="$HOME/.bash_profile"
        if [ -w "$HOME" ] && { [ ! -e "$rc" ] || [ -w "$rc" ]; } && ! grep -qF "$line" "$rc" 2>/dev/null; then
          printf '\n# added by apkscope installer\n%s\n' "$line" >> "$rc"
          added="bash"
        fi
        ;;
      *)
        # unknown shell — don't touch anything, just print instructions
        ;;
    esac

    if [ -n "$added" ]; then
      ok "PATH updated for future $added sessions"
      case "$added" in
        fish) now_hint="fish_add_path $(homepath "$DEST")" ;;
        *)    now_hint="source $(homepath "$rc")" ;;
      esac
    else
      warn "$(homepath "$DEST") is not on your PATH; add it:"
      case "${SHELL:-}" in
        */fish) printf '    %s\n' "fish_add_path $DEST" >&2 ;;
        */zsh)  printf '    %s\n' "echo '$line' >> ~/.zshrc" >&2 ;;
        */bash) printf '    %s\n' "echo '$line' >> ~/.bashrc" >&2 ;;
        *)      printf '    %s\n' "add $DEST to your PATH" >&2 ;;
      esac
    fi
    ;;
esac

printf '\n'
"$DEST/apkscope" --version
printf '\n'
if [ -n "$now_hint" ]; then
  field "use it now" "$now_hint"
fi
field "learn more" "apkscope --help"
printf '\n'
