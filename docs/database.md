# Local PostgreSQL

## Setup

Run `make db-init` once. It creates an ignored `.env` from `.env.example`; set a local password there before starting the database.

```text
make db-up
make db-status
make db-shell
```

The service is PostgreSQL 18 on `127.0.0.1:${ITC_MEMORY_POSTGRES_PORT}` and persists data in `postgres/data/`. No data directory is committed to Git.

Use `make db-down` to stop the container. It preserves the host data directory. `make db-up` starts the same service again. `make db-logs` follows logs.

## Application connection

The Phase 2 Rust application will read a `DATABASE_URL` or equivalent configuration, never hard-coded credentials. A local URL follows this shape:

```text
postgres://itc_memory:<password>@127.0.0.1:5432/itc_memory
```

Apply the versioned schema with `make db-migrate`; use `make db-schema` to confirm the latest applied migration. The Rust application reads the `ITC_MEMORY_POSTGRES_*` values directly from `.env`, avoiding a duplicated password in a `DATABASE_URL`.

For installed binaries running outside the checkout, link this file as `~/.config/itc-memory/.env` as described in [MCP setup](mcp.md). Process environment values take precedence.

## Backups and restore

`make db-backup` writes a compressed SQL dump to `postgres/backups/`. Those files are ignored by Git. To restore one into the currently configured database:

```text
make db-restore FILE=postgres/backups/itc-memory-YYYYMMDDTHHMMSSZ.sql.gz
```

Restore applies SQL to the selected database and can change its contents. Take a fresh backup first. `make db-reset` intentionally refuses to delete data; destructive removal is a manual, explicit operation.
