# Herdr Sleep. `make help` lists the targets.

CARGO ?= cargo
PLUGIN_ID = pepsit36.herdr-sleep
BINARIES = herdr-sleep herdr-sleep-ph
VERSION = $(shell sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' herdr-plugin.toml | head -n 1)
OS = $(shell uname -s | sed -e 's/Linux/linux/' -e 's/Darwin/macos/')
ARCH = $(shell uname -m | sed -e 's/arm64/aarch64/' -e 's/amd64/x86_64/')
HERDR_VERSIONS ?= 0.9.2 latest

.PHONY: help build test check fmt clippy changelog e2e link unlink dist clean

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  %-10s %s\n", $$1, $$2}'

build: ## Build the release binaries into target/release
	$(CARGO) build --release --workspace

test: ## Run the unit and integration tests
	$(CARGO) test --workspace --locked

fmt: ## Check formatting
	$(CARGO) fmt --all -- --check

clippy: ## Lint with every warning denied
	$(CARGO) clippy --workspace --all-targets --locked -- -D warnings

check: fmt clippy test ## Everything the branch pipeline runs

changelog: ## Stamp CHANGELOG.md with the version this branch publishes
	$(CARGO) xtask changelog stamp

e2e: ## Run the Docker end-to-end suite (HERDR_VERSIONS="0.9.2 latest")
	@echo "The end-to-end suite is not in place yet (see tests/e2e)."; exit 1

link: ## Build, install the binaries into bin/ and link this checkout into Herdr
	$(CARGO) build --release --workspace
	mkdir -p bin
	for b in $(BINARIES); do cp target/release/$$b bin/$$b.new && chmod +x bin/$$b.new && mv -f bin/$$b.new bin/$$b; done
	herdr plugin link .

unlink: ## Unlink the plugin from Herdr
	herdr plugin unlink $(PLUGIN_ID)

dist: ## Archive target/release into dist/herdr-sleep-<version>-<os>-<arch>.tar.gz (+ .sha256)
	mkdir -p dist
	tar -czf dist/herdr-sleep-$(VERSION)-$(OS)-$(ARCH).tar.gz -C target/release $(BINARIES)
	cd dist && { sha256sum herdr-sleep-$(VERSION)-$(OS)-$(ARCH).tar.gz 2>/dev/null || shasum -a 256 herdr-sleep-$(VERSION)-$(OS)-$(ARCH).tar.gz; } > herdr-sleep-$(VERSION)-$(OS)-$(ARCH).tar.gz.sha256

clean: ## Remove build artifacts
	$(CARGO) clean
	rm -rf dist
