#!/bin/bash
set -euo pipefail

purge_data=0
if [[ "${1:-}" == "--purge-data" ]]; then
    purge_data=1
    shift
fi
[[ "$#" == 0 ]] || { printf 'usage: %s [--purge-data]\n' "$0" >&2; exit 2; }
if [[ "$EUID" -ne 0 ]]; then
    if [[ "$purge_data" == 1 ]]; then
        exec /usr/bin/sudo "$0" --purge-data
    else
        exec /usr/bin/sudo "$0"
    fi
fi

console_uid=$(/usr/bin/stat -f '%u' /dev/console)
if [[ "$console_uid" -gt 0 ]]; then
    /bin/launchctl bootout "gui/$console_uid/com.jnel.eidos.collector.session" >/dev/null 2>&1 || true
fi
/bin/launchctl bootout system/com.jnel.eidos.collector >/dev/null 2>&1 || true
rm -f /Library/LaunchDaemons/com.jnel.eidos.collector.plist
rm -f /Library/LaunchAgents/com.jnel.eidos.collector.session.plist
rm -rf "/Library/Application Support/Eidos Collector/Eidos Collector.app"
/usr/bin/rmdir "/Library/Application Support/Eidos Collector" >/dev/null 2>&1 || true
rm -f /usr/local/bin/eidos /var/run/eidos-collector.sock /var/log/eidos-collector.log
if [[ "$purge_data" == 1 ]]; then
    rm -rf /var/db/eidos-collector
    printf '%s\n' 'Eidos Collector and its local spool were removed; the login-keychain study key was preserved'
else
    printf '%s\n' 'Eidos Collector was removed; /var/db/eidos-collector and the login-keychain study key were preserved'
fi
