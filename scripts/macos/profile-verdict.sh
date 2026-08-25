#!/bin/bash
# shellcheck disable=SC2034

# Sourced by build-collector.sh. Sets PROFILE_VALID without printing any
# provisioning-profile or certificate contents.
validate_es_profile() {
    local scratch="$1"
    local selected_signing_hash="${2:-}"
    PROFILE_VALID=0
    PROFILE_FILE="$scratch/endpoint-security.provisionprofile"
    local decoded="$scratch/profile.plist"

    if [[ -z "${APPLE_ES_PROVISIONING_PROFILE:-}" ]]; then
        PROFILE_REASON="unavailable"
        return
    fi
    if ! printf '%s' "$APPLE_ES_PROVISIONING_PROFILE" | /usr/bin/base64 -D >"$PROFILE_FILE"; then
        PROFILE_REASON="invalid base64"
        return
    fi
    if ! /usr/bin/security cms -D -i "$PROFILE_FILE" -o "$decoded" >/dev/null 2>&1; then
        PROFILE_REASON="invalid CMS"
        return
    fi

    local es_value app_identifier expiration expiration_epoch now_epoch
    es_value=$(/usr/bin/plutil -extract Entitlements.com.apple.developer.endpoint-security.client raw -o - "$decoded" 2>/dev/null || true)
    app_identifier=$(/usr/bin/plutil -extract Entitlements.com.apple.application-identifier raw -o - "$decoded" 2>/dev/null || true)
    expiration=$(/usr/bin/plutil -extract ExpirationDate raw -o - "$decoded" 2>/dev/null || true)
    if [[ "$es_value" != "true" ]]; then
        PROFILE_REASON="missing Endpoint Security entitlement"
        return
    fi
    if [[ "$app_identifier" != *.com.jnel.eidos.collector ]]; then
        PROFILE_REASON="bundle identifier mismatch"
        return
    fi
    expiration_epoch=$(/bin/date -j -f '%Y-%m-%dT%H:%M:%SZ' "$expiration" '+%s' 2>/dev/null || true)
    now_epoch=$(/bin/date '+%s')
    if [[ -z "$expiration_epoch" || "$expiration_epoch" -le "$now_epoch" ]]; then
        PROFILE_REASON="expired or unreadable expiration"
        return
    fi

    local signing_hash
    signing_hash=$selected_signing_hash
    if [[ -z "$signing_hash" ]]; then
        signing_hash=$(/usr/bin/security find-identity -p codesigning -v 2>/dev/null | /usr/bin/awk '/Developer ID Application/ {print $2; exit}')
    fi
    if [[ -z "$signing_hash" && -n "${APPLE_CERTIFICATE_P12:-}" ]]; then
        local p12="$scratch/certificate.p12" pem="$scratch/certificate.pem"
        printf '%s' "$APPLE_CERTIFICATE_P12" | /usr/bin/base64 -D >"$p12" || true
        if /usr/bin/openssl pkcs12 -in "$p12" -clcerts -nokeys -passin env:APPLE_CERTIFICATE_PASSWORD -out "$pem" >/dev/null 2>&1; then
            signing_hash=$(/usr/bin/openssl x509 -in "$pem" -noout -fingerprint -sha1 | /usr/bin/sed 's/^.*=//; s/://g')
        fi
    fi
    if [[ -z "$signing_hash" ]]; then
        PROFILE_REASON="no Developer ID Application certificate"
        return
    fi

    local index=0 profile_hash cert_text cert_der
    while :; do
        cert_text="$scratch/profile-cert-$index.txt"
        cert_der="$scratch/profile-cert-$index.der"
        if ! /usr/bin/plutil -extract "DeveloperCertificates.$index" raw -o "$cert_text" "$decoded" >/dev/null 2>&1; then
            break
        fi
        if /usr/bin/base64 -D <"$cert_text" >"$cert_der" 2>/dev/null; then
            profile_hash=$(/usr/bin/openssl x509 -inform der -in "$cert_der" -noout -fingerprint -sha1 2>/dev/null | /usr/bin/sed 's/^.*=//; s/://g')
            profile_hash=$(printf '%s' "$profile_hash" | /usr/bin/tr '[:lower:]' '[:upper:]')
            signing_hash=$(printf '%s' "$signing_hash" | /usr/bin/tr '[:lower:]' '[:upper:]')
            if [[ "$profile_hash" == "$signing_hash" ]]; then
                PROFILE_VALID=1
                PROFILE_REASON="valid"
                return
            fi
        fi
        index=$((index + 1))
    done
    PROFILE_REASON="signing certificate mismatch"
}
