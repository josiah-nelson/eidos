#!/bin/bash
set -euo pipefail

umask 077
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=$(cd -- "$SCRIPT_DIR/../.." && pwd)
OUTPUT_DIR="$REPO_DIR/dist/macos"
APP="$OUTPUT_DIR/Eidos Collector.app"
SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/eidos-build.XXXXXX")
trap 'rm -rf "$SCRATCH"' EXIT
signing_hash=""
if [[ "${1:-}" == "--signing-cert-sha1" ]]; then
    [[ "$#" -ge 2 ]] || { printf '%s\n' 'missing signing certificate hash' >&2; exit 2; }
    signing_hash=$2
    shift 2
fi
[[ "$#" == 0 ]] || { printf 'usage: %s [--signing-cert-sha1 HASH]\n' "$0" >&2; exit 2; }

# shellcheck source=profile-verdict.sh
# shellcheck disable=SC1091
source "$SCRIPT_DIR/profile-verdict.sh"
validate_es_profile "$SCRATCH" "$signing_hash"

cd "$REPO_DIR"
cargo build --locked --release -p eidos-macos-collector --features endpoint-security
cargo build --locked --release -p eidos-cli

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$OUTPUT_DIR"
install -m 0644 "$SCRIPT_DIR/Info.plist" "$APP/Contents/Info.plist"
install -m 0755 "$REPO_DIR/target/release/eidos-collector" "$APP/Contents/MacOS/eidos-collector"
install -m 0755 "$REPO_DIR/target/release/eidos" "$OUTPUT_DIR/eidos"

if [[ "$PROFILE_VALID" == "1" && "${EIDOS_ES_ENTITLED:-0}" == "1" ]]; then
    install -m 0600 "$PROFILE_FILE" "$APP/Contents/embedded.provisionprofile"
    printf '%s\n' endpoint-security >"$OUTPUT_DIR/entitlement-mode"
    printf '%s\n' 'provisioning profile verdict: valid; Endpoint Security signing enabled'
else
    rm -f "$APP/Contents/embedded.provisionprofile"
    printf '%s\n' pending >"$OUTPUT_DIR/entitlement-mode"
    printf 'provisioning profile verdict: %s; pending entitlements selected\n' "$PROFILE_REASON"
fi
