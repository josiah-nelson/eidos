#!/bin/bash
set -euo pipefail

umask 077
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=$(cd -- "$SCRIPT_DIR/../.." && pwd)
OUTPUT_DIR="$REPO_DIR/dist/macos"
APP="$OUTPUT_DIR/Eidos Collector.app"
SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/eidos-sign.XXXXXX")
TEMP_KEYCHAIN=""
trap '[[ -z "$TEMP_KEYCHAIN" ]] || /usr/bin/security delete-keychain "$TEMP_KEYCHAIN" >/dev/null 2>&1 || true; rm -rf "$SCRATCH"' EXIT

identity=$(/usr/bin/security find-identity -p codesigning -v 2>/dev/null | /usr/bin/sed -n 's/^.*"\(Developer ID Application[^"].*\)".*$/\1/p' | /usr/bin/head -1)
identity_hash=$(/usr/bin/security find-identity -p codesigning -v 2>/dev/null | /usr/bin/awk '/Developer ID Application/ {print $2; exit}')
keychain_path=""
import_temporary_identity() {
    : "${APPLE_CERTIFICATE_P12:?APPLE_CERTIFICATE_P12 is required when the login keychain has no Developer ID Application identity}"
    : "${APPLE_CERTIFICATE_PASSWORD:?APPLE_CERTIFICATE_PASSWORD is required}"
    local p12="$SCRATCH/certificate.p12"
    printf '%s' "$APPLE_CERTIFICATE_P12" | /usr/bin/base64 -D >"$p12"
    TEMP_KEYCHAIN="$SCRATCH/signing.keychain-db"
    /usr/bin/security create-keychain -p '' "$TEMP_KEYCHAIN"
    /usr/bin/security unlock-keychain -p '' "$TEMP_KEYCHAIN"
    /usr/bin/security import "$p12" -k "$TEMP_KEYCHAIN" -P "$APPLE_CERTIFICATE_PASSWORD" -T /usr/bin/codesign >/dev/null
    /usr/bin/security set-key-partition-list -S apple-tool:,apple: -s -k '' "$TEMP_KEYCHAIN" >/dev/null
    identity=$(/usr/bin/security find-identity -p codesigning -v "$TEMP_KEYCHAIN" | /usr/bin/sed -n 's/^.*"\(Developer ID Application[^"].*\)".*$/\1/p' | /usr/bin/head -1)
    identity_hash=$(/usr/bin/security find-identity -p codesigning -v "$TEMP_KEYCHAIN" | /usr/bin/awk '/Developer ID Application/ {print $2; exit}')
    keychain_path="$TEMP_KEYCHAIN"
}
if [[ -z "$identity" ]]; then
    import_temporary_identity
fi
[[ -n "$identity" && -n "$identity_hash" ]] || { printf '%s\n' 'no Developer ID Application signing identity found' >&2; exit 1; }

build_for_identity() {
    "$SCRIPT_DIR/build-collector.sh" --signing-cert-sha1 "$identity_hash"
    mode=$(<"$OUTPUT_DIR/entitlement-mode")
    if [[ "$mode" == endpoint-security ]]; then
        entitlements="$SCRIPT_DIR/collector.entitlements"
    else
        entitlements="$SCRIPT_DIR/collector-pending.entitlements"
    fi
}

build_for_identity
if [[ -n "$keychain_path" ]]; then
    /usr/bin/codesign --force --options runtime --timestamp --entitlements "$entitlements" --sign "$identity" --keychain "$keychain_path" "$APP"
elif ! /usr/bin/codesign --force --options runtime --timestamp --entitlements "$entitlements" --sign "$identity" "$APP"; then
    if [[ -z "${APPLE_CERTIFICATE_P12:-}" ]]; then
        printf '%s\n' 'login-keychain identity could not sign and APPLE_CERTIFICATE_P12 is unavailable' >&2
        exit 1
    fi
    import_temporary_identity
    build_for_identity
    /usr/bin/codesign --force --options runtime --timestamp --entitlements "$entitlements" --sign "$identity" --keychain "$keychain_path" "$APP"
fi
/usr/bin/codesign --verify --deep --strict -vv "$APP"

: "${APPLE_API_KEY_P8:?APPLE_API_KEY_P8 is required for notarization}"
: "${APPLE_API_KEY_ID:?APPLE_API_KEY_ID is required for notarization}"
: "${APPLE_API_ISSUER_ID:?APPLE_API_ISSUER_ID is required for notarization}"
api_key="$SCRATCH/notary-key.p8"
printf '%s' "$APPLE_API_KEY_P8" | /usr/bin/base64 -D >"$api_key"
archive="$SCRATCH/Eidos Collector.app.zip"
/usr/bin/ditto -c -k --keepParent "$APP" "$archive"
result="$SCRATCH/notary-result.json"
/usr/bin/xcrun notarytool submit "$archive" --key "$api_key" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID" --wait --output-format json >"$result"
submission_id=$(/usr/bin/plutil -extract id raw -o - "$result")
submission_status=$(/usr/bin/plutil -extract status raw -o - "$result")
printf 'signing identity: %s\n' "$identity"
printf 'notarization id=%s status=%s\n' "$submission_id" "$submission_status"
[[ "$submission_status" == Accepted ]] || exit 1

/usr/bin/xcrun stapler staple "$APP"
/usr/bin/codesign --verify --deep --strict -vv "$APP"
/usr/sbin/spctl -a -vv -t exec "$APP"
rm -f "$OUTPUT_DIR/Eidos Collector.app.zip"
/usr/bin/ditto -c -k --keepParent "$APP" "$OUTPUT_DIR/Eidos Collector.app.zip"
