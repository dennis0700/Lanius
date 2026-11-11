English | [简体中文](README.zh-CN.md)

# Lanius

Use Kiro through OpenAI- and Anthropic-compatible APIs. Lanius lets compatible
clients, SDKs, and coding tools connect to Kiro with a local or self-hosted
gateway.

![Lanius desktop app](desktop-ui.png)

## Quick start

Choose the desktop app for local use, or the CLI for a server, Docker, and
automation.

### Desktop app

Download the package for your platform from the [GitHub Releases](https://github.com/dennis0700/lanius/releases) page, then open Lanius and:

1. Add your Kiro API key in **Settings**.
2. Keep the default address, `127.0.0.1:18000`, unless another device needs to connect.
3. Copy the connection details shown in the app into your client.

### CLI or Linux server

Download a Linux release, or build the CLI from source. Set a strong client key
and one Kiro credential source before starting. A Kiro API key is the simplest
option for servers.

```sh
export PROXY_API_KEY="$(openssl rand -hex 32)"
export KIRO_API_KEY="ksk_your_kiro_key"
export KIRO_REGION="us-east-1"
export SERVER_HOST="127.0.0.1"

lanius
```

When building from this checkout, run `cargo run -p lanius-cli` instead of
`lanius`. Building requires Rust 1.85 or later. For a persistent Linux service,
follow the [systemd deployment guide](docs/deployment.md).

### Verify it works

```sh
curl http://127.0.0.1:18000/health

curl http://127.0.0.1:18000/v1/models \
  -H "Authorization: Bearer $PROXY_API_KEY"
```

## Connect a client

Use `PROXY_API_KEY` as the API key in your client. This is the key protecting
Lanius, not your Kiro API key.

| Client | Base URL | Messages endpoint |
|---|---|---|
| OpenAI-compatible client | `http://127.0.0.1:18000/v1` | `POST /chat/completions` or `POST /responses` |
| Anthropic-compatible client | `http://127.0.0.1:18000` | `POST http://127.0.0.1:18000/v1/messages` |

For example, an OpenAI-compatible request is:

```sh
curl http://127.0.0.1:18000/v1/chat/completions \
  -H "Authorization: Bearer $PROXY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"claude-sonnet-4.6","messages":[{"role":"user","content":"Hello"}]}'
```

Lanius also supports `GET /v1/models`, `POST /v1/responses`,
`POST /v1/messages/count_tokens`, `GET /usage`, and `GET /account`. See the
[API and client guide](docs/request-flow.md) for request formats.

### Codex CLI

Codex uses the OpenAI Responses API. Configure its provider URL as
`http://127.0.0.1:18000/v1` and use `PROXY_API_KEY` through an environment
variable. The [Codex guide](docs/zh-CN/codex.md) currently covers this setup in
Chinese.

## Credentials and configuration

Set `PROXY_API_KEY` and choose one Kiro credential source:

| Setting | Use |
|---|---|
| `KIRO_API_KEY` | Recommended for servers and containers. Create a `ksk_...` key at [Kiro](https://app.kiro.dev). |
| `KIRO_CREDS_FILE` | Use an existing Kiro desktop credentials file. |
| `KIRO_CLI_DB_FILE` | Use an existing Kiro CLI login database. |
| `REFRESH_TOKEN` | Use an existing refresh token when the other sources are unavailable. |

`KIRO_REGION` must match the region for your Kiro key or account. The default
is `us-east-1`. See the [configuration reference](docs/configuration.md) for
all settings.

## Deployment

- [Linux with systemd](docs/deployment.md)
- [Docker and Kubernetes](docs/container.md)
- [Desktop app guide](docs/gui.md)

## Security

- Generate a unique, random `PROXY_API_KEY`. Do not use an example value.
- Keep Kiro credentials and `PROXY_API_KEY` out of Git, screenshots, logs, and
  issue reports. Use an environment file with restrictive permissions for a
  server.
- Bind to `127.0.0.1` when clients run locally or a reverse proxy is in front
  of Lanius. `0.0.0.0` exposes it on every network interface.
- Lanius serves HTTP. For traffic outside the host or cluster, use TLS and
  access controls at a reverse proxy, Ingress, or gateway.
- `/health` is public for health checks. All other endpoints require
  `PROXY_API_KEY`.

## Updating

The desktop app can check for updates from Settings. For the CLI, run
`lanius update`; use `lanius update --check` to check without installing.

## More documentation

Advanced setup, compatibility notes, and troubleshooting are in [docs/](docs).

## License

AGPL-3.0. See [LICENSE](LICENSE).
