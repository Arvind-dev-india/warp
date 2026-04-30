# `warp_local_proxy`

Fork-owned HTTP server that the Warp client points at (via `WARP_SERVER_ROOT_URL`)
instead of `https://app.warp.dev`. Goal: handle every operation locally so no
traffic leaves the box.

> **Status:** Skeleton. v0 serves a healthz endpoint and a GraphQL stub that
> answers every operation with a structured "not implemented yet" error.
> Per-operation handlers (canned identity, models list, real AI inference) land
> in subsequent commits.

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

## Smoke check

```bash
curl http://localhost:8765/healthz
# -> {"status":"ok"}

curl -X POST http://localhost:8765/graphql/v2 \
  -H 'Content-Type: application/json' \
  -d '{"operationName":"GetUser","query":"query GetUser{...}","variables":{}}'
# -> {"errors":[{"message":"... GraphQL operation 'GetUser' is not implemented yet. ...","extensions":{"code":"LOCAL_PROXY_UNIMPLEMENTED","operation":"GetUser"}}]}
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
3. **503 handlers** for cloud-only ops (cloud agents, Drive, attachments).

No requests reach `app.warp.dev` or any other Warp-hosted endpoint when the
proxy is in front.

## See also

- `~/.copilot/session-state/.../plan.md` — high-level fork plan.
- `~/.copilot/session-state/.../files/FORK_NOTES.md` — every patched line in the upstream tree.
