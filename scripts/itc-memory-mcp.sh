#!/usr/bin/env bash
# Launch the local itc-memory MCP server for IDE stdio integrations.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

cd "${REPO_ROOT}"

if [[ -x "${HOME}/.cargo/bin/itc-memory-mcp" ]]; then
  exec "${HOME}/.cargo/bin/itc-memory-mcp" --transport stdio "$@"
fi

if command -v itc-memory-mcp >/dev/null 2>&1; then
  exec itc-memory-mcp --transport stdio "$@"
fi

exec cargo run --quiet --manifest-path "${REPO_ROOT}/Cargo.toml" \
  -p itc-memory-mcp -- --transport stdio "$@"
