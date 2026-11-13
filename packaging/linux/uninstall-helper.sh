#!/usr/bin/env bash
# Removes what install-helper.sh installed. Run it as yourself: it asks sudo
# for the removals only.
set -euo pipefail

readonly HELPER=/usr/libexec/okapi/okapi-helper
readonly POLICY=/usr/share/polkit-1/actions/io.github.cryborg.okapi.policy

if [[ $EUID -eq 0 ]]; then
    echo "Run this script as yourself: it asks sudo for the removals only." >&2
    exit 1
fi

sudo rm -f -- "$HELPER" "$POLICY"
directory=$(dirname -- "$HELPER")
# Only when empty: never anything else that lies there.
if [[ -d $directory ]]; then
    sudo rmdir --ignore-fail-on-non-empty -- "$directory"
fi
echo "Removed $HELPER and $POLICY."
