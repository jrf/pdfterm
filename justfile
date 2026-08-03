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

# Install pdfterm to ~/.local/bin
install: release
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p ~/.local/bin
    cp target/release/pdfterm ~/.local/bin/
    if [[ "$(uname -s)" == Darwin ]]; then
      codesign -s - ~/.local/bin/pdfterm
    fi
