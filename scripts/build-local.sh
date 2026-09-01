#!/usr/bin/env bash
#
# build-local.sh — Build warp_local_proxy, warp-oss, and/or warp-tui-oss.
#
# Usage:
#   ./scripts/build-local.sh               # build proxy + warp-oss (release)
#   ./scripts/build-local.sh --proxy-only  # proxy only
#   ./scripts/build-local.sh --warp-only   # warp-oss only
#   ./scripts/build-local.sh --tui         # also build warp-tui-oss
#   ./scripts/build-local.sh --tui-only    # warp-tui-oss only
#   ./scripts/build-local.sh --debug       # debug profile
#

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="release"
PROXY_ONLY=0
WARP_ONLY=0
WITH_TUI=0
TUI_ONLY=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --debug)      PROFILE="debug"; shift ;;
        --release)    PROFILE="release"; shift ;;
        --proxy-only) PROXY_ONLY=1; shift ;;
        --warp-only)  WARP_ONLY=1; shift ;;
        --tui)        WITH_TUI=1; shift ;;
        --tui-only)   TUI_ONLY=1; shift ;;
        -h|--help)
            echo "Usage: $0 [--debug|--release] [--proxy-only|--warp-only] [--tui|--tui-only]"
            exit 0
            ;;
        *) echo "Unknown arg: $1"; exit 1 ;;
    esac
done

RELEASE_FLAG=""
[[ "$PROFILE" == "release" ]] && RELEASE_FLAG="--release"

cd "$REPO_ROOT"

if [[ "$TUI_ONLY" -eq 0 && "$WARP_ONLY" -eq 0 ]]; then
    echo "=== Building warp_local_proxy ($PROFILE) ==="
    cargo build $RELEASE_FLAG -p warp_local_proxy
    echo "  -> target/$PROFILE/warp-local-proxy"
fi

if [[ "$TUI_ONLY" -eq 0 && "$PROXY_ONLY" -eq 0 ]]; then
    echo ""
    echo "=== Building warp-oss ($PROFILE) ==="
    cargo build $RELEASE_FLAG --bin warp-oss
    echo "  -> target/$PROFILE/warp-oss"
fi

if [[ "$WITH_TUI" -eq 1 || "$TUI_ONLY" -eq 1 ]]; then
    echo ""
    echo "=== Building warp-tui-oss ($PROFILE) ==="
    CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}" \
        cargo build $RELEASE_FLAG -p warp_tui --bin warp-tui-oss --features standalone
    tui_bin="$REPO_ROOT/target/$PROFILE/warp-tui-oss"
    echo "  -> target/$PROFILE/warp-tui-oss"

    echo "=== Preparing warp-tui-oss resources ==="
    NO_LICENSES=1 SETTINGS_SCHEMA_EXECUTABLE="$tui_bin" \
        "$REPO_ROOT/script/prepare_bundled_resources" \
        "$REPO_ROOT/target/$PROFILE/resources" oss
fi

echo ""
echo "Build complete."
