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
	@$(DIFF_README_PREVIEW) || { \
		echo "README.md's TUI preview is stale; run 'make update-readme-preview' to update it."; exit 1; }

DIFF_README_PREVIEW = cargo run --quiet --example generate_readme_preview > target/readme-preview.md && \
	awk '/preview:end/ { p = 0 } p; /preview:start/ { p = 1 }' README.md | \
	diff -u --color - target/readme-preview.md

diff-readme-preview:
	@$(DIFF_README_PREVIEW)

update-readme-preview:
	@$(DIFF_README_PREVIEW) || { \
		printf "\nUpdate README.md's TUI preview? [y/N] " && read -r answer < /dev/tty && \
		echo "$$answer" | grep -qi '^y' && cargo run --quiet --example generate_readme_preview -- --write; \
	}

githook:
	chmod +x scripts/pre-push-hook && cp scripts/pre-push-hook .git/hooks/pre-push

fmt:
	cargo fmt

lint:
	cargo clippy

clean:
	cargo clean

.PHONY: install watch-install build run test fmt lint clean ci githook diff-readme-preview update-readme-preview
