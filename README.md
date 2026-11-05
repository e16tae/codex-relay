# codex-relay

A small, self-hosted gateway that exposes a **ChatGPT/Codex subscription** as an
**OpenAI-compatible API**, so services like Nextcloud Assistant, n8n or Open WebUI
can use it with nothing more than a base URL and an API key.

> **Status: early development.** Chat completions (text, images, JSON output,
> tool calls,
> streaming), models, ChatGPT sign-in, token refresh,
> client API keys and
> health checks and image generation work. Verified with the official OpenAI
> Python SDK.

> **Disclaimer.** This is an unofficial project and is not affiliated with OpenAI.
> It talks to the same backend the Codex CLI uses with your ChatGPT sign-in.
> Using a subscription this way may conflict with OpenAI's terms of use. Use it
> at your own risk. Sharing one account among many people is the most likely way to get it limited.

## Goals (v0.1)

- `POST /v1/chat/completions`: streaming and non-streaming, with tool calls
- `GET /v1/models`
- `POST /v1/images/generations` via the Codex image backend (`gpt-image-2`).
  The backend picks the image size itself and ignores `size`/`quality`;
  `background: transparent` works. `n > 1` runs parallel calls (max 4).
  Any requested image model name is mapped to `CODEX_RELAY_IMAGE_MODEL`.
- Automatic token refresh plus a daily keepalive. If the sign-in is revoked, the
  relay stops serving and sends an alert. It never falls back silently.
- Fixes for client quirks (for example `"name": null` in tool messages from Nextcloud)
- `/healthz` reporting token state and remaining quota
- Named client API keys (`nextcloud:…`, `n8n:…`), so each service can be told apart and revoked separately
- Distroless, non-root container image with a built-in `healthcheck` command

## Running with Docker

```sh
cp compose.example.yaml compose.yaml        # set the network your services use
echo "nextcloud:$(openssl rand -hex 32)" > api-keys && chmod 644 api-keys
docker compose run --rm codex-relay login   # sign in to ChatGPT (device code)
docker compose up -d
```

Services on the same Docker network then use:

| Setting  | Value                         |
|----------|-------------------------------|
| Base URL | `http://codex-relay:8080/v1`  |
| API key  | the key after `name:` in `api-keys` |

## Configuration

| Variable                     | Default          | Description |
|------------------------------|------------------|-------------|
| `CODEX_RELAY_LISTEN`         | `0.0.0.0:8080`   | Listen address |
| `CODEX_RELAY_DATA_DIR`       | `/data`          | ChatGPT credentials and state |
| `CODEX_RELAY_API_KEYS`       | —                | Comma-separated `name:key` pairs |
| `CODEX_RELAY_API_KEYS_FILE`  | —                | File with one `name:key` per line (`#` comments allowed) |
| `CODEX_RELAY_DEFAULT_REASONING_EFFORT` | — | Effort when a request sets none |
| `CODEX_RELAY_IMAGE_MODEL`    | `gpt-image-2`    | Backend image model |
| `RUST_LOG`                   | `info`           | Log filter |

Keys must be at least 16 characters.

## Building from source

```sh
cargo build --release
./target/release/codex-relay serve --data-dir ./data --api-keys "dev:$(openssl rand -hex 16)"
```

## License

MIT
