# Herdr Sleep. `make help` lists the targets.

CARGO ?= cargo
HERDR_VERSIONS ?= 0.9.2 latest

.PHONY: help build test check fmt clippy e2e clean

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

e2e: ## Run the Docker end-to-end suite (HERDR_VERSIONS="0.9.2 latest")
	@echo "The end-to-end suite is not in place yet (see tests/e2e)."; exit 1

clean: ## Remove build artifacts
	$(CARGO) clean
	rm -rf dist
