_default:
    @just --list

fmt:
    cargo fmt --all

lint:
    scripts/lint

test:
    scripts/test

check-all:
    scripts/check-all

# Build in debug mode
build:
    cargo build

# Build in release mode
release:
    cargo build --release

# Install pdfterm to Cargo's bin directory
install:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo_home="${CARGO_HOME:-$HOME/.cargo}"
    cargo install --path . --locked --force --root "$cargo_home"
    if [[ "$(uname -s)" == Darwin ]]; then
      codesign -s - "$cargo_home/bin/pdfterm"
    fi
