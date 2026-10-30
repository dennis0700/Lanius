[English](../container.md) | 简体中文

# 在容器 / Kubernetes 中运行 Lanius

仓库根目录的 [`Dockerfile`](../../Dockerfile) 在 `rust:1-alpine` 上把无界面网关
（`lanius`）构建为静态 musl 二进制，再复制到
`gcr.io/distroless/static-debian12:nonroot`。生成的镜像（约 11 MB）：

- 以 UID/GID `65532`（非 root）运行，不含 shell 和包管理器；
- 自带访问 Kiro HTTPS 所需的 CA 证书；
- 监听 `0.0.0.0:18000`（已预设 `SERVER_HOST`/`SERVER_PORT`）；
- `ENTRYPOINT` 为 `lanius`，因此 `docker run <image> probe "hi"` 会执行子命令；
- 收到 `SIGTERM`（`docker stop` 和 Kubernetes 发送的信号）时优雅关闭。

## 构建

```sh
docker build -t lanius .
# 多架构（arm64 在 QEMU 下构建较慢）：
docker buildx build --platform linux/amd64,linux/arm64 -t <registry>/lanius:<tag> --push .
```

## 用 Docker 运行

```sh
docker run -d --name lanius -p 18000:18000 \
  -e PROXY_API_KEY="$(openssl rand -hex 32)" \
  -e KIRO_REGION=us-east-1 \
  -e KIRO_API_KEY=ksk_xxxxxxxx \
  lanius

curl -fsS http://127.0.0.1:18000/health
```

所有配置都通过环境变量完成，见 [configuration.md](configuration.md)。在共享环境中，
请把 `PROXY_API_KEY` 和凭据放进 `--env-file`，不要写在命令行上。

## Kubernetes

[`deploy/kubernetes/lanius.yaml`](../../deploy/kubernetes/lanius.yaml) 是一个最小示例，
包含 Secret、Deployment 和 ClusterIP Service。它设置了 `runAsNonRoot`、
`readOnlyRootFilesystem`，去掉了全部 capabilities，并用 `GET /health`（无需认证）
作为 readiness 和 liveness probe。

```sh
kubectl apply -f deploy/kubernetes/lanius.yaml
kubectl port-forward svc/lanius 18000:18000
```

Lanius 只提供明文 HTTP。流量离开集群时，请在 Ingress 或 Gateway 上终止 TLS。

### 凭据

推荐从 Secret 注入 `KIRO_API_KEY`（一个或多个逗号分隔的 `ksk_...` key）。它不会刷新，
也不会写盘，因此兼容只读根文件系统和多副本。

`KIRO_CREDS_FILE` / `KIRO_CLI_DB_FILE` 也能用，但 Lanius 会把刷新后的 token 写回该文件。
Secret 卷是只读的，写入会失败（日志为 `failed to persist refreshed credentials`，不致命），
pod 重启后又会从旧 token 开始。如果必须用这种方式，请从可写的 PersistentVolume 挂载该文件、
只运行一个副本，并保留 `readOnlyRootFilesystem`（卷本身可写）。多个副本刷新同一个
refresh token 可能会互相使对方失效。

### 其他说明

- `DEBUG_MODE` 会把调试转储写到 `DEBUG_DIR`（默认是 `/home/nonroot` 下的 `debug_logs`）。
  启用 `readOnlyRootFilesystem: true` 时，请挂载一个 `emptyDir` 并让 `DEBUG_DIR` 指向它。
- `lanius update` 无法升级镜像：二进制属于 root，而进程以 UID 65532 运行。升级时请构建或拉取新镜像。
- 示例设置了 `terminationGracePeriodSeconds: 60`，让进行中的流式响应在 `SIGTERM` 后
  有时间完成。如果客户端的流更长，请调大该值。
