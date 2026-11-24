# itc-memory architecture

## Scope and authority

itc-memory is a local-first Rust application that provides durable project intelligence to a CLI and MCP clients. PostgreSQL provides persistent structured storage.

Git repositories and their checked-out files remain authoritative for code, plans, authored documents, and Git history. PostgreSQL is authoritative for normalized project intelligence and workflow state. File chunks, search documents, symbol records, and retrieval rankings are derived data and must be rebuildable.

## Initial deployment

PostgreSQL runs through `docker/compose.yml` and stores its database files in `postgres/data`. The service is bound to loopback only. Database credentials are supplied from the ignored `.env`; `.env.example` is a safe template. `make` is the supported local operator interface.

The initial configuration is intentionally local and single-machine. PostgreSQL is not exposed on the network; the optional Streamable HTTP MCP transport binds to loopback and requires bearer authentication.

## Application boundaries

```text
IDE agent -- MCP CLI discovery
     |
    CLI
     |
application services
       /         |          \\
discovery    workflow     retrieval
       \\         |          /
             PostgreSQL
```

The Rust application owns business logic and the CLI is its primary interface. MCP exposes CLI help, not a duplicate set of operation schemas. Search, filesystem discovery, Git inspection, symbol extraction, and linting are deterministic. CLI `lint-rust` runs Clippy; `review-rust` runs the same check before delegating selected source and diagnostics to a model. Clippy can execute project build code and is not sandboxed; see [MCP setup](mcp.md). Registered local models can interpret results through bounded inference profiles, but do not replace deterministic tools or autonomously modify source files.

## Repository identity and local discovery

Indexing uses the local list of exact checkout folders if it was compiled into the binary; otherwise it defaults to the current checkout. `ITC_MEMORY_INDEX_ROOTS_FILE` selects a line-based file and `ITC_MEMORY_INDEX_ROOTS` a comma-separated list; either overrides the default, with the file taking precedence. Broad parent folders are rejected. Discovery must not assume a directory name is a project identity. A repository is identified primarily by its canonicalized Git remote URL; its local checkout path is a `RepositoryLocation` and multiple locations may belong to the same repository. Repositories without remotes retain a generated local identity based on their Git directory and first-seen location.

The scanner records the configured root, checkout path, canonical remote, branch, HEAD commit, worktree status, individual changed paths, and scan timestamps. It uses file size, modification time, and content hash to refresh only changed data. It records unreadable paths and failed extraction without abandoning the whole run. It will never delete or modify discovered project files. `index refresh` is incremental; `index rescan` explicitly rebuilds derived content and symbols.

The scanner skips Git internals, `target`, `node_modules`, `.cache`, `postgres`, `.venv`, and `vendor`, as well as nested checkouts unless they are configured as separate roots. Dedicated client checkouts at `.cursor`, `.claude`, `.codex`, and `.copilot` are indexed under the product and retain their product-relative paths. Only supported project documents are indexed; executable client configuration is not indexed. Other nested checkouts remain excluded.

For each nested client checkout, document revisions record that checkout's HEAD rather than the product HEAD. Each indexing run records each checkout's branch, HEAD, dirty state, and root verification in its configuration, available via `index status --json`; these describe the observed checkout, not a guarantee that dirty or untracked file content exists in that commit. If Git inspection fails, the nested directory is skipped, the failure is recorded, and the partial run does not mark unseen documents stale. On a successful refresh, removed files or a removed checkout become stale. These exclusions are fixed in the scanner; a general configurable private-path exclusion list is not implemented.

## Schema boundaries

The schema and migrations cover these bounded areas:

- **Identity:** projects, repositories, repository locations, branches, commits, pull requests.
- **Source intelligence:** source documents, revisions, chunks, symbols, indexing runs and failures.
- **Workflow memory:** plans, phases, tasks, decisions, sessions, checkpoints, conventions.
- **Retrieval metadata:** full-text search vectors, freshness, provenance, supersession, and ranking inputs.

Every extracted or inferred record will retain source path/repository/revision provenance, discovery time, verification time where applicable, and freshness/supersession state. UUID primary keys are preferred for durable cross-machine records; natural unique constraints will prevent duplicate remotes, locations, revisions, and chunks.

Workflow operations are independent services, not a Plan → Agent → Ask pipeline. An indexed Markdown plan can be synchronized into plan/phase/checkbox rows without editing the source file; tasks, validated decisions, checkpoints, and sessions can also be recorded independently. Agent recovery reads structured plan/task state, source-validated decisions, checkpoint handoffs, task-linked implementation references, and indexed local changes without depending on transcript ingestion or Git-history reconstruction.

Context-pack retrieval fuses exact PostgreSQL full-text matches, normalized keyword matches, Rust-symbol source paths, and Cargo dependency-manifest paths with reciprocal-rank fusion. Text matches select a best-matching paragraph within each indexed document before generating an excerpt. Results are deduplicated by project, repository root, and source path; excerpts retain freshness and channel explanations and stay within the configured byte budget. Context metrics report the fused relevance score, excerpt size, and retrieval latency. A labeled PostgreSQL fixture measures recall@2, precision@2, and reciprocal rank; on its `connect_db sqlx` case, recall and precision rise from 0.5 to 1.0 versus exact full-text retrieval alone.

A small local embedding pilot (seven synthetic documents and six queries) measured MRR 1.000 versus 0.917 for lexical retrieval, but is too small to justify a production vector dependency. The database image does not include pgvector. Keep embeddings and pgvector deferred until a representative, repository-grounded benchmark shows a repeatable gain that outweighs added latency and operational complexity.

## Rust code conventions

The Cargo workspace has six crates: `itc-memory-core`, `itc-memory-db`, `itc-memory-index`, `itc-memory-model`, `itc-memory-cli`, and `itc-memory-mcp`. It uses Rust 2024, workspace dependency versions, focused modules, explicit public exports, and Clippy pedantic/nursery checks.

## Decisions and rejected alternatives

- PostgreSQL in Docker is selected; SQLite is rejected because structured memory is intended to be shared across local tools and eventually machines.
- A host bind mount is selected over a Docker-managed volume so data resides predictably under this project and can be backed up directly.
- CLI is the first client; MCP is an integration layer, not an alternative application core.
- Stdio and Streamable HTTP MCP expose the same CLI discovery tool. Stdio is the local IDE default; HTTP binds only to loopback and uses a shared bearer token.
- Raw session transcript ingestion and generic autonomous agent frameworks are not foundations for the initial product.
- GitHub/network indexing is not a substitute for local discovery; local uncommitted state must be visible.
