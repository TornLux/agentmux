#!/usr/bin/env bash
# Build a Unix release tarball ready for end users to download + extract.
#
# Detects the host OS + arch via uname and produces one of:
#   agentmux-vX.Y.Z-linux-x86_64.tar.gz
#   agentmux-vX.Y.Z-macos-aarch64.tar.gz   (Apple Silicon)
#   agentmux-vX.Y.Z-macos-x86_64.tar.gz    (Intel Mac)
#
# Inner layout:
#   agentmux-vX.Y.Z-<platform>/
#     bin/{broker,claude-attach,hook-stop,hook-notification,hook-pretool,
#          hook-posttool,platform-discord,agentmux-cli}
#     README.md
#     QUICKSTART.md
#     PLAN.md
#     LICENSE  (if present)
#
# Notes:
#   - agentmux-tray is omitted on every Unix variant (Win32-only crate).
#   - The .ps1 files under scripts/ are skipped because they're PowerShell;
#     Unix users invoke binaries directly from bin/ for now.
#
# Usage:
#   ./scripts/build-release.sh                  # build + tar
#   SKIP_BUILD=1 ./scripts/build-release.sh     # reuse existing target/release/
#   OUT_DIR=custom ./scripts/build-release.sh   # tar lands in custom/

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# --- detect platform --------------------------------------------------------
case "$(uname -s)" in
    Linux)   OS=linux ;;
    Darwin)  OS=macos ;;
    *)       echo "build-release.sh: unsupported OS $(uname -s) — Linux and macOS only" >&2; exit 1 ;;
esac
case "$(uname -m)" in
    x86_64|amd64)   ARCH=x86_64 ;;
    arm64|aarch64)  ARCH=aarch64 ;;
    *)              echo "build-release.sh: unsupported arch $(uname -m)" >&2; exit 1 ;;
esac
PLATFORM="${OS}-${ARCH}"

# --- read workspace version from root Cargo.toml ----------------------------
VERSION="$(awk '
    /^\[workspace\.package\]/ { in_section = 1; next }
    /^\[/ && in_section      { exit }
    in_section && /^version[[:space:]]*=/ {
        match($0, /"[^"]+"/)
        print substr($0, RSTART + 1, RLENGTH - 2)
        exit
    }
' Cargo.toml)"

if [[ -z "$VERSION" ]]; then
    echo "could not read [workspace.package].version from Cargo.toml" >&2
    exit 1
fi

TAG="v$VERSION"
STEM="agentmux-${TAG}-${PLATFORM}"

echo "agentmux release builder"
echo "  version:  $VERSION"
echo "  artifact: ${STEM}.tar.gz"
echo

# --- build ------------------------------------------------------------------
if [[ "${SKIP_BUILD:-}" != "1" ]]; then
    echo "[1/4] cargo build --release --workspace --exclude agentmux-tray..."
    cargo build --release --workspace --exclude agentmux-tray
else
    echo "[1/4] cargo build (skipped)"
fi

# --- stage ------------------------------------------------------------------
echo "[2/4] staging tree..."
STAGING="$ROOT/.release-staging"
rm -rf "$STAGING"
STAGE_ROOT="$STAGING/$STEM"
mkdir -p "$STAGE_ROOT/bin"

BINARIES=(
    broker
    claude-attach
    hook-stop
    hook-notification
    hook-pretool
    hook-posttool
    platform-discord
    agentmux-cli
)
for b in "${BINARIES[@]}"; do
    src="$ROOT/target/release/$b"
    if [[ ! -f "$src" ]]; then
        echo "missing build artifact: $src" >&2
        exit 1
    fi
    cp "$src" "$STAGE_ROOT/bin/"
    echo "  + bin/$b"
done

for f in README.md QUICKSTART.md PLAN.md LICENSE; do
    if [[ -f "$ROOT/$f" ]]; then
        cp "$ROOT/$f" "$STAGE_ROOT/"
        echo "  + $f"
    else
        echo "  - $f (missing — skipped)"
    fi
done

# --- tar --------------------------------------------------------------------
echo "[3/4] tar + gzip..."
DIST="${OUT_DIR:-$ROOT/dist}"
mkdir -p "$DIST"
ARCHIVE="$DIST/${STEM}.tar.gz"
rm -f "$ARCHIVE"
tar -C "$STAGING" -czf "$ARCHIVE" "$STEM"

# --- cleanup ----------------------------------------------------------------
echo "[4/4] cleanup..."
rm -rf "$STAGING"

SIZE_BYTES=$(stat -c%s "$ARCHIVE" 2>/dev/null || stat -f%z "$ARCHIVE")
SIZE_MB=$(awk "BEGIN { printf \"%.1f\", $SIZE_BYTES / 1048576 }")
echo
echo "release built:"
echo "  $ARCHIVE"
echo "  ${SIZE_MB} MB"
