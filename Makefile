.PHONY: all build test fmt fmt-check clippy check clean

all: check

build:
	cargo build --workspace --all-targets

test:
	cargo test --workspace --all-targets

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

# Full pre-commit check: format, lint, build, test.
check: fmt-check clippy build test

clean:
	cargo clean
