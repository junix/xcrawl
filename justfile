# justfile for Rust project

default:
    @just --list

# Build the crate
build:
    cargo build

# Build with release optimizations
build-release:
    cargo build --release

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

# Run all checks
check-all: fmt-check clippy test

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
