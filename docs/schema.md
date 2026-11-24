# Canonical PostgreSQL schema

The schema is defined by SQLx migration files in `crates/itc-memory-db/migrations/`. It covers project/repository identity, source intelligence, workflow continuity, indexing operations, and privacy-preserving model inference usage. Records use application-generated UUIDs so a later multi-machine design is not constrained by database-local integer identifiers.

## Provenance and freshness

Source documents retain checkout location and path. Revisions retain content hashes, Git revision when known, discovery time, and current state. Freshness distinguishes current, stale, historical, temporary, duplicate, and superseded material; duplicate and superseded documents retain links to their canonical or replacement documents where known. Derived chunks, Rust symbols, Cargo dependencies, and local worktree changes are rebuildable from source. Decisions, conventions, checkpoints, sessions, and tasks retain structured provenance JSON. Indexing runs and their failure records retain counts, status, operation, path, and error detail.

Repository identity is the canonical remote where available; individual checkouts are `repository_locations`. Thus `/opt2` and `/opt3` copies of one remote are one repository while retaining machine-specific state.

## Workflow continuity

`plans` records one explicitly selected current plan per project and the source revision last synchronized. `phases` and `tasks` keep the plan's phases, checkbox state, agent-managed status, and source line. Markdown remains authoritative: `plan sync` imports indexed plan content without writing to project files. A normalized plan is stale when its source revision changes or is no longer current.

Tasks may also be created independently of any plan. The project can focus one task, and `task_references` associates tasks with commit SHAs, pull requests, or implementation references. `decisions` can be reported independently; linked decisions record the source revision they were validated against and become stale when that revision changes. `checkpoints` capture a summary, next steps, blockers, optional task/session links, and a snapshot of indexed repository state. Session transcript ingestion is not required.

`workflow state` retrieves the current plan, phases/tasks, focused task, validated decisions, recent checkpoints, local changes observed since the latest checkpoint, and task references as one JSON-capable recovery response. Individual plan, task, decision, checkpoint, and session commands remain usable independently.

## Search and migration policy

`document_chunks` has a generated PostgreSQL full-text vector and GIN index. Rust declarations are extracted with source-line provenance, and Cargo dependency metadata is associated with its manifest. Context packs fuse exact and expanded full-text matches with symbol/dependency source paths, deduplicate results, enforce an excerpt-byte budget, and report fused relevance, retrieval latency, and excerpt size. A small synthetic embedding pilot is not sufficient evidence to add pgvector; embeddings and vector retrieval remain deferred pending representative repository-grounded benchmarks.

`repository_changes` is a refreshed snapshot of staged, unstaged, and untracked paths. It is not a replacement for Git; the checkout remains authoritative.

`model_inference_runs` records profile/model/provider IDs, hosted and trusted-LAN scope, outcome, fallback use, latency, token usage when available, and estimated configured cost. Prompts, retrieved context, responses, and provider credentials are not persisted.

For the first release, the pre-release schema history is consolidated into one greenfield baseline plus one small forward migration that demonstrates future migration application. After release, migrations are append-only and forward-only: apply them with `make db-migrate`, inspect state with `make db-schema`, and do not edit an applied migration. Before any migration that could remove or rewrite durable data, run `make db-backup`, test against a restored copy, and document recovery. SQLx has no automatic down migration in this project.
