# `warp_local_proxy`

Fork-owned HTTP server that the Warp client points at (via `WARP_SERVER_ROOT_URL`)
instead of `https://app.warp.dev`. Warp-hosted operations stay local; AI
inference is sent only to the backend configured by the operator.

The proxy serves local identity/settings data, model choices, AI inference,
agent run restoration, orchestration events, and local agent messaging.

## Windows launcher

Use the repository launcher rather than starting `warp-oss.exe` directly:

```powershell
.\scripts\warp-local.ps1 -Profile debug
```

It reads `%USERPROFILE%\.config\warp-local\config.env`, incrementally rebuilds
both binaries, starts the proxy, waits for `/healthz`, and then launches Warp.
Pass `-KeepProxy` to leave the proxy running after Warp exits.

## Run with default backend

```bash
cargo run -p warp_local_proxy
# Listens on 127.0.0.1:8765, talks to http://localhost:3113/v1 with model gpt-5-mini.
```

## Configure for any OpenAI-compatible endpoint

```bash
# Ollama
cargo run -p warp_local_proxy -- \
  --backend-base-url http://localhost:11434/v1 \
  --backend-auth-style none \
  --default-model llama3.1

# LM Studio
cargo run -p warp_local_proxy -- \
  --backend-base-url http://localhost:1234/v1 \
  --backend-auth-style none \
  --default-model your-model-name

# Direct OpenAI
cargo run -p warp_local_proxy -- \
  --backend-base-url https://api.openai.com/v1 \
  --backend-auth-style bearer \
  --backend-api-key sk-... \
  --default-model gpt-4o-mini
```

## Configure for Azure OpenAI / Azure AI Foundry

Azure uses a different auth header (`api-key:` instead of `Authorization: Bearer`)
and requires an `api-version` query parameter. The proxy handles both when
`--backend-auth-style azure-api-key` is selected.

```bash
# Azure OpenAI (legacy deployments URL)
cargo run -p warp_local_proxy -- \
  --backend-base-url 'https://my-resource.openai.azure.com/openai/deployments/my-gpt5-deployment' \
  --backend-auth-style azure-api-key \
  --backend-api-key '<your-azure-api-key>' \
  --azure-api-version 2024-08-01-preview \
  --default-model gpt-5-mini

# Azure AI Foundry (newer OpenAI-compat endpoint)
cargo run -p warp_local_proxy -- \
  --backend-base-url 'https://my-foundry.services.ai.azure.com/openai/v1' \
  --backend-auth-style azure-api-key \
  --backend-api-key '<your-key>' \
  --azure-api-version 2025-04-01-preview \
  --default-model gpt-5-mini
```

## All knobs (CLI flag / env var)

| Flag                        | Env var                              | Default                       |
|-----------------------------|--------------------------------------|-------------------------------|
| `--bind`                    | `WARP_LOCAL_PROXY_BIND`              | `127.0.0.1:8765`              |
| `--backend-base-url`        | `WARP_LOCAL_PROXY_BACKEND`           | `http://localhost:3113/v1`    |
| `--backend-auth-style`      | `WARP_LOCAL_PROXY_AUTH_STYLE`        | `bearer`                      |
| `--backend-api-key`         | `WARP_LOCAL_PROXY_BACKEND_API_KEY`   | (unset)                       |
| `--azure-api-version`       | `WARP_LOCAL_PROXY_AZURE_API_VERSION` | (unset; only for Azure)       |
| `--default-model`           | `WARP_LOCAL_PROXY_DEFAULT_MODEL`     | `gpt-5-mini`                  |

The configured default model is always advertised to Warp. Backend `/models`
discovery is optional enrichment, is refreshed when Warp requests model
choices, and embedding-only model IDs are excluded. Custom OpenAI Chat
Completions endpoints configured in Warp settings are also resolved by the
proxy using their `config_key`, endpoint URL, API key, and provider model slug.

## Smoke check

```bash
curl http://localhost:8765/healthz
# -> {"status":"ok"}

curl -X POST http://localhost:8765/graphql/v2 \
  -H 'Content-Type: application/json' \
  -d '{"operationName":"GetUser","query":"query GetUser{...}","variables":{}}'
# -> local synthetic user data under {"data":{"user":...}}
```

## Architecture

```
+-----------------+      +--------------------+      +----------------------+
|  warp-oss       | -->  |  warp_local_proxy  | -->  |  OpenAI-compat /     |
|  (forked Warp)  |      |  (this crate)      |      |  Azure backend       |
|                 |      |                    |      |                      |
|  WARP_SERVER_   |      |  axum, port 8765   |      |  localhost:3113/v1   |
|   ROOT_URL=...  |      +--------------------+      |  (default)           |
+-----------------+                                  +----------------------+
```

The proxy holds:
1. **Stub handlers** for identity / workspace / settings / experiments / model-list ops — return canned local data.
2. **Real handlers** for `generateCommands`, `generateDialogue`, `generate_code_review_content` — call the configured AI backend.
3. **Local agent services** for run metadata, event SSE, messaging, task IDs, child-result caching, and the minimal Factory MCP protocol.
4. **Structured unsupported responses** for remaining cloud-only operations such as attachments and transcripts.

No requests reach `app.warp.dev` when the proxy is in front. The current client
still attempts Firebase once before using the proxy's local token fallback, and
the proxy intentionally contacts the configured inference backend.

## See also

- `~/.copilot/session-state/.../plan.md` — high-level fork plan.
- `~/.copilot/session-state/.../files/FORK_NOTES.md` — every patched line in the upstream tree.
