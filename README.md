# codex-relay

A small, self-hosted gateway that exposes a **ChatGPT/Codex subscription** as an
**OpenAI-compatible API**. Services like Nextcloud Assistant, n8n or Open WebUI
need only a base URL and an API key to use it.

Single static Rust binary in a distroless image. It idles at about 3 MiB of memory.

> **Disclaimer.** This is an unofficial project and is not affiliated with OpenAI.
> It uses the same backend and sign-in as the Codex CLI. Using a subscription this
> way may conflict with OpenAI's terms of use. Use it at your own risk. Sharing
> one account among many people is the most likely way to get it limited.

## Features

| Endpoint | Notes |
|---|---|
| `POST /v1/chat/completions` | Streaming and non-streaming, tool calls (parallel too), image input, `response_format` (JSON object / JSON schema), `reasoning_effort` |
| `GET /v1/models` | Models available to the signed-in account, plus the image model |
| `POST /v1/images/generations` | `gpt-image-2` via the Codex image backend, `b64_json` output |
| `GET /healthz` | Sign-in state and subscription usage limits (no auth, no secrets) |

- **Sign-in built in.** `codex-relay login` uses the same device-code flow as
  `codex login --device-auth`, so it works on headless servers.
- **Token care.** The token is refreshed before it expires and at least once a
  day, so the sign-in never goes stale. If OpenAI rejects it (password change,
  sign-out, ...), the relay stops and answers `503` with the reason until you run
  `login` again. It never falls back silently. A new sign-in is picked up
  without a restart.
- **Usage limits.** The backend reports the subscription limits in every
  response. The relay shows them in `/healthz` and `codex-relay status`, logs a
  warning at 80/90/95 %, and adds the reset time to rate-limit errors.
- **Transient errors retried** like the Codex CLI does: `5xx` responses and
  connection failures get up to 4 retries (200 ms, 400 ms, 800 ms, 1.6 s). They
  come before any output, so a streamed answer is never duplicated. `429` and
  other errors pass through.
- **Client quirks absorbed.** `system` messages are sent as `developer`.
  Unsupported sampling parameters (`temperature`, `top_p`, ...) are dropped.
  `null` fields (for example Nextcloud's `"name": null` in tool messages) are ignored.
- **Named client keys** (`nextcloud:…`, `n8n:…`) so each service can be told
  apart in logs and revoked on its own.

### Backend limitations

- Image generation ignores `size`, `quality` and `n`. The backend picks the size
  itself, and only `background: transparent` is honoured. The relay makes `n`
  parallel calls (max 4). Any requested image model (`dall-e-3`,
  `gpt-image-1`, ...) is mapped to `CODEX_RELAY_IMAGE_MODEL`.
- No embeddings, audio, or legacy `functions` API.
- Usage-limit data is as fresh as the last request (the relay makes no extra calls).

## Running with Docker

```sh
cp compose.example.yaml compose.yaml        # set the network your services use
echo "nextcloud:$(openssl rand -hex 32)" > api-keys && chmod 644 api-keys
docker compose run --rm codex-relay login   # sign in to ChatGPT (device code)
docker compose up -d
docker exec codex-relay codex-relay status  # sign-in state and usage limits
```

Services on the same Docker network then use:

| Setting  | Value                         |
|----------|-------------------------------|
| Base URL | `http://codex-relay:8080/v1`  |
| API key  | the key after `name:` in `api-keys` |

`codex-relay status` prints something like:

```
codex-relay 0.1.0 — ok
ChatGPT sign-in : ready (plan pro), token refreshed 1h 1m ago
7-day window    : 35% used, resets in 5d 20h
Credits         : 62500
Observed        : 0m ago (updates with each request)
```

### Nextcloud

In *Administration settings → Artificial intelligence → OpenAI and LocalAI integration*:

- Service URL: `http://codex-relay:8080/v1` (the relay must join the `nextcloud-aio` network)
- API key: the `nextcloud:` key
- Default completion model: any chat model from the list, e.g. `gpt-6.1-terra`
- Image model: `gpt-image-2`

Assistant, Context Agent (tool calls), Context Chat, summaries, translation and
text-to-image all work through the relay.

## Configuration

| Variable                     | Default          | Description |
|------------------------------|------------------|-------------|
| `CODEX_RELAY_LISTEN`         | `0.0.0.0:8080`   | Listen address |
| `CODEX_RELAY_DATA_DIR`       | `/data`          | ChatGPT credentials (`auth.json`, mode 600) |
| `CODEX_RELAY_API_KEYS`       | —                | Comma-separated `name:key` pairs |
| `CODEX_RELAY_API_KEYS_FILE`  | —                | File with one `name:key` per line (`#` comments allowed) |
| `CODEX_RELAY_DEFAULT_REASONING_EFFORT` | — | Effort when a request sets none |
| `CODEX_RELAY_IMAGE_MODEL`    | `gpt-image-2`    | Backend image model |
| `CODEX_RELAY_UPSTREAM_URL`   | `https://chatgpt.com/backend-api/codex` | Backend base URL |
| `CODEX_RELAY_CLIENT_VERSION` | `0.166.0`        | Codex CLI version reported to the backend |
| `RUST_LOG`                   | `info`           | Log filter |

Keys must be at least 16 characters.

## Building from source

```sh
cargo build --release
./target/release/codex-relay login --data-dir ./data
./target/release/codex-relay serve --data-dir ./data --api-keys "dev:$(openssl rand -hex 16)"
```

## License

MIT
