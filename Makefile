SHELL := bash

.PHONY: check test-root fmt doc dr publish test-ws test

check:
	cargo check --target x86_64-unknown-linux-gnu
	cargo check --target aarch64-unknown-linux-gnu
	cargo check --target x86_64-unknown-linux-musl
	cargo check --target aarch64-unknown-linux-musl
	cargo check --target x86_64-apple-darwin
	cargo check --target aarch64-apple-darwin
	cargo check --target x86_64-pc-windows-msvc
	cargo check --target aarch64-pc-windows-msvc
	@echo "Cross-Platform checks are passed"
test-root:
	cargo test
fmt:
	cargo fmt
doc:
	cargo doc --open
dr:
	cargo publish --dry-run

publish:
	cargo publish

test-ws:
	cargo test --workspace
test:
	cargo test && cargo test --workspace
