English | [简体中文](zh-CN/container.md)

# Running Lanius in a container / Kubernetes

The repo-root [`Dockerfile`](../Dockerfile) builds the headless gateway
(`lanius`) as a static musl binary on `rust:1-alpine` and copies it into
`gcr.io/distroless/static-debian12:nonroot`. The resulting image (about 11 MB):

- runs as UID/GID `65532` (non-root), with no shell or package manager;
- includes CA certificates for HTTPS to Kiro;
- listens on `0.0.0.0:18000` (`SERVER_HOST`/`SERVER_PORT` are preset);
- uses `lanius` as the `ENTRYPOINT`, so `docker run <image> probe "hi"` runs a
  subcommand;
- shuts down gracefully on `SIGTERM` (what `docker stop` and Kubernetes send).

## Build

```sh
docker build -t lanius .
# Multi-arch (arm64 builds under QEMU are slow):
docker buildx build --platform linux/amd64,linux/arm64 -t <registry>/lanius:<tag> --push .
```

## Run with Docker

```sh
docker run -d --name lanius -p 18000:18000 \
  -e PROXY_API_KEY="$(openssl rand -hex 32)" \
  -e KIRO_REGION=us-east-1 \
  -e KIRO_API_KEY=ksk_xxxxxxxx \
  lanius

curl -fsS http://127.0.0.1:18000/health
```

All configuration is through environment variables; see
[configuration.md](configuration.md). Put `PROXY_API_KEY` and credentials in
an `--env-file` rather than on the command line in shared environments.

## Kubernetes

[`deploy/kubernetes/lanius.yaml`](../deploy/kubernetes/lanius.yaml) is a
minimal example with a Secret, Deployment, and ClusterIP Service. It sets
`runAsNonRoot`, `readOnlyRootFilesystem`, drops all capabilities, and uses
`GET /health` (unauthenticated) for readiness and liveness probes.

```sh
kubectl apply -f deploy/kubernetes/lanius.yaml
kubectl port-forward svc/lanius 18000:18000
```

Lanius serves plain HTTP. Terminate TLS at an Ingress or Gateway if traffic
leaves the cluster.

### Credentials

Use `KIRO_API_KEY` (one or more comma-separated `ksk_...` keys) from a Secret.
It is never refreshed or written to disk, so it works with a read-only root
filesystem and with more than one replica.

`KIRO_CREDS_FILE` / `KIRO_CLI_DB_FILE` also work, but Lanius writes refreshed
tokens back to that file. A Secret volume is read-only, so the write fails
(logged as `failed to persist refreshed credentials`, not fatal) and the next
pod restart starts from the stale token. If you need this path, mount the file
from a writable PersistentVolume, run a single replica, and keep
`readOnlyRootFilesystem` (the volume itself is writable). Several replicas
refreshing the same refresh token can invalidate each other.

### Other notes

- `DEBUG_MODE` writes dumps to `DEBUG_DIR` (default `debug_logs` under
  `/home/nonroot`). With `readOnlyRootFilesystem: true`, mount an `emptyDir`
  and point `DEBUG_DIR` at it.
- `lanius update` can't upgrade the image: the binary is root-owned and the
  process runs as UID 65532. Upgrade by building or pulling a new image.
- The example sets `terminationGracePeriodSeconds: 60` so streaming responses
  in flight can finish after `SIGTERM`. Raise it if your clients run longer
  streams.
