# Local model registry and routing

The model crate provides a JSON-configured registry shared by the CLI and MCP server. Configured model IDs and task-profile IDs are stable names defined in the registry. Dynamic selections use exact provider-qualified IDs returned from live discovery; arbitrary model names are rejected. The registry defaults to the local, Git-ignored `config/models.json`, overridable with `ITC_MEMORY_MODELS_FILE`. Relative paths are resolved against the real location of the loaded `.env`; see [MCP setup](mcp.md) for user-level configuration.

## Local and LAN setup

Create a local registry with provider endpoints under `providers[].base_url` and `providers[].trusted_lan`. The [test fixture](../crates/itc-memory-mcp/tests/fixtures/models.json) illustrates the format; configure the models and task profiles needed for your installation.

Installed models are discovered from each Ollama `/api/tags` response and appear in `model list` as `provider-id::exact-model-name`; model names do not need to be copied into the registry. Discovered models are rechecked against the provider before inference, are never automatically assigned to a task profile, and make no capability, context-window, or quality claims. The `models` section is for deliberate task-profile assignments, not for populating the live model listing.

## CLI

```text
cargo run -p itc-memory-cli -- model list --json
cargo run -p itc-memory-cli -- model profile <configured-model-id> --json
cargo run -p itc-memory-cli -- model run --profile rust-reviewer --project https-github-com-interchouette-itc-itc-memory "Review the current indexing changes."
cargo run -p itc-memory-cli -- model usage --json
```

`model run` and `model compare` can take an explicit model override from the configured or dynamically discovered catalog. When a project is specified, retrieval is bounded and uses the indexed context services. Context packs fuse exact full-text, normalized keyword, Rust-symbol, and Cargo-dependency channels; excerpts are centered on matches and mark them with `<<...>>`. Context JSON includes the selected source paths, reasons, freshness, byte count, fused relevance, and retrieval latency. For configured Ollama models, the declared `context_window` is sent as `num_ctx`; dynamically discovered models leave the provider's context setting unchanged. Model calls are bounded by the provider timeout and input-byte limit, which is not a token-count guarantee. Empty completions are recorded as failures and can fall through to the next configured model; an explicit model override does not silently choose another model.

The MCP server exposes `list_local_models`, `get_model_profile`, `ask_local_model`, `review_rust`, `analyze_architecture`, `generate_tests`, and `compare_models` over the same stdio and Streamable HTTP service. `list_local_models` returns current provider-qualified IDs for discovered models; pass the exact ID to an inference tool to select one. `review_rust` retrieves the requested checkout scope and runs Clippy before model delegation; see [MCP setup and security](mcp.md) for its inputs and execution risks. These are delegated inference tools, not autonomous coding agents; source changes remain the responsibility of the calling agent.

## Hosted providers and privacy

Hosted providers are optional. A hosted entry must use HTTPS and a named API-key environment variable, for example:

```json
{
  "id": "hosted-provider",
  "kind": "open_ai_compatible",
  "base_url": "https://models.example.com/v1",
  "hosted": true,
  "api_key_env": "ITC_MEMORY_HOSTED_API_KEY",
  "timeout_seconds": 60
}
```

Put the key value in the process environment, never in the JSON file or an MCP tool argument. `hosted` means a third-party hosted API; its discovery and inference require both `ITC_MEMORY_ALLOW_HOSTED=true` (operator consent) and per-request `allow_hosted=true` (caller consent). If either is absent, no hosted network request is made.

`ITC_MEMORY_ALLOW_HOSTED` is not required for local or explicitly trusted LAN inference. A non-loopback private endpoint must be marked `"trusted_lan": true` in the operator-managed registry; the validator accepts private IP ranges and `.local` hostnames, not arbitrary public hosts. Trusted-LAN prompts and retrieved context still travel across the LAN, so configure only a server you trust. This allows local/LAN inference and consent-gated third-party APIs to be used side by side without treating them as the same privacy boundary.

The inference audit table stores model/provider/profile IDs, hosted status, success or failure code, fallback use, elapsed time, token counts when reported, and configured estimated cost. It deliberately does not store prompts, retrieved context, or responses. A request cancelled before the provider returns is stopped but has no final usage payload and is not included in the usage aggregates. `model usage` summarizes calls made through itc-memory only; it cannot measure hosted usage billed by Copilot, Cursor, or another client outside this service.

## Provider support and limitations

- Ollama uses `/api/tags` and `/api/chat`, including optional `think` control and reported token counts. Dynamic catalog entries are based on installed model names; they are not automatically routed or capability-tested.
- OpenAI-compatible providers use `/models` and `/chat/completions`; this supports compatible local servers such as llama.cpp and optional configured hosted endpoints.
- Configure a llama.cpp provider's confirmed `/v1` base URL when that server is running.
- MCP model-tool handlers propagate request cancellation by dropping active discovery or inference futures; provider timeouts remain an independent bound.
- Provider discovery reports configured model availability and dynamically lists installed provider IDs. It does not benchmark model quality or infer effective runtime context limits.
- Hosted APIs, non-default providers, and IDE-specific model behavior have not been independently measured by this project.
