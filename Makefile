.PHONY: help install dev build bundle test test-rust test-ts lint lint-rust fmt-rust clean help

help:
	@echo "Muni — Tauri 2 + React/TS + Rust"
	@echo ""
	@echo "Targets:"
	@echo "  install    Install JS deps (pnpm) and pre-fetch Rust deps (cargo)"
	@echo "  dev        Run the Tauri dev shell (pnpm tauri dev)"
	@echo "  build      Production frontend + Rust build (pnpm tauri build)"
	@echo "  bundle     Alias for build"
	@echo "  test       Run TS unit tests + Rust unit/integration tests"
	@echo "  test-rust  Cargo tests only"
	@echo "  test-ts    Vitest tests only"
	@echo "  lint       TS typecheck + cargo clippy"
	@echo "  lint-rust  cargo clippy --all-targets -- -D warnings"
	@echo "  fmt-rust   cargo fmt"
	@echo "  clean      Remove node_modules / target / dist"

install:
	pnpm install
	cargo fetch --manifest-path apps/desktop/src-tauri/Cargo.toml

dev:
	pnpm dev

build bundle:
	pnpm build

test: test-ts test-rust

test-ts:
	pnpm test

test-rust:
	cargo test --features test-fixtures --manifest-path apps/desktop/src-tauri/Cargo.toml

lint: lint-rust
	pnpm lint

lint-rust:
	cargo clippy --manifest-path apps/desktop/src-tauri/Cargo.toml --all-targets -- -D warnings

fmt-rust:
	cargo fmt --manifest-path apps/desktop/src-tauri/Cargo.toml --all

clean:
	rm -rf node_modules apps/*/node_modules apps/*/dist apps/*/src-tauri/target apps/*/src-tauri/gen
