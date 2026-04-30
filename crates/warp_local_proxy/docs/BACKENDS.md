# Connecting `warp_local_proxy` to different AI backends

This guide covers every supported backend shape: how to start the proxy, what
flags to set, and how to point the Warp client at it.

---

## 1. Quick start (default — your local gateway)

```bash
# Terminal 1: start the proxy with defaults
cd ~/projects/warp
cargo run -p warp_local_proxy

# Terminal 2: point the warp-oss build at it
export WARP_SERVER_ROOT_URL=http://127.0.0.1:8765
./target/release/warp-oss
```

Defaults are tuned for the dev VM:

| Setting              | Default                       |
|----------------------|-------------------------------|
| Bind                 | `127.0.0.1:8765`              |
| Backend base URL     | `http://localhost:3113/v1`    |
| Backend auth style   | `bearer`                      |
| Default model        | `gpt-5-mini`                  |

---

## 2. Backend recipes

The same proxy supports any OpenAI-compatible HTTP API plus Azure OpenAI and
Azure AI Foundry. Pick one column.

### 2.1 Bearer-style backends

These send `Authorization: Bearer <key>` to the backend. Use this for OpenAI
itself, GitHub Copilot proxies (like the user's `:3113`), and most hosted
gateways.

| Backend          | Base URL                              | Auth                  | Model id (`--default-model`)        |
|------------------|---------------------------------------|-----------------------|--------------------------------------|
| Local gateway    | `http://localhost:3113/v1`            | `bearer` (key opt.)   | `gpt-5-mini`                         |
| LiteLLM proxy    | `http://localhost:8000/v1`            | `bearer` (key opt.)   | depends on your LiteLLM config       |
| OpenAI direct    | `https://api.openai.com/v1`           | `bearer` (key req'd)  | `gpt-4o-mini`, `gpt-5-mini`, etc.    |
| OpenRouter       | `https://openrouter.ai/api/v1`        | `bearer` (key req'd)  | `anthropic/claude-sonnet-4.5`, etc.  |
| DeepInfra        | `https://api.deepinfra.com/v1/openai` | `bearer` (key req'd)  | `meta-llama/Meta-Llama-3.1-70B-Instruct` |
| Together         | `https://api.together.xyz/v1`         | `bearer` (key req'd)  | provider-specific                    |
| Groq             | `https://api.groq.com/openai/v1`      | `bearer` (key req'd)  | `llama-3.3-70b-versatile`            |
| Fireworks        | `https://api.fireworks.ai/inference/v1` | `bearer`            | provider-specific                    |

```bash
# OpenAI direct
cargo run -p warp_local_proxy -- \
  --backend-base-url https://api.openai.com/v1 \
  --backend-auth-style bearer \
  --backend-api-key sk-... \
  --default-model gpt-4o-mini
```

### 2.2 No-auth local backends

Most local model servers run without auth on `localhost`.

| Backend     | Base URL                          | Auth       | Model id                             |
|-------------|-----------------------------------|------------|--------------------------------------|
| Ollama      | `http://localhost:11434/v1`       | `none`     | `llama3.1`, `qwen2.5`, etc.          |
| LM Studio   | `http://localhost:1234/v1`        | `none`     | model id from the LM Studio UI       |
| vLLM        | `http://localhost:8000/v1`        | `none`     | the model id served by vLLM          |
| llama.cpp   | `http://localhost:8080/v1`        | `none`     | usually `gpt-3.5-turbo` (alias)      |
| llamafile   | `http://localhost:8080/v1`        | `none`     | usually `gpt-3.5-turbo`              |
| KoboldAI    | `http://localhost:5001/v1`        | `none`     | currently-loaded model               |

```bash
# Ollama
cargo run -p warp_local_proxy -- \
  --backend-base-url http://localhost:11434/v1 \
  --backend-auth-style none \
  --default-model llama3.1
```

### 2.3 Azure OpenAI (legacy deployments URL)

Azure uses a `api-key:` header (not `Authorization: Bearer`) and requires an
`api-version` query parameter on every request. The proxy handles both when
`--backend-auth-style azure-api-key` is selected.

```bash
cargo run -p warp_local_proxy -- \
  --backend-base-url 'https://my-resource.openai.azure.com/openai/deployments/my-gpt5-deployment' \
  --backend-auth-style azure-api-key \
  --backend-api-key '<your-azure-api-key>' \
  --azure-api-version 2024-08-01-preview \
  --default-model gpt-5-mini
```

The proxy will POST to:

```
https://my-resource.openai.azure.com/openai/deployments/my-gpt5-deployment/chat/completions?api-version=2024-08-01-preview
```

Common Azure `api-version` values:
- `2024-02-15-preview`
- `2024-06-01`
- `2024-08-01-preview`
- `2024-10-21`
- `2025-01-01-preview`
- `2025-04-01-preview`

Pick the one your deployment supports. If you get `Unsupported API version`,
update this flag.

### 2.4 Azure AI Foundry (newer OpenAI-compat endpoint)

Foundry exposes an OpenAI-compatible base URL like
`https://<endpoint>.services.ai.azure.com/openai/v1`. Auth and `api-version`
work the same as Azure OpenAI.

```bash
cargo run -p warp_local_proxy -- \
  --backend-base-url 'https://my-foundry.services.ai.azure.com/openai/v1' \
  --backend-auth-style azure-api-key \
  --backend-api-key '<your-key>' \
  --azure-api-version 2025-04-01-preview \
  --default-model gpt-5-mini
```

### 2.5 GitHub Copilot / GitHub Models

If you're proxying GitHub Models (or a Copilot-style gateway):

```bash
cargo run -p warp_local_proxy -- \
  --backend-base-url https://models.inference.ai.azure.com \
  --backend-auth-style bearer \
  --backend-api-key '<github-pat-with-models-scope>' \
  --default-model gpt-4o-mini
```

### 2.6 Mixing — point each user at their own backend

Run multiple proxy instances on different ports:

```bash
# user 1 — direct OpenAI
WARP_LOCAL_PROXY_BIND=127.0.0.1:8765 \
WARP_LOCAL_PROXY_BACKEND=https://api.openai.com/v1 \
WARP_LOCAL_PROXY_BACKEND_API_KEY=sk-... \
cargo run -p warp_local_proxy &

# user 2 — local Ollama
WARP_LOCAL_PROXY_BIND=127.0.0.1:8766 \
WARP_LOCAL_PROXY_BACKEND=http://localhost:11434/v1 \
WARP_LOCAL_PROXY_AUTH_STYLE=none \
WARP_LOCAL_PROXY_DEFAULT_MODEL=llama3.1 \
cargo run -p warp_local_proxy &

# Each user sets WARP_SERVER_ROOT_URL appropriately.
```

---

## 3. Pointing the Warp client at the proxy

Once `Channel::allows_server_url_overrides()` includes `Oss` (it does in this
fork), the OSS binary honors `WARP_SERVER_ROOT_URL`:

```bash
export WARP_SERVER_ROOT_URL=http://127.0.0.1:8765
~/projects/warp/target/release/warp-oss
```

You can also set it in your shell rc, your systemd `--user` unit, or pass it
inline:

```bash
WARP_SERVER_ROOT_URL=http://127.0.0.1:8765 ./target/release/warp-oss
```

### Verifying the redirect took effect

The proxy logs every GraphQL operation it receives. Tail the proxy stdout —
if you see `INFO graphql request received operation=...` lines on launch, the
client is talking to the proxy.

If the proxy logs nothing when you launch `warp-oss`, double-check:
1. `Channel::Oss` is in `allows_server_url_overrides()` (search `crates/warp_core/src/channel/mod.rs`).
2. The env var is exported in the same shell that launches `warp-oss`.
3. The proxy is bound to the address in `WARP_SERVER_ROOT_URL`.

---

## 4. Persistent setup with `systemd --user`

```ini
# ~/.config/systemd/user/warp-local-proxy.service
[Unit]
Description=Warp local proxy for AI / identity / settings ops
After=network.target

[Service]
Environment=WARP_LOCAL_PROXY_BIND=127.0.0.1:8765
Environment=WARP_LOCAL_PROXY_BACKEND=http://localhost:3113/v1
Environment=WARP_LOCAL_PROXY_AUTH_STYLE=bearer
Environment=WARP_LOCAL_PROXY_DEFAULT_MODEL=gpt-5-mini
ExecStart=%h/.cargo/bin/warp-local-proxy
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now warp-local-proxy
systemctl --user status warp-local-proxy
```

Add `WARP_SERVER_ROOT_URL=http://127.0.0.1:8765` to your shell rc so every Warp
invocation picks up the redirect.

---

## 5. All flags / env vars (full reference)

| Flag                        | Env var                              | Default                      | Notes                                                |
|-----------------------------|--------------------------------------|------------------------------|------------------------------------------------------|
| `--bind`                    | `WARP_LOCAL_PROXY_BIND`              | `127.0.0.1:8765`             | Address the proxy listens on                         |
| `--backend-base-url`        | `WARP_LOCAL_PROXY_BACKEND`           | `http://localhost:3113/v1`   | Base URL; proxy appends `/chat/completions`, `/models` |
| `--backend-auth-style`      | `WARP_LOCAL_PROXY_AUTH_STYLE`        | `bearer`                     | `bearer` / `azure-api-key` / `none`                  |
| `--backend-api-key`         | `WARP_LOCAL_PROXY_BACKEND_API_KEY`   | (unset)                      | Sent per the chosen auth style                       |
| `--azure-api-version`       | `WARP_LOCAL_PROXY_AZURE_API_VERSION` | (unset)                      | Azure-only; appended as `?api-version=...`           |
| `--default-model`           | `WARP_LOCAL_PROXY_DEFAULT_MODEL`     | `gpt-5-mini`                 | Used when an op doesn't name a specific model        |

---

## 6. Troubleshooting

**Client launches but everything is broken / blank screen.**
Almost always the canned identity / workspace / settings ops returning the
wrong shape. Check the proxy log for which operations the client requested,
compare against the cynic types in `crates/graphql/src/api/`.

**Proxy logs `LOCAL_PROXY_UNIMPLEMENTED operation='X'`.**
We haven't added a handler for `X` yet. Add one in
`crates/warp_local_proxy/src/operations/`. The cynic Rust type at
`crates/graphql/src/api/{queries|mutations}/X.rs` is the source of truth for
the response shape.

**`AppChatReverse: Chat failed, 401` from the backend.**
Your backend's upstream auth has expired (e.g. Grok cookies, Copilot session).
Re-auth on the backend side. The proxy itself is fine.

**Azure returns `Unsupported API version`.**
Update `--azure-api-version` to one your deployment lists.

**Ollama / LM Studio returns `model not found`.**
Run `curl http://localhost:11434/v1/models` and pick an `id` from the list.
Pass it as `--default-model`.
