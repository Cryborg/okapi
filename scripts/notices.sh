#!/usr/bin/env bash
# The notices of the crates each binary embeds (crates/cli/CRATES_NOTICES,
# crates/app/CRATES_NOTICES), written by cargo-about from about.toml.
#
#   scripts/notices.sh        writes them: after any change of dependencies
#   scripts/notices.sh check  fails when those committed are out of date
#
# Needs cargo-about, at this version (another may word its output apart):
#   cargo install cargo-about --locked --features cli --version 0.9.3
set -euo pipefail

readonly CARGO_ABOUT_VERSION="cargo-about 0.9.3"

mode=${1:-write}
case "$mode" in
    write | check) ;;
    *)
        echo "usage: scripts/notices.sh [check]" >&2
        exit 2
        ;;
esac

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
if [ "$(cargo about --version 2>/dev/null)" != "$CARGO_ABOUT_VERSION" ]; then
    echo "$CARGO_ABOUT_VERSION is needed: cargo install cargo-about --locked --features cli --version 0.9.3" >&2
    exit 1
fi

scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT

# The notices of `crate`'s binary, written to `file`, in LF line endings
# (some crates ship their license texts in CRLF). A failure says why: a
# license refused names its crate.
generate() {
    local crate=$1 file=$2
    if ! cargo about generate --locked --config "$root/about.toml" \
        --manifest-path "$root/crates/$crate/Cargo.toml" \
        --output-file "$scratch/raw" "$root/about.hbs" 2>"$scratch/log"; then
        cat "$scratch/log" >&2
        exit 1
    fi
    tr -d '\r' <"$scratch/raw" >"$file"
}

status=0
for crate in cli app; do
    committed=$root/crates/$crate/CRATES_NOTICES
    if [ "$mode" = check ]; then
        generate "$crate" "$scratch/fresh"
        if ! cmp -s "$scratch/fresh" "$committed"; then
            echo "crates/$crate/CRATES_NOTICES is out of date: run scripts/notices.sh" >&2
            status=1
        fi
    else
        generate "$crate" "$committed"
    fi
done
exit "$status"
