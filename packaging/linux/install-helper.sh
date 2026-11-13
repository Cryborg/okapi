#!/usr/bin/env bash
# Installs okapi-helper and its polkit policy. Run it as yourself, never with
# sudo: it builds as you, then asks sudo for the two copies only (cargo never
# runs as root). HELPER is the path the policy and okapi-platform name, and
# both scripts name the same two files; a test checks they agree.
set -euo pipefail

readonly HELPER=/usr/libexec/okapi/okapi-helper
readonly POLICY=/usr/share/polkit-1/actions/io.github.cryborg.okapi.policy

workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
readonly workspace

if [[ $EUID -eq 0 ]]; then
    echo "Run this script as yourself: it asks sudo for the copies only." >&2
    exit 1
fi

# Its own target directory and this machine's target, whatever
# CARGO_TARGET_DIR or a configured build target say: the copy below must
# take the binary just built.
host=$(rustc -vV | sed -n 's/^host: //p')
readonly host
if [[ -z $host ]]; then
    echo "rustc does not say which target this machine is." >&2
    exit 1
fi
readonly target="$workspace/target"
cargo build --release --locked --package okapi-helper --target "$host" \
    --manifest-path "$workspace/Cargo.toml" --target-dir "$target"

# The policy first: alone, it lets pkexec run nothing.
sudo install -D --owner=root --group=root --mode=0644 \
    "$workspace/packaging/linux/io.github.cryborg.okapi.policy" "$POLICY"
# Its directory root's too, even if it was there: no one else may rename
# the binary pkexec runs as root.
sudo install -d --owner=root --group=root --mode=0755 "$(dirname -- "$HELPER")"
sudo install --owner=root --group=root --mode=0755 \
    "$target/$host/release/okapi-helper" "$HELPER"
echo "Installed $HELPER and $POLICY."
