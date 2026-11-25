#!/usr/bin/env bash
# What `okapi-vm run --coverage` and `okapi-vm coverage report` need on the
# host, without sudo: llvm-profdata and llvm-cov of the LLVM rustc was built
# with, and rustfilt, which turns the symbols back into Rust paths.
set -euo pipefail

rustup component add llvm-tools
cargo install --locked rustfilt
