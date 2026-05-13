#!/usr/bin/env bash
#
# build-local.sh — Build warp_local_proxy and/or warp-oss.
#
# Usage:
#   ./scripts/build-local.sh              # build both (release)
#   ./scripts/build-local.sh --proxy-only  # proxy only
#   ./scripts/build-local.sh --warp-only   # warp-oss only
#   ./scripts/build-local.sh --debug       # debug profile
#

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="release"
PROXY_ONLY=0
WARP_ONLY=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --debug)      PROFILE="debug"; shift ;;
        --release)    PROFILE="release"; shift ;;
        --proxy-only) PROXY_ONLY=1; shift ;;
        --warp-only)  WARP_ONLY=1; shift ;;
        -h|--help)
            echo "Usage: $0 [--debug|--release] [--proxy-only|--warp-only]"
            exit 0
            ;;
        *) echo "Unknown arg: $1"; exit 1 ;;
    esac
done

RELEASE_FLAG=""
[[ "$PROFILE" == "release" ]] && RELEASE_FLAG="--release"

cd "$REPO_ROOT"

if [[ "$WARP_ONLY" -eq 0 ]]; then
    echo "=== Building warp_local_proxy ($PROFILE) ==="
    cargo build $RELEASE_FLAG -p warp_local_proxy
    echo "  -> target/$PROFILE/warp-local-proxy"
fi

if [[ "$PROXY_ONLY" -eq 0 ]]; then
    echo ""
    echo "=== Building warp-oss ($PROFILE) ==="
    cargo build $RELEASE_FLAG --bin warp-oss
    echo "  -> target/$PROFILE/warp-oss"
fi

echo ""
echo "Build complete."
