# Rust workspace targets.

CARGO ?= cargo

.PHONY: check test lint format db-migrate db-schema index-refresh search context

check:
	$(CARGO) check --workspace

test: db-up
	@set -a; source $(ROOT)/.env; set +a; $(CARGO) test --workspace

lint:
	$(CARGO) fmt --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings

format:
	$(CARGO) fmt

db-migrate: db-up
	$(CARGO) run -p itc-memory-cli -- database migrate

db-schema: db-up
	$(CARGO) run -p itc-memory-cli -- database status

index-refresh: db-up
	$(CARGO) run -p itc-memory-cli -- index refresh $(ARGS)

search: db-up
	$(CARGO) run -p itc-memory-cli -- search $(ARGS)

context: db-up
	$(CARGO) run -p itc-memory-cli -- context build $(ARGS)
