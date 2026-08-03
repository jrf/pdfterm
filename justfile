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

pdfium:
    scripts/fetch-pdfium

# Build in debug mode
build:
    cargo build

# Build in release mode
release:
    cargo build --release

# Install pdfterm and PDFium to ~/.local/bin
install: pdfium release
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p ~/.local/bin
    cp target/release/pdfterm ~/.local/bin/
    case "$(uname -s)" in
      Darwin)
        cp target/pdfium/lib/libpdfium.dylib ~/.local/bin/
        codesign -s - ~/.local/bin/libpdfium.dylib
        codesign -s - ~/.local/bin/pdfterm
        ;;
      Linux)
        cp target/pdfium/lib/libpdfium.so ~/.local/bin/
        ;;
      *)
        echo "unsupported platform: $(uname -s)" >&2
        exit 1
        ;;
    esac
