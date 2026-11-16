#!/usr/bin/env bash
# Installs Okapi's desktop entry and icon for the current user, so that GNOME
# under Wayland shows them for a window run with `cargo run -p okapi-app`: it
# matches the window's application id against the name of a desktop entry.
# Run it as yourself, never with sudo: everything goes under XDG_DATA_HOME.
# APP_ID is the interface's (crates/app/src/main.rs); a test checks they agree.
# To remove: delete the two files it prints.
set -euo pipefail

readonly APP_ID=io.github.cryborg.Okapi

workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
readonly workspace
readonly data="${XDG_DATA_HOME:-$HOME/.local/share}"
readonly entry="$data/applications/$APP_ID.desktop"
readonly hicolor="$data/icons/hicolor"
readonly icon="$hicolor/scalable/apps/$APP_ID.svg"

if [[ $EUID -eq 0 ]]; then
    echo "Run this script as yourself: it installs for your user only." >&2
    exit 1
fi

# The binary `cargo run -p okapi-app` runs, built here: GNOME drops an entry
# whose Exec names no program it can find, and okapi-app is on no PATH.
readonly target="$workspace/target"
cargo build --locked --package okapi-app \
    --manifest-path "$workspace/Cargo.toml" --target-dir "$target"
readonly binary="$target/debug/okapi-app"
if [[ ! -x $binary ]]; then
    echo "$binary was not built (is a build target configured?)." >&2
    exit 1
fi
# Written unquoted into Exec below, and as a sed replacement.
if [[ ! $binary =~ ^[A-Za-z0-9/._-]+$ ]]; then
    echo "$binary holds characters an Exec line would need quoted." >&2
    exit 1
fi

install -D --mode=0644 "$workspace/crates/app/assets/okapi-icon.svg" "$icon"
install -d "$(dirname -- "$entry")"
sed "s|^Exec=.*|Exec=$binary|" \
    "$workspace/packaging/linux/$APP_ID.desktop" > "$entry"

# A cache left stale would hide the new icon: refreshed only where there is
# one, never created (without one, the directory is read as it is).
if [[ -f $hicolor/icon-theme.cache ]] && command -v gtk-update-icon-cache > /dev/null; then
    gtk-update-icon-cache --quiet --ignore-theme-index "$hicolor"
fi
# GNOME Shell reads a theme's folders again only once the theme's own date
# changes, not that of a folder below it: without this, it keeps a gear for
# the icon until the next session.
touch -- "$hicolor"
if command -v update-desktop-database > /dev/null; then
    update-desktop-database --quiet "$(dirname -- "$entry")"
fi
echo "Installed $entry and $icon."
