.PHONY: all fmt clippy test build

all: fmt clippy test build

fmt:
	cargo fmt

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test --all-features

build:
	cargo build --release