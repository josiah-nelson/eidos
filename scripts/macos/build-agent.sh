#!/bin/bash
# Build `Eidos.app`, the app bundle the macOS agent is installed from.
#
# The agent is a command-line program, so a bundle looks like ceremony until
# you need Full Disk Access: that is only properly supported for executables
# inside an app bundle, and a loose binary inherits whatever the process that
# launched it was granted. Bundling the same binary changes nothing about how
# it runs and everything about what the user can grant it.
#
# Usage: build-agent.sh [--sign IDENTITY|--sign-adhoc|--no-sign]
#
# With no flag the script signs with the first "Developer ID Application"
# identity in the keychain, and falls back to an ad-hoc signature (valid
# locally, not distributable) with a printed note when there is none.
# Notarisation is a separate step and belongs to a release run that holds the
# Apple credentials; see docs/releasing.md.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=$(cd -- "$SCRIPT_DIR/../.." && pwd)
OUTPUT_DIR="$REPO_DIR/dist/macos"
APP="$OUTPUT_DIR/Eidos.app"

identity=""
mode=auto
chosen_automatically=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --sign)
            [[ $# -ge 2 ]] || { printf '%s\n' 'missing signing identity' >&2; exit 2; }
            mode=identity
            identity=$2
            shift 2
            ;;
        --sign-adhoc) mode=adhoc; shift ;;
        --no-sign) mode=none; shift ;;
        -h|--help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) printf 'unknown option: %s\n' "$1" >&2; exit 2 ;;
    esac
done

cd "$REPO_DIR"
cargo build --locked --release -p eidos-cli

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
install -m 0644 "$SCRIPT_DIR/agent-Info.plist" "$APP/Contents/Info.plist"
install -m 0755 "$REPO_DIR/target/release/eidos" "$APP/Contents/MacOS/eidos"

if [[ "$mode" == auto ]]; then
    identity=$(/usr/bin/security find-identity -p codesigning -v 2>/dev/null |
        /usr/bin/sed -n 's/^.*"\(Developer ID Application[^"]*\)".*$/\1/p' | /usr/bin/head -1)
    if [[ -n "$identity" ]]; then
        mode=identity
        chosen_automatically=1
    else
        mode=adhoc
        printf '%s\n' 'no Developer ID Application identity in the keychain; signing ad-hoc (local use only)'
    fi
fi

sign_adhoc() {
    /usr/bin/codesign --force --sign - "$APP"
    printf '%s\n' 'signed ad-hoc (local use only)'
}

case "$mode" in
    identity)
        # The hardened runtime is what notarisation later requires.
        if /usr/bin/codesign --force --options runtime --timestamp --sign "$identity" "$APP"; then
            printf 'signed with: %s\n' "$identity"
        elif [[ "$chosen_automatically" == 1 ]]; then
            # A keychain identity that needs an interactive unlock cannot sign
            # from a non-interactive shell. A local build should still produce
            # a runnable bundle; a release run signs from its own keychain.
            printf '%s\n' 'the keychain identity could not sign here; falling back to ad-hoc' >&2
            mode=adhoc
            sign_adhoc
        else
            printf 'signing with %s failed\n' "$identity" >&2
            exit 1
        fi
        ;;
    adhoc)
        sign_adhoc
        ;;
    none)
        printf '%s\n' 'left unsigned'
        ;;
esac

if [[ "$mode" != none ]]; then
    /usr/bin/codesign --verify --strict --verbose=2 "$APP"
fi

printf 'built %s\n' "$APP"
printf 'install the agent with: "%s/Contents/MacOS/eidos" service install --start-now\n' "$APP"
