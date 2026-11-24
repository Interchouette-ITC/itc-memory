# Phase 0 repository audit

Audited: 23 November 2026.

## Current state

The repository contains no Rust workspace, application code, persistence layer, indexing implementation, retrieval mechanism, session/checkpoint mechanism, Docker configuration, or test suite. The existing material is:

- `.cursor/plans/itc-memory — implementation roadmap.md`: the accepted product roadmap.
- `research/sources.md`: canonical external research intake; it is not implementation data.
- `README.md`: the project entry point.

Therefore no existing application implementation needs preservation or migration. The Phase 1 infrastructure added alongside this audit is the first operational component.

## Reusable local conventions

A reference Rust project was inspected for its root `Makefile` with focused includes in `make/`, workspace-level dependency/configuration approach, and preference for small, focused Rust modules with adjacent unit tests.

## Initial implementation order

1. Phase 1: persistent local PostgreSQL infrastructure (complete when validated).
2. Phase 2: Cargo workspace, configuration, migrations, and real-PostgreSQL integration test harness.
3. Phase 3: configurable repository discovery with remote-based identity and incremental indexing.

No model routing, vector database, generic agent framework, or external-repository ingestion belongs before these foundations.
