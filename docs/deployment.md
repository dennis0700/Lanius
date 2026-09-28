English | [简体中文](zh-CN/deployment.md)

# Deploying `lanius-cli` on Linux with systemd

This guide installs the headless gateway binary (`lanius`) on a Linux server
and runs it as a systemd service under a dedicated unprivileged user.

Layout used throughout:

| Path | Contents |
|---|---|
| `/opt/lanius/lanius` | The binary |
| `/opt/lanius/lanius.env` | Service configuration (environment variables), mode `600` |
| `/opt/lanius/creds.json` | Kiro credentials (if using `KIRO_CREDS_FILE`), mode `600` |
| `/etc/systemd/system/lanius.service` | systemd unit |

## Prerequisites

- Linux on `x86_64` (amd64) or `aarch64` (arm64), with systemd.
- Root access (or `sudo`).
- Valid Kiro credentials produced elsewhere (Kiro desktop app or
  `kiro-cli login`). Lanius only reads and refreshes existing credentials; it
  cannot log in by itself. See [auth.md](auth.md).

## 1. Get the binary

### Option A: prebuilt release (recommended)

The [Linux build workflow](../.github/workflows/linux-build.yml) publishes a
fully static musl binary for each version tag. It has no runtime dependencies
(no glibc, no OpenSSL), so it runs on any distribution.

```sh
VERSION=0.1.1   # the release you want, without the leading "v"
case "$(uname -m)" in
  x86_64)        ARCH=amd64 ;;
  aarch64|arm64) ARCH=arm64 ;;
  *) echo "unsupported architecture: $(uname -m)"; exit 1 ;;
esac

NAME="lanius-${VERSION}-linux-${ARCH}"
curl -fLO "https://github.com/dennis0700/Lanius/releases/download/v${VERSION}/${NAME}.tar.gz"
tar xzf "${NAME}.tar.gz"
cd "${NAME}"    # contains: lanius, lanius.service, lanius.env.example
```

### Option B: build from source

Requires Rust 1.85+. On the server itself:

```sh
git clone https://github.com/dennis0700/Lanius.git
cd Lanius
cargo build --release -p lanius-cli
# binary: target/release/lanius
# unit/env templates: deploy/systemd/lanius.service, deploy/lanius.env.example
```

To produce the same static binary as CI (e.g. cross-compiling from another
machine), install [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild)
and zig, then:

```sh
rustup target add x86_64-unknown-linux-musl   # or aarch64-unknown-linux-musl
cargo zigbuild --release -p lanius-cli --target x86_64-unknown-linux-musl
# binary: target/x86_64-unknown-linux-musl/release/lanius
```

The remaining steps assume `lanius`, `lanius.service`, and
`lanius.env.example` are in the current directory; adjust paths if you built
from source.

## 2. Install

Run as root:

```sh
useradd --system --no-create-home --shell /usr/sbin/nologin lanius

install -d -o lanius -g lanius -m 750 /opt/lanius
install -o root -g root -m 755 lanius /opt/lanius/lanius
install -o lanius -g lanius -m 600 lanius.env.example /opt/lanius/lanius.env

# Optional: put `lanius` on PATH for ad-hoc `lanius probe` / `lanius help`.
ln -sf /opt/lanius/lanius /usr/local/bin/lanius
```

The binary is owned by root so the service account cannot replace it.

## 3. Configure

Edit `/opt/lanius/lanius.env`. At minimum:

```sh
# Generate a strong client key:
openssl rand -hex 32
```

```ini
PROXY_API_KEY=<the value generated above>
KIRO_REGION=us-east-1
SERVER_HOST=127.0.0.1
SERVER_PORT=8000
KIRO_CREDS_FILE=/opt/lanius/creds.json
LOG_LEVEL=INFO
```

Notes:

- Use `SERVER_HOST=127.0.0.1` when clients run on the same host or behind a
  reverse proxy. `0.0.0.0` exposes the gateway on every interface; only do
  that with a firewall in front of it. Lanius serves plain HTTP, so put a TLS
  terminating reverse proxy (nginx, Caddy, etc.) in front of it for any
  traffic that leaves the host.
- `EnvironmentFile` syntax is `KEY=value`, one per line. Don't `export`, and
  quote values that contain spaces.
- Every supported variable is listed in [configuration.md](configuration.md).

### Credentials

Pick one source (the precedence order is described in [auth.md](auth.md)):

- **`REFRESH_TOKEN`** (plus optional `PROFILE_ARN`) set directly in
  `lanius.env`.
- **`KIRO_CREDS_FILE`**: copy the JSON token file from a machine where you're
  already logged in (the Kiro default is
  `~/.aws/sso/cache/kiro-auth-token.json`):

  ```sh
  install -o lanius -g lanius -m 600 kiro-auth-token.json /opt/lanius/creds.json
  ```

- **`KIRO_CLI_DB_FILE`**: point at a `kiro-cli` SQLite database, e.g. copy it
  to `/opt/lanius/data.sqlite3` with the same ownership and mode.

Always use absolute paths. The `lanius` user has no home directory, so `~`
won't resolve to anything useful.

Lanius writes refreshed tokens back to the credentials file (the JSON file is
rewritten atomically, which needs write access to its directory). Keeping it
under `/opt/lanius` means the default unit's sandbox already allows this. If
the file must live elsewhere, see
[Credentials outside `/opt/lanius`](#credentials-outside-optlanius).

## 4. Verify before enabling

Run a live probe as the service user with the same environment the service
will use. This catches bad credentials or config before systemd starts
restarting a failing process:

```sh
cd /opt/lanius
sudo -u lanius sh -c 'set -a; . /opt/lanius/lanius.env; set +a; exec /opt/lanius/lanius probe "hello"'
```

Run it as `lanius`, not root. Otherwise a token refresh can leave the
credentials file owned by root, and the service won't be able to read it.

## 5. Install and start the systemd service

```sh
install -o root -g root -m 644 lanius.service /etc/systemd/system/lanius.service
systemctl daemon-reload
systemctl enable --now lanius
```

Check it:

```sh
systemctl status lanius
journalctl -u lanius -f                     # follow logs
curl -fsS http://127.0.0.1:8000/health      # unauthenticated health check
curl -fsS http://127.0.0.1:8000/v1/models \
  -H "Authorization: Bearer <PROXY_API_KEY>"
```

The shipped unit ([`deploy/systemd/lanius.service`](../deploy/systemd/lanius.service)):

- starts after the network is online and restarts on failure;
- runs as `lanius:lanius` with `NoNewPrivileges`, `ProtectSystem=strict`,
  `ProtectHome`, `PrivateTmp`, and only `/opt/lanius` writable;
- stops with `SIGINT`, the signal Lanius handles for graceful shutdown (it
  doesn't handle systemd's default `SIGTERM`).

Logs go to stdout/stderr and end up in the journal. No log files are needed.
With `DEBUG_MODE` enabled, debug dumps go to `DEBUG_DIR` (default
`debug_logs`), relative to `WorkingDirectory`, so `/opt/lanius/debug_logs`.

### Customizing the unit

Use a drop-in instead of editing the unit file, so upgrades that replace
`lanius.service` keep your changes:

```sh
systemctl edit lanius
```

For example, to raise the open-file limit:

```ini
[Service]
LimitNOFILE=65535
```

Then `systemctl restart lanius`.

#### Credentials outside `/opt/lanius`

`ProtectSystem=strict` makes the whole filesystem read-only and
`ProtectHome=true` hides `/home`, `/root`, and `/run/user`. If
`KIRO_CREDS_FILE`/`KIRO_CLI_DB_FILE` points elsewhere, allow its directory in
a drop-in (the service user also needs normal Unix permissions on it):

```ini
[Service]
ReadWritePaths=/srv/kiro
```

For a file under `/home`, also add `ProtectHome=false` (or `ProtectHome=read-only`
together with `ReadWritePaths=`). Copying the file into `/opt/lanius` is
usually simpler.

#### Binding to a port below 1024

Keep the unprivileged user and grant only the bind capability:

```ini
[Service]
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
```

## Upgrading

### Option A: `lanius update` (recommended)

The binary can update itself in place:

```sh
sudo lanius update           # checks GitHub Releases, prompts, then installs
sudo lanius update --check   # only report whether a newer version exists
sudo lanius update -y        # skip the confirmation prompt (e.g. in scripts)
```

`update` downloads the release archive for your platform from GitHub,
verifies it against a `minisign` signature embedded in the binary (see
`deploy/lanius-release.pub`), and atomically replaces the running binary —
refusing to proceed if the signature doesn't match or `/opt/lanius` isn't
writable. It never touches `lanius.service` or `lanius.env`. It does *not*
restart the service; run `sudo systemctl restart lanius` afterward. A
backup of the previous binary is kept at `/opt/lanius/lanius.old`. Running
inside a container is refused, since the update would not survive a
restart there — rebuild or re-pull the image instead.

### Option B: manual

```sh
# In the extracted directory of the new release:
install -o root -g root -m 755 lanius /opt/lanius/lanius.new
systemctl stop lanius
mv /opt/lanius/lanius /opt/lanius/lanius.old
mv /opt/lanius/lanius.new /opt/lanius/lanius
systemctl start lanius
systemctl status lanius
```

If the new release changes `lanius.service`, copy it over and run
`systemctl daemon-reload` before starting. To roll back, move `lanius.old`
back into place and restart.

## Uninstalling

This removes the service, the binary, **and the configuration and
credentials** in `/opt/lanius`:

```sh
systemctl disable --now lanius
rm /etc/systemd/system/lanius.service
rm -rf /etc/systemd/system/lanius.service.d
systemctl daemon-reload
rm -f /usr/local/bin/lanius
rm -rf /opt/lanius
userdel lanius
```

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `status=203/EXEC` | Wrong architecture binary, missing execute bit, or `/opt` mounted `noexec`. Check with `file /opt/lanius/lanius`. |
| `status=217/USER` | The `lanius` user doesn't exist. Run the `useradd` step. |
| Exits immediately, log mentions `PROXY_API_KEY` | Config validation failed. The key is required and can't be empty. |
| `Permission denied` / `Read-only file system` on the credentials file | File isn't owned by `lanius`, or it sits outside `ReadWritePaths`. See [Credentials outside `/opt/lanius`](#credentials-outside-optlanius). |
| `Address already in use` | Another process owns `SERVER_PORT`. Find it with `ss -ltnp`. |
| Upstream 401/403 in logs | Credentials expired or were revoked. Log in again elsewhere and copy the new file. |
| Not reachable from other hosts | `SERVER_HOST=127.0.0.1`, or a firewall is blocking the port. |

Use `journalctl -u lanius -n 200 --no-pager` for recent logs. Set
`LOG_LEVEL=DEBUG` in `lanius.env` and restart for more detail.
