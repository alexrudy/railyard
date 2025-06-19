#!/usr/bin/env just --justfile


nightly := "nightly-2024-04-16"
msrv := "1.74"
rust := env("RUSTUP_TOOLCHAIN", "stable")

# Run all checks
all: fmt check-all deny clippy docs test machete udeps msrv
    @echo "All checks passed 🍻"

lint: fmt clippy
    @echo "Lints passed 🧹"

# Check for unused dependencies
udeps:
    #!/usr/bin/env sh
    set -euo pipefail

    bold() {
        echo "\033[1m$1\033[0m"
    }

    export CARGO_TARGET_DIR="target/hack/"
    bold "cargo +{{nightly}} udeps"
    cargo +{{nightly}} udeps  --all-features
    bold "cargo +{{nightly}} hack udeps"
    cargo +{{nightly}} hack udeps --each-feature

# Use machete to check for unused dependencies
machete:
    cargo +{{rust}} machete --skip-target-dir

alias c := check
# Check compilation
check:
    cargo +{{rust}} check --all-targets --all-features

# Check compilation across all features
check-all: check

# Build the library in release mode
build:
    cargo +{{rust}} build --release

# Run clippy
clippy:
    cargo +{{rust}} clippy --all-targets --all-features -- -D warnings

alias r := run
alias s := run
alias serve := run

# Run the project
run:
    cargo +{{rust}} run --all-features



alias d := docs
alias doc := docs
# Build documentation
docs:
    cargo +{{rust}} doc --all-features --no-deps

# Build and read documentation
read: docs
    cargo +{{rust}} doc --all-features --no-deps --open

all-docs:
    cargo +{{rust}} doc --all-features

# Check support for MSRV
msrv:
    cargo +{{msrv}} check --target-dir target/msrv/ --all-targets --all-features
    cargo +{{msrv}} doc --target-dir target/msrv/ --all-features --no-deps


alias t := test
# Run cargo tests
test: test-build test-run

[private]
test-build:
    cargo +{{rust}} nextest run --all-features --no-run

[private]
test-run:
    cargo +{{rust}} nextest run --all-features
    cargo +{{rust}} test --all-features --doc

# Run coverage tests
coverage:
    cargo +{{rust}} tarpaulin -o html --all-features

alias timing := timings
# Compile with timing checks
timings:
    cargo +{{rust}} build --all-features --timings

# Run deny checks
deny:
    cargo +{{rust}} deny check

# Run fmt checks
fmt:
    cargo +{{rust}} fmt --all --check

# Run pre-commit checks
pre-commit:
    pre-commit run --all-files

[private]
pre-commit-ci:
    SKIP=cargo-machete,fmt,check,clippy pre-commit run --color=always --all-files --show-diff-on-failure --hook-stage commit
