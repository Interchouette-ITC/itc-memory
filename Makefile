# itc-memory developer targets

include make/common.mk
include make/docker.mk
include make/rust.mk

.DEFAULT_GOAL := help

.PHONY: help

help:
	@echo "itc-memory targets"
	@echo ""
	@echo "Database (make/docker.mk):"
	@echo "  make db-init       create .env from .env.example if needed"
	@echo "  make db-up         start PostgreSQL"
	@echo "  make db-down       stop PostgreSQL (preserves data)"
	@echo "  make db-status     show service state"
	@echo "  make db-logs       follow PostgreSQL logs"
	@echo "  make db-shell      open psql in the database container"
	@echo "  make db-backup     create postgres/backups/itc-memory-<UTC timestamp>.sql.gz"
	@echo "  make db-restore FILE=<path> restore a compressed SQL backup"
	@echo "  make db-reset      delete data only after explicit confirmation"
	@echo ""
	@echo "Rust (make/rust.mk):"
	@echo "  make check         cargo check --workspace"
	@echo "  make test          run unit and PostgreSQL integration tests"
	@echo "  make lint          cargo fmt --check and clippy"
	@echo "  make db-migrate    apply pending schema migrations"
	@echo "  make db-schema     show connection and latest migration"
