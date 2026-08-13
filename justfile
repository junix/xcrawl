# justfile for Rust project

default:
    @just --list

# Build the crate
build:
    cargo build

# Build with release optimizations
build-release:
    cargo build --release

# Install the release binary into the shared per-platform bin directory.
install:
    #!/usr/bin/env bash
    set -euo pipefail
    case "$(uname -s)" in
      Darwin) os_name=macos ;;
      Linux) os_name=linux ;;
      *) echo "unsupported OS" >&2; exit 1 ;;
    esac
    case "$(uname -m)" in
      arm64|aarch64) arch_name=arm64 ;;
      x86_64|amd64) arch_name=x86 ;;
      *) echo "unsupported architecture" >&2; exit 1 ;;
    esac
    install_dir="${SYNC_BIN_DIR:-${HOME}/sync/${os_name}-${arch_name}-bin}"
    cargo build --release --locked
    mkdir -p "$install_dir"
    cp target/release/xcrawl "$install_dir/xcrawl"
    if [[ "$os_name" == "macos" ]]; then
      codesign --force --sign - "$install_dir/xcrawl"
    fi
    echo "Installed $install_dir/xcrawl"

# Run tests
test:
    cargo test

# Run tests with output
test-verbose:
    cargo test -- --nocapture

# Check code without building
check:
    cargo check

# Format code
fmt:
    cargo fmt

# Format code and check
fmt-check:
    cargo fmt -- --check

# Run linter
clippy:
    cargo clippy -- -D warnings

# Run linter with fixes
clippy-fix:
    cargo clippy --fix --allow-dirty --allow-staged

# Run the complete release gate
check-all:
    cargo fmt --all -- --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked --all-targets
    cargo build --locked

# Clean build artifacts
clean:
    cargo clean

# Update dependencies
update:
    cargo update

# Run cargo doc with open
doc:
    cargo doc --open

# Run with watch
watch:
    cargo watch -x check -x test -x run

# Install dev tools
install-tools:
    cargo install cargo-watch cargo-edit cargo-audit

# Benchmark
bench:
    cargo bench

# Generate coverage
coverage:
    cargo tarpaulin --out Html

# Show dependency tree
deps:
    cargo tree

# Show outdated dependencies
outdated:
    cargo outdated
