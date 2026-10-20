.DEFAULT_GOAL := build

install:
	cargo build --release
	@mkdir -p $(HOME)/.local/bin && cp target/release/gng $(HOME)/.local/bin/gng && \
		echo "Installed gng to $(HOME)/.local/bin"
	@echo ":$$PATH:" | grep -q ":$(HOME)/.local/bin:" || \
		{ RC=$(HOME)/.$${SHELL##*/}rc; \
		  echo 'export PATH="$(HOME)/.local/bin:$$PATH"' >> $$RC; \
		  echo "Added $(HOME)/.local/bin to PATH in $$RC - restart your shell or run: source $$RC"; }

WATCH_INSTALL_STAMP := .watch-install-stamp
watch-install:
	@touch -d '1970-01-01' $(WATCH_INSTALL_STAMP)
	@echo "Watching for changes..."
	@while true; do \
		if [ -n "$$(find src Cargo.toml Cargo.lock -type f -newer $(WATCH_INSTALL_STAMP) 2>/dev/null)" ]; then \
			$(MAKE) --no-print-directory install && touch $(WATCH_INSTALL_STAMP) && echo "Watching for changes..."; \
		fi; \
		sleep 1; \
	done

build:
	cargo build

run:
	cargo run

test:
	cargo test

ci:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

fmt:
	cargo fmt

lint:
	cargo clippy

clean:
	cargo clean

.PHONY: install watch-install build run test fmt lint clean ci
