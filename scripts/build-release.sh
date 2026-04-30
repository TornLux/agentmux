#!/usr/bin/env bash
# Build a Unix release tarball ready for end users to download + extract.
#
# Produces one of:
#   agentmux-vX.Y.Z-linux-x86_64.tar.gz
#   agentmux-vX.Y.Z-macos-aarch64.tar.gz   (Apple Silicon)
#   agentmux-vX.Y.Z-macos-x86_64.tar.gz    (Intel Mac)
#
# Two modes:
#   - Native (default): platform inferred from `uname`; binaries from
#     target/release/.
#   - Cross-compile: set TARGET=<rustc target triple> and the script
#     passes `--target <triple>` to cargo, derives the platform stem
#     from the triple, and reads binaries from target/$TARGET/release/.
#     Currently used for cross-building x86_64-apple-darwin from an
#     Apple Silicon CI runner.
#
# Inner layout:
#   agentmux-vX.Y.Z-<platform>/
#     bin/{broker,claude-attach,hook-stop,hook-notification,hook-pretool,
#          hook-posttool,platform-discord,agentmux-cli}
#     README.md
#     QUICKSTART.md
#     PLAN.md
#     LICENSE-MIT
#     LICENSE-APACHE
#
# Notes:
#   - agentmux-tray is omitted on every Unix variant (Win32-only crate).
#   - The .ps1 files under scripts/ are skipped because they're PowerShell;
#     Unix users invoke binaries directly from bin/ for now.
#
# Usage:
#   ./scripts/build-release.sh                                    # native build + tar
#   SKIP_BUILD=1 ./scripts/build-release.sh                       # reuse existing target/release/
#   OUT_DIR=custom ./scripts/build-release.sh                     # tar lands in custom/
#   TARGET=x86_64-apple-darwin ./scripts/build-release.sh         # cross-compile

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# --- pick platform + build dir ---------------------------------------------
# In cross-compile mode (TARGET set) we trust the triple and ignore uname,
# since `uname -m` would report the *host* arch (e.g. arm64 on the runner)
# even though we're producing an x86_64 binary.
CARGO_FLAGS=()
if [[ -n "${TARGET:-}" ]]; then
    case "$TARGET" in
        x86_64-apple-darwin)        PLATFORM=macos-x86_64 ;;
        aarch64-apple-darwin)       PLATFORM=macos-aarch64 ;;
        x86_64-unknown-linux-gnu)   PLATFORM=linux-x86_64 ;;
        aarch64-unknown-linux-gnu)  PLATFORM=linux-aarch64 ;;
        *)
            echo "build-release.sh: unsupported TARGET '$TARGET'" >&2
            echo "  supported: x86_64-apple-darwin, aarch64-apple-darwin," >&2
            echo "             x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu" >&2
            exit 1
            ;;
    esac
    BUILD_DIR="target/${TARGET}/release"
    CARGO_FLAGS+=(--target "$TARGET")
else
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
    BUILD_DIR="target/release"
fi

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
    if [[ ${#CARGO_FLAGS[@]} -gt 0 ]]; then
        echo "[1/4] cargo build --release --workspace --exclude agentmux-tray ${CARGO_FLAGS[*]}..."
    else
        echo "[1/4] cargo build --release --workspace --exclude agentmux-tray..."
    fi
    cargo build --release --workspace --exclude agentmux-tray "${CARGO_FLAGS[@]}"
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
    src="$ROOT/$BUILD_DIR/$b"
    if [[ ! -f "$src" ]]; then
        echo "missing build artifact: $src" >&2
        exit 1
    fi
    cp "$src" "$STAGE_ROOT/bin/"
    echo "  + bin/$b"
done

for f in README.md QUICKSTART.md PLAN.md LICENSE-MIT LICENSE-APACHE; do
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
