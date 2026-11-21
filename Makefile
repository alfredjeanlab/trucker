.PHONY: check build test clean

check:
	cargo fmt
	cargo clippy --all-targets -- -D warnings
	cargo test

build:
	cargo build --release

test:
	cargo test

clean:
	cargo clean
