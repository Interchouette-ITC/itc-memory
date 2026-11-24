# MCP server

`itc-memory-mcp` helps agents discover the installed `itc-memory` CLI. It advertises one tool, `get_cli_help`, over stdio and Streamable HTTP. Agents use that help and then run CLI commands in the shell.

## Prerequisites

Install both binaries as shown below. CLI operations that use indexed data require PostgreSQL and the local environment configuration; model operations additionally require a local model registry. Discovering CLI help does not require a database or model connection.

## Native project configurations

Each IDE has one per-project configuration checkout, on a branch named for that product. For Copilot the checkout is `.copilot`; native locations such as `.github/agents` and `.vscode/mcp.json` are symlinks into that checkout. Cursor, Codex, and Claude use `.cursor`, `.codex`, and `.claude` respectively. Claude's root `.mcp.json` and Codex's root `AGENTS.md` are also native entry points and may link into their corresponding checkout. Keep GitHub Actions workflows in the product repository, not in the Copilot checkout. The global checkouts use their IDE-name branches (for example, `itc-copilot`); product checkouts use product branches such as `itc-memory`.

The stdio configurations invoke the shared [launcher script](../scripts/itc-memory-mcp.sh), which establishes the itc-memory repository working directory. It prefers `~/.cargo/bin/itc-memory-mcp`, then an executable on `PATH`, falling back to `cargo run` when no binary is installed. Reinstall the binary after changing its Rust code. Credentials are not placed in MCP arguments or tool schemas.

To use the installed CLI and MCP server from any directory, install both binaries and link the existing configuration once:

```sh
cargo install --path crates/itc-memory-cli --locked
cargo install --path crates/itc-memory-mcp --locked
mkdir -p ~/.config/itc-memory
ln -s /path/to/itc-memory/.env ~/.config/itc-memory/.env
```

Replace `/path/to/itc-memory` with the checkout path. Do not overwrite an existing user configuration without checking it first. Both programs load `~/.config/itc-memory/.env` first; if it is absent they look for `.env` in the current directory and its parents. Existing process environment variables take precedence. Relative `ITC_MEMORY_INDEX_ROOTS_FILE` and `ITC_MEMORY_MODELS_FILE` paths, and the default `config/models.json`, are resolved relative to the real `.env` location, even when it is a symlink. With no `.env`, relative paths are resolved from the current directory. The `config/` directory is local and ignored by Git. To change the CLI's embedded allowlist, edit `config/index-roots.txt` and reinstall the CLI. Compiled binaries can contain the paths and must not be published if those paths are private. Restart the IDE's MCP server after changing the launcher or installing a new binary.

To index more repositories, update the compiled list or override it with a file or a comma-separated list of absolute paths to exact Git checkouts. The file setting takes precedence over the comma-separated list; either overrides the compiled default. These are exact locations, not discovery roots: nested repositories and sibling folders are not included unless separately listed, except for a nested `.cursor` checkout at the root of an allowed product. `search-files`, `search-code`, `project context`, and review-source selection use the configured roots; other CLI database commands can access previously indexed records outside those roots. Clippy subprocesses are not sandboxed.

After an index refresh, supported files in a product's shared `.cursor` checkout, including plans and `.cursor/rules/*.mdc`, are available through the CLI as product context under their `.cursor/...` paths. This does not make other IDEs automatically apply Cursor rules or load Cursor agent definitions. Nested checkout provenance and failure behavior are described in [architecture](architecture.md); actual IDE access remains subject to the runtime checks below.

For manual startup:

```text
itc-memory-mcp --transport stdio
```

Stdio is the recommended IDE transport.

## CLI discovery

Call `get_cli_help` with `{}` for the command list or `{"command":["context","build"]}` for a specific command. The tool runs the installed CLI with `--help`; it does not run arbitrary commands. The agent then executes the appropriate `itc-memory` command in its shell. For example:

```sh
itc-memory project list --json
itc-memory project context PROJECT --json
itc-memory context build PROJECT --task "TASK" --json
itc-memory search-files "QUERY" --project PROJECT --json
itc-memory search-code "QUERY" --project PROJECT --json
itc-memory model generate-tests "CODE" --project PROJECT --json
```

The former 19 MCP operation tools have been removed. CLI commands provide retrieval, workflow, and model operations directly. For architecture analysis, use `model run --profile architecture-analyst --project PROJECT "QUESTION"`.

| Former MCP operation | CLI equivalent |
| --- | --- |
| `search_projects` | `project list` |
| `search_files` | `search-files` |
| `search_code` | `search-code` |
| `get_project_context` | `project context` |
| `get_current_plan` | `plan current` |
| `get_project_decisions` | `decisions --limit 50` |
| `get_repository_status` | `repo list` |
| `get_local_changes` | `repo changes` |
| `build_context_pack` | `context build` |
| `record_checkpoint` | `checkpoint record` |
| `record_decision` | `decision record` |
| `lint_rust` | `lint-rust` |
| `review_rust` | `review-rust` |
| `list_local_models` | `model list` |
| `get_model_profile` | `model profile` |
| `ask_local_model` | `model run` |
| `analyze_architecture` | `model run --profile architecture-analyst` |
| `generate_tests` | `model generate-tests` |
| `compare_models` | `model compare` |

Use `get_cli_help` with the CLI command path for arguments and options. Responses and defaults are not necessarily identical to the former MCP tools. In particular, CLI lint returns a nonzero exit status on Clippy failure even when `--json` includes diagnostics.

`search-files` returns literal case-sensitive matching lines and one-based line numbers from indexed content; a case-insensitive path match without a matching line has null line fields. Results describe the indexed revision, so refresh after changes.

`lint-rust PROJECT --root-path PATH` runs Clippy. `review-rust PROJECT --root-path PATH --file src/lib.rs --json` runs Clippy and delegates selected source and diagnostics to the review model; use `--changed` instead of `--file` to review current changes. These commands run Cargo build scripts and can write build artifacts; model calls also record usage. Runs have a five-minute timeout and output limits; review input is capped at 30 files and 50,000 source/diff bytes.

Model registry configuration, local and trusted-LAN setup, dynamically discovered model selection, usage metadata, and the two-part consent requirement for third-party hosted inference are documented in [model routing](model-routing.md). Model calls store privacy-preserving usage metadata, not prompts or responses.

## Specialist agents

The project provides Planner, Ask, and Rust Reviewer agents for both IDEs. Cursor loads project subagents from `.cursor/agents/`; VS Code Copilot loads workspace agents from `.github/agents/`. They use MCP to discover CLI commands, then execute the CLI. Their availability and shell permissions still need a smoke check in each IDE.

These agents use the model selected by their IDE or harness; they do not automatically run on a local model. Local-model inference is a separate, explicit delegation through CLI commands such as `review-rust` or `model run`.

The transport integration test verifies the single advertised tool and its help response over stdio and HTTP. It does not establish that an IDE agent or subagent can use it.

## Streamable HTTP

The server process selects one transport at startup: `stdio` (the default, used by both IDE configurations) or `http`. Both expose the same discovery tool. HTTP requires a bearer token of at least 32 characters and binds to loopback only. Generate a token outside source control, then run:

```text
ITC_MEMORY_MCP_TOKEN=<secret> itc-memory-mcp --transport http
```

The default endpoint is `http://127.0.0.1:8765/mcp`; override it with `ITC_MEMORY_MCP_BIND=127.0.0.1:PORT`. The server rejects browser `Origin` headers and untrusted loopback `Host` values to reduce DNS-rebinding risk. These Host checks are not a client allowlist: there is no separate per-client allowlist, and authorized HTTP clients share the configured bearer token.

This is a local HTTP service, not a network listener. A cloud agent cannot access the workstation's stdio process or loopback endpoint. Remote access requires a separately secured tunnel or proxy deployment; do not expose this plain-HTTP listener directly to a network. The MCP HTTP bearer token is supplied through environment configuration, never a tool argument.
