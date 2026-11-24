# MCP server

`itc-memory-mcp` exposes the project-memory and registered-model services over MCP. The tool handler is shared by stdio and Streamable HTTP; tools do not accept database credentials or arbitrary filesystem paths.

## Prerequisites

1. Copy `.env.example` to `.env` and configure the local PostgreSQL connection.
2. Start PostgreSQL and apply migrations with `make db-up` and `make db-migrate`.
3. Configure `config/models.json` locally; the repository does not ship a model registry. The test fixture at [`crates/itc-memory-mcp/tests/fixtures/models.json`](../crates/itc-memory-mcp/tests/fixtures/models.json) illustrates the format.
4. Optionally create `config/index-roots.txt` before building to compile an allowlist into the binaries. Otherwise they default to the current checkout. `ITC_MEMORY_INDEX_ROOTS_FILE` or a comma-separated `ITC_MEMORY_INDEX_ROOTS` can override either default. All entries must be exact Git checkouts; broad parent folders are rejected.

## Native project configurations

Each IDE has one per-project configuration checkout, on a branch named for that product. For Copilot the checkout is `.copilot`; native locations such as `.github/agents` and `.vscode/mcp.json` are symlinks into that checkout. Cursor, Codex, and Claude use `.cursor`, `.codex`, and `.claude` respectively. Claude's root `.mcp.json` and Codex's root `AGENTS.md` are also native entry points and may link into their corresponding checkout. Keep GitHub Actions workflows in the product repository, not in the Copilot checkout. The global checkouts use their IDE-name branches (for example, `itc-copilot`); product checkouts use product branches such as `itc-memory`.

The stdio configurations invoke the shared [launcher script](../scripts/itc-memory-mcp.sh), which establishes the itc-memory repository working directory and leaves mutation permission disabled. It prefers `~/.cargo/bin/itc-memory-mcp`, then an executable on `PATH`, falling back to `cargo run` when no binary is installed. Reinstall the binary after changing its Rust code. Credentials are not placed in MCP arguments or tool schemas.

To use the installed CLI and MCP server from any directory, install both binaries and link the existing configuration once:

```sh
cargo install --path crates/itc-memory-cli --locked
cargo install --path crates/itc-memory-mcp --locked
mkdir -p ~/.config/itc-memory
ln -s /path/to/itc-memory/.env ~/.config/itc-memory/.env
```

Replace `/path/to/itc-memory` with the checkout path. Do not overwrite an existing user configuration without checking it first. Both programs load `~/.config/itc-memory/.env` first; if it is absent they look for `.env` in the current directory and its parents. Existing process environment variables take precedence. Relative `ITC_MEMORY_INDEX_ROOTS_FILE` and `ITC_MEMORY_MODELS_FILE` paths, and the default `config/models.json`, are resolved relative to the real `.env` location, even when it is a symlink. With no `.env`, relative paths are resolved from the current directory. The `config/` directory is local and ignored by Git. To change an embedded allowlist, edit `config/index-roots.txt` and reinstall both binaries. Compiled binaries contain the paths and must not be published if those paths are private. Restart the IDE's MCP server after changing the launcher or installing a new binary.

To grant access to more repositories, update the compiled list or override it with a file or a comma-separated list of absolute paths to exact Git checkouts. The file setting takes precedence over the comma-separated list; either overrides the compiled default. These are exact locations, not discovery roots: nested repositories and sibling folders are not included unless separately listed, except for a nested `.cursor` checkout at the root of an allowed product. Indexed records and review-source selection are limited to the configured checkouts; Clippy subprocesses are not sandboxed.

After an index refresh, supported files in a product's shared `.cursor` checkout, including plans and `.cursor/rules/*.mdc`, are available as product context under their `.cursor/...` paths. The same backend makes that context available to any connected MCP client, including Copilot, Cursor, Codex, and Claude; separate IDE-specific configuration repositories are not needed for retrieval. This does not make other IDEs automatically apply Cursor rules or load Cursor agent definitions. Nested checkout provenance and failure behavior are described in [architecture](architecture.md); actual IDE access remains subject to the runtime checks below.

For manual startup:

```text
cargo run -p itc-memory-mcp -- --transport stdio
```

The process needs the user or nearby `.env`, or the corresponding database variables exported. Stdio is the recommended IDE transport.

## Available tools

- `search_projects`, `search_files`, and `search_code`
- `get_project_context`, `get_current_plan`, and `get_project_decisions`
- `get_repository_status` and `get_local_changes`
- `build_context_pack`
- `record_checkpoint` and `record_decision`
- `lint_rust` and `review_rust`
- `list_local_models`, `get_model_profile`, `ask_local_model`, `analyze_architecture`, `generate_tests`, and `compare_models`

Database retrieval and review-source selection are constrained to configured roots; Clippy's subprocess is not sandboxed as described below. Search limits and context-pack byte budgets are capped. Project context omits historical absolute-path snapshots. Database-mutating tools return an explicit error unless `ITC_MEMORY_MCP_ALLOW_WRITES=true`.

`search_files` returns literal case-sensitive matching lines and one-based line numbers from indexed content; a case-insensitive path match without a matching line has null line fields. Source text is indexed for selected checkouts, and a normal refresh backfills previously indexed source files. Results describe the indexed revision, so refresh after changes.

`lint_rust` accepts an indexed project slug and its exact allowed `root_path`, runs `cargo clippy --workspace --all-targets --message-format=json`, and returns its success, exit code, diagnostics, and stderr. `review_rust` accepts the same project and root plus `scope`: `files` with repository-relative Rust `files`, or `changed` / `diff` for staged, unstaged, and untracked Rust changes. It collects line-numbered working-tree source and separate staged/unstaged diffs, runs the same Clippy service, then delegates source, diagnostics, and indexed context to the configured Rust-review model. The response includes the lint result, source scope, model result, and routing attempts. A failed Clippy run is reported; if it fails before producing diagnostics, review returns an error rather than claiming a lint-backed result.

Both tools execute code from the selected checkout: Cargo can run build scripts and procedural macros, write build artifacts, and access the network or other paths available to the server process. They are **not sandboxed**, and can run even when database mutations are disabled. Use them only for trusted repositories and clients. Runs have a five-minute timeout, output limits, and cancellation handling; review input is capped at 30 files and 50,000 source/diff bytes. Large diagnostic payloads must be inspected separately with `lint_rust`.

Example `review_rust` arguments:

```json
{
  "project": "project-slug",
  "root_path": "/path/to/checkout",
  "scope": "files",
  "files": ["src/lib.rs"]
}
```

For local changes, use `"scope": "changed"` or `"scope": "diff"` without `files`. Both include staged and unstaged diffs and untracked Rust source; `lint_rust` only needs `project` and `root_path`.

Model registry configuration, local and trusted-LAN setup, dynamically discovered model selection, usage metadata, and the two-part consent requirement for third-party hosted inference are documented in [model routing](model-routing.md). Model calls store privacy-preserving usage metadata, not prompts or responses.

## Specialist agents

The project provides read-only Planner, Ask, and Rust Reviewer agents for both IDEs. Cursor loads project subagents from `.cursor/agents/`; VS Code Copilot loads workspace agents from `.github/agents/`. They use the existing MCP tools rather than introducing a separate model or backend. Their availability is configured, but runtime use in each IDE still needs a brief smoke check.

These agents use the model selected by their IDE or harness; they do not automatically run on a local model. Local-model inference is a separate, explicit delegation through tools such as `review_rust` or `ask_local_model`. Verify tool access and local-model delegation independently.

The transport integration test compares the tools' advertised names and input schemas over stdio and HTTP and calls the PostgreSQL-backed `search_projects` tool over both. An in-process MCP test also retrieves a rule from a nested `.cursor` checkout. These tests do not establish that every MCP tool executes over both transports or that an IDE agent or subagent can use it.

## Streamable HTTP

The server process selects one transport at startup: `stdio` (the default, used by both IDE configurations) or `http`. Both adapters expose the same tool handler and schemas. HTTP requires a bearer token of at least 32 characters and binds to loopback only. Generate a token outside source control, then run:

```text
ITC_MEMORY_MCP_TOKEN=<secret> cargo run -p itc-memory-mcp -- --transport http
```

The default endpoint is `http://127.0.0.1:8765/mcp`; override it with `ITC_MEMORY_MCP_BIND=127.0.0.1:PORT`. The server rejects browser `Origin` headers and untrusted loopback `Host` values to reduce DNS-rebinding risk. These Host checks are not a client allowlist: there is no separate per-client allowlist, and authorized HTTP clients share the configured bearer token.

This is a local HTTP service, not a network listener. A cloud agent cannot access the workstation's stdio process or loopback endpoint. Remote access requires a separately secured tunnel or proxy deployment; do not expose this plain-HTTP listener directly to a network. The MCP HTTP bearer token is supplied through environment configuration, never a tool argument.
