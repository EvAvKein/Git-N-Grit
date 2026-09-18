.DEFAULT_GOAL := build

build:
	cargo build

run:
	cargo run

test:
	cargo test

ci:
	cargo fmt --check
	cargo clippy
	cargo test

fmt:
	cargo fmt

lint:
	cargo clippy

clean:
	cargo clean


.PHONY: build run test fmt lint clean ci
