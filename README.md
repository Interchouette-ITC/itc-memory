# itc-memory

Local-first project intelligence and retrieval-augmented generation (RAG) for Interchouette projects.

Retrieval-augmented generation (RAG) is an information-retrieval mechanism that allows models to access and utilize additional data.

itc-memory gives development agents persistent, source-backed project context without repeatedly reconstructing it from files and Git history. It indexes selected local repositories, including plans, documentation, Rust symbols, Cargo dependencies, and local changes, and retrieves relevant context for a task.

Plans, tasks, decisions, and checkpoints can be used independently to maintain continuity between sessions. Git and project files remain the source of truth; PostgreSQL stores structured project memory and rebuildable indexes. Search and indexing are deterministic Rust services, while local or trusted-LAN models can help interpret the retrieved context.

### Target architecture

```text
  VS Code Copilot     Cursor     Codex     Claude
         \              |          |         /
          +-------------+----------+--------+
                         |
              MCP CLI discovery
                         |
                    Rust CLI
                         |
                 application services
                  /           |           \
             Search       Workflow     Model router
                  \           |           /
                       PostgreSQL
                           |
                   Persistent host storage
```

MCP explains the CLI to agents; agents execute the CLI through the shell.

## Start the local database

```text
make db-init
# Set ITC_MEMORY_POSTGRES_PASSWORD in .env.
make db-up
make db-status
```

See [the architecture](docs/architecture.md), [database operations](docs/database.md), and [local model routing](docs/model-routing.md).

## Index and retrieve project context

After configuring `.env` and applying migrations with `make db-migrate`, indexing uses the local `config/index-roots.txt` list if it was present at build time; otherwise it defaults to the current checkout. Override it with `ITC_MEMORY_INDEX_ROOTS_FILE` or a comma-separated `ITC_MEMORY_INDEX_ROOTS` list. Entries must be exact Git checkouts. Reinstall the CLI after changing the compiled list; see [MCP setup](docs/mcp.md).

```text
itc-memory index refresh
itc-memory index status
itc-memory search "database migration" --freshness current
itc-memory symbols "connect"
itc-memory dependencies "sqlx"
itc-memory repo changes
itc-memory context build https-github-com-interchouette-itc-itc-memory --task "Improve retrieval" --max-bytes 8000 --json
```

`--root` is a one-off override and must also name exact checkout folders. `index rescan` forces derived content and symbol rebuilding; ordinary refreshes remain incremental. Search, repository status, plans, decisions, checkpoints, and context packs also support structured JSON output.

## Independent workflow continuity

Plan, Agent, and Ask are independent ways to use project memory, not steps in a required sequence. Task creation, decision recording, checkpoints, and session tracking can each be used on their own. `workflow state` combines the available structured data for an agent handoff; it does not ingest conversation transcripts.

```text
itc-memory workflow state https-github-com-interchouette-itc-itc-memory --json
itc-memory plan sync https-github-com-interchouette-itc-itc-memory --path ".cursor/plans/itc-memory — implementation roadmap.md"
itc-memory task add https-github-com-interchouette-itc-itc-memory "Verify retrieval behavior"
itc-memory task list https-github-com-interchouette-itc-itc-memory --json
itc-memory decision record https-github-com-interchouette-itc-itc-memory --title "Keep plans authoritative" --rationale "Plan files remain the source of truth."
itc-memory checkpoint record https-github-com-interchouette-itc-itc-memory --title "Indexing complete" --summary "The changed documents are indexed." --next-steps "Run the focused checks."
itc-memory context build https-github-com-interchouette-itc-itc-memory --task "How does indexing work?" --json
```

`plan sync` imports phase headings and Markdown checkboxes from a currently indexed plan without editing that file. Run it again after indexing plan changes; until then, `workflow state` reports the normalized plan as stale. Agents can independently create/update/focus tasks, attach commit or pull-request references, record validated decisions or checkpoints, and start/finish sessions. Checkpoints include the latest indexed repository state and local changes; `workflow state` reports changes observed since the newest checkpoint.

## MCP integrations

The MCP server exposes one CLI-discovery tool over stdio or authenticated, loopback-only Streamable HTTP. See [MCP setup](docs/mcp.md) and the included [VS Code](.vscode/mcp.json) and [Cursor](.cursor/mcp.json) configurations.
