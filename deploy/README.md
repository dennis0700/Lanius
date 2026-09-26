# Deploying `lanius-cli` on Linux (systemd)

CI (`.github/workflows/linux-build.yml`) cross-compiles `lanius-cli`
(binary name `lanius`) for Linux using [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild),
targeting:

- `aarch64-unknown-linux-musl` (arm64 servers)
- `x86_64-unknown-linux-musl` (amd64 servers)

Both targets produce a single **fully static** binary (no glibc/OpenSSL/etc.
runtime dependency — verified with `file`, which reports "statically
linked"), so it can be copied to any matching-architecture Linux host and
run directly, without installing anything else.

The workflow runs on version tags (`v*.*.*`) or manual dispatch, and
uploads a `lanius-<version>-linux-<arch>.tar.gz` archive (containing the
binary, `lanius.service`, and `lanius.env.example`) as both a build
artifact and a GitHub Release asset.

## Install

```sh
# On the target server, as root:
tar xzf lanius-<version>-linux-<arch>.tar.gz
cd lanius-<version>-linux-<arch>

useradd --system --no-create-home --shell /usr/sbin/nologin lanius
mkdir -p /opt/lanius
cp lanius /opt/lanius/
cp lanius.env.example /opt/lanius/lanius.env   # then edit with real values
chmod 600 /opt/lanius/lanius.env
chown -R lanius:lanius /opt/lanius

cp lanius.service /etc/systemd/system/lanius.service
systemctl daemon-reload
systemctl enable --now lanius
systemctl status lanius
journalctl -u lanius -f
```

## Credentials

`lanius` doesn't create or log into any account itself — `KIRO_CREDS_FILE`
/ `KIRO_CLI_DB_FILE` in `lanius.env` must point at a credentials file that
already exists, produced elsewhere (the Kiro desktop app, or `kiro-cli
login`). There's no single official path for it; the `~/...` paths in
`lanius.env.example` are just `kiro-cli`'s own default locations, listed
for reference — you can point at any path.

Because the `lanius` systemd user is created with `--no-create-home`, it
has no real `$HOME`, so `~` expansion is unreliable in this context. Use
an absolute path instead, e.g. copy the file to `/opt/lanius/creds.json`
(owned by `lanius:lanius`, mode `600`) and set:

```
KIRO_CREDS_FILE=/opt/lanius/creds.json
```

## Upgrading

```sh
systemctl stop lanius
cp lanius /opt/lanius/lanius   # overwrite with the new binary
systemctl start lanius
```

See [`docs/configuration.md`](../docs/configuration.md) for every
environment variable `lanius.env` can set.
