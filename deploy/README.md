# Deployment files

Templates bundled into each Linux release archive
(`lanius-<version>-linux-<arch>.tar.gz`, built by
[`.github/workflows/linux-build.yml`](../.github/workflows/linux-build.yml)):

| File | Purpose |
|---|---|
| [`systemd/lanius.service`](systemd/lanius.service) | Hardened systemd unit running `/opt/lanius/lanius` as the `lanius` user |
| [`lanius.env.example`](lanius.env.example) | Template for `/opt/lanius/lanius.env` |

For the full install, configuration, upgrade, and troubleshooting guide, see
[`docs/deployment.md`](../docs/deployment.md)
([简体中文](../docs/zh-CN/deployment.md)).
