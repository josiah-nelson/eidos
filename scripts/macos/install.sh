#!/bin/bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=$(cd -- "$SCRIPT_DIR/../.." && pwd)
OUTPUT_DIR="$REPO_DIR/dist/macos"

bootstrap_launchd() {
    local domain=$1
    local plist=$2
    for _ in 1 2; do
        if /bin/launchctl bootstrap "$domain" "$plist" 2>/dev/null; then
            return 0
        fi
        /bin/sleep 1
    done
    /bin/launchctl bootstrap "$domain" "$plist"
}

enable_endpoint_security=0
if [[ "${1:-}" == "--endpoint-security" ]]; then
    enable_endpoint_security=1
    shift
fi
[[ "$#" == 0 ]] || { printf 'usage: %s [--endpoint-security]\n' "$0" >&2; exit 2; }

if [[ "$EUID" -ne 0 ]]; then
    if [[ "$enable_endpoint_security" == 1 ]]; then
        exec /usr/bin/sudo "$0" --endpoint-security
    else
        exec /usr/bin/sudo "$0"
    fi
fi

pkg="$OUTPUT_DIR/Eidos Collector.pkg"
if [[ -f "$pkg" ]]; then
    /usr/sbin/spctl -a -vv -t install "$pkg"
    /usr/sbin/installer -pkg "$pkg" -target /
else
    app="$OUTPUT_DIR/Eidos Collector.app"
    /usr/bin/codesign --verify --deep --strict -vv "$app"
    /usr/bin/codesign --verify --strict -vv "$OUTPUT_DIR/eidos"
    install -d -o root -g wheel -m 0755 "/Library/Application Support/Eidos Collector"
    rm -rf "/Library/Application Support/Eidos Collector/Eidos Collector.app"
    /usr/bin/ditto "$app" "/Library/Application Support/Eidos Collector/Eidos Collector.app"
    /usr/sbin/chown -R root:wheel "/Library/Application Support/Eidos Collector/Eidos Collector.app"
    install -o root -g wheel -m 0755 "$OUTPUT_DIR/eidos" /usr/local/bin/eidos
    install -o root -g wheel -m 0644 "$SCRIPT_DIR/LaunchDaemons/com.jnel.eidos.collector.plist" /Library/LaunchDaemons/com.jnel.eidos.collector.plist
    install -o root -g wheel -m 0644 "$SCRIPT_DIR/LaunchAgents/com.jnel.eidos.collector.session.plist" /Library/LaunchAgents/com.jnel.eidos.collector.session.plist
fi

plist=/Library/LaunchDaemons/com.jnel.eidos.collector.plist
/bin/launchctl bootout system/com.jnel.eidos.collector >/dev/null 2>&1 || true
install -o root -g wheel -m 0640 /dev/null /var/log/eidos-collector.log
if [[ "$enable_endpoint_security" == 1 ]]; then
    /usr/libexec/PlistBuddy -c 'Delete :ProgramArguments:1' "$plist" 2>/dev/null || true
    /usr/libexec/PlistBuddy -c 'Add :ProgramArguments:1 string --endpoint-security' "$plist"
fi
if [[ -f "$OUTPUT_DIR/entitlement-mode" && "$(<"$OUTPUT_DIR/entitlement-mode")" == endpoint-security ]]; then
    entitlement_index=1
    [[ "$enable_endpoint_security" == 0 ]] || entitlement_index=2
    /usr/libexec/PlistBuddy -c "Delete :ProgramArguments:$entitlement_index" "$plist" 2>/dev/null || true
    /usr/libexec/PlistBuddy -c "Add :ProgramArguments:$entitlement_index string --entitlement-claimed" "$plist"
fi
bootstrap_launchd system "$plist"
/bin/launchctl enable system/com.jnel.eidos.collector
/bin/launchctl kickstart -k system/com.jnel.eidos.collector

console_uid=$(/usr/bin/stat -f '%u' /dev/console)
if [[ "$console_uid" -gt 0 ]]; then
    /bin/launchctl bootout "gui/$console_uid/com.jnel.eidos.collector.session" >/dev/null 2>&1 || true
    bootstrap_launchd "gui/$console_uid" /Library/LaunchAgents/com.jnel.eidos.collector.session.plist || true
fi
printf '%s\n' 'Eidos Collector installed and started'
