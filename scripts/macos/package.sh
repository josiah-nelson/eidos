#!/bin/bash
set -euo pipefail

umask 077
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=$(cd -- "$SCRIPT_DIR/../.." && pwd)
OUTPUT_DIR="$REPO_DIR/dist/macos"
APP="$OUTPUT_DIR/Eidos Collector.app"
SCRATCH=$(mktemp -d "$OUTPUT_DIR/.eidos-package.XXXXXX")
trap 'rm -rf "$SCRATCH"' EXIT
final_pkg="$OUTPUT_DIR/Eidos Collector.pkg"
rm -f "$final_pkg"

/usr/bin/codesign --verify --deep --strict -vv "$APP"
installer_identity=$(/usr/bin/security find-identity -p basic -v 2>/dev/null | /usr/bin/sed -n 's/^.*"\(Developer ID Installer[^"].*\)".*$/\1/p' | /usr/bin/head -1)
if [[ -z "$installer_identity" ]]; then
    printf '%s\n' 'no Developer ID Installer identity found; shipping notarized app archive plus install.sh'
    exit 0
fi

payload="$SCRATCH/payload"
mkdir -p "$payload/Library/Application Support/Eidos Collector" \
    "$payload/Library/LaunchDaemons" \
    "$payload/Library/LaunchAgents" \
    "$payload/usr/local/bin"
/usr/bin/ditto "$APP" "$payload/Library/Application Support/Eidos Collector/Eidos Collector.app"
install -m 0755 "$OUTPUT_DIR/eidos" "$payload/usr/local/bin/eidos"
install -m 0644 "$SCRIPT_DIR/LaunchDaemons/com.jnel.eidos.collector.plist" "$payload/Library/LaunchDaemons/com.jnel.eidos.collector.plist"
install -m 0644 "$SCRIPT_DIR/LaunchAgents/com.jnel.eidos.collector.session.plist" "$payload/Library/LaunchAgents/com.jnel.eidos.collector.session.plist"

version=$(/usr/bin/sed -n 's/^version = "\([0-9][0-9.]*\).*"/\1/p' "$REPO_DIR/Cargo.toml" | /usr/bin/head -1)
pkg="$SCRATCH/Eidos Collector.pkg"
/usr/bin/pkgbuild --root "$payload" --scripts "$SCRIPT_DIR/pkg-scripts" \
    --identifier com.jnel.eidos.collector --version "$version" \
    --install-location / --sign "$installer_identity" "$pkg"

: "${APPLE_API_KEY_P8:?APPLE_API_KEY_P8 is required for package notarization}"
: "${APPLE_API_KEY_ID:?APPLE_API_KEY_ID is required for package notarization}"
: "${APPLE_API_ISSUER_ID:?APPLE_API_ISSUER_ID is required for package notarization}"
api_key="$SCRATCH/notary-key.p8"
printf '%s' "$APPLE_API_KEY_P8" | /usr/bin/base64 -D >"$api_key"
result="$SCRATCH/notary-result.json"
/usr/bin/xcrun notarytool submit "$pkg" --key "$api_key" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID" --wait --output-format json >"$result"
submission_id=$(/usr/bin/plutil -extract id raw -o - "$result")
submission_status=$(/usr/bin/plutil -extract status raw -o - "$result")
printf 'installer signing identity: %s\n' "$installer_identity"
printf 'package notarization id=%s status=%s\n' "$submission_id" "$submission_status"
[[ "$submission_status" == Accepted ]] || exit 1
/usr/bin/xcrun stapler staple "$pkg"
/usr/sbin/spctl -a -vv -t install "$pkg"
/bin/mv "$pkg" "$final_pkg"
