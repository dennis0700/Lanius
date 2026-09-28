> [English](../deployment.md) | 简体中文

# 在 Linux 上使用 systemd 部署 `lanius-cli`

本指南介绍如何在 Linux 服务器上安装无界面网关二进制程序（`lanius`），并以专用的
非特权用户身份将其作为 systemd 服务运行。

全文使用以下目录布局：

| 路径 | 内容 |
|---|---|
| `/opt/lanius/lanius` | 二进制程序 |
| `/opt/lanius/lanius.env` | 服务配置（环境变量），权限 `600` |
| `/opt/lanius/creds.json` | Kiro 凭据（使用 `KIRO_CREDS_FILE` 时），权限 `600` |
| `/etc/systemd/system/lanius.service` | systemd unit 文件 |

## 前置条件

- 运行 systemd 的 Linux，架构为 `x86_64`（amd64）或 `aarch64`（arm64）。
- root 权限（或 `sudo`）。
- 在其他地方生成的有效 Kiro 凭据（Kiro 桌面应用或 `kiro-cli login`）。Lanius 只会
  读取并刷新已有凭据，本身无法登录。详见 [auth.md](auth.md)。

## 1. 获取二进制程序

### 方式 A：使用预构建的 Release（推荐）

[Linux 构建工作流](../../.github/workflows/linux-build.yml) 会为每个版本 tag 发布
完全静态链接的 musl 二进制程序。它没有任何运行时依赖（不依赖 glibc、OpenSSL），可以
在任意发行版上运行。

```sh
VERSION=0.1.1   # 需要的版本号，不带前缀 "v"
case "$(uname -m)" in
  x86_64)        ARCH=amd64 ;;
  aarch64|arm64) ARCH=arm64 ;;
  *) echo "unsupported architecture: $(uname -m)"; exit 1 ;;
esac

NAME="lanius-${VERSION}-linux-${ARCH}"
curl -fLO "https://github.com/dennis0700/Lanius/releases/download/v${VERSION}/${NAME}.tar.gz"
tar xzf "${NAME}.tar.gz"
cd "${NAME}"    # 包含：lanius、lanius.service、lanius.env.example
```

### 方式 B：从源码构建

需要 Rust 1.85+。在服务器上执行：

```sh
git clone https://github.com/dennis0700/Lanius.git
cd Lanius
cargo build --release -p lanius-cli
# 二进制：target/release/lanius
# unit/env 模板：deploy/systemd/lanius.service、deploy/lanius.env.example
```

如需构建与 CI 相同的静态二进制（例如在另一台机器上交叉编译），先安装
[`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) 和 zig，然后：

```sh
rustup target add x86_64-unknown-linux-musl   # 或 aarch64-unknown-linux-musl
cargo zigbuild --release -p lanius-cli --target x86_64-unknown-linux-musl
# 二进制：target/x86_64-unknown-linux-musl/release/lanius
```

后续步骤假定 `lanius`、`lanius.service` 和 `lanius.env.example` 位于当前目录；
如果是从源码构建，请相应调整路径。

## 2. 安装

以 root 身份执行：

```sh
useradd --system --no-create-home --shell /usr/sbin/nologin lanius

install -d -o lanius -g lanius -m 750 /opt/lanius
install -o root -g root -m 755 lanius /opt/lanius/lanius
install -o lanius -g lanius -m 600 lanius.env.example /opt/lanius/lanius.env

# 可选：把 `lanius` 加入 PATH，方便临时执行 `lanius probe` / `lanius help`。
ln -sf /opt/lanius/lanius /usr/local/bin/lanius
```

二进制文件归 root 所有，服务账号无法替换它。

## 3. 配置

编辑 `/opt/lanius/lanius.env`，至少需要：

```sh
# 生成一个强随机的客户端密钥：
openssl rand -hex 32
```

```ini
PROXY_API_KEY=<上面生成的值>
KIRO_REGION=us-east-1
SERVER_HOST=127.0.0.1
SERVER_PORT=8000
KIRO_CREDS_FILE=/opt/lanius/creds.json
LOG_LEVEL=INFO
```

注意：

- 客户端在同一台主机上或位于反向代理之后时，使用 `SERVER_HOST=127.0.0.1`。
  `0.0.0.0` 会在所有网卡上暴露网关，只应在有防火墙保护时使用。Lanius 只提供明文
  HTTP，凡是离开本机的流量，都应在前面放一个负责 TLS 终止的反向代理（nginx、Caddy
  等）。
- `EnvironmentFile` 的语法是每行一个 `KEY=value`。不要写 `export`，包含空格的值需要
  加引号。
- 所有支持的变量见 [configuration.md](configuration.md)。

### 凭据

任选一种来源（优先级顺序见 [auth.md](auth.md)）：

- **`REFRESH_TOKEN`**（可选搭配 `PROFILE_ARN`）：直接写在 `lanius.env` 中。
- **`KIRO_CREDS_FILE`**：从已登录的机器上复制 JSON token 文件（Kiro 默认位置为
  `~/.aws/sso/cache/kiro-auth-token.json`）：

  ```sh
  install -o lanius -g lanius -m 600 kiro-auth-token.json /opt/lanius/creds.json
  ```

- **`KIRO_CLI_DB_FILE`**：指向 `kiro-cli` 的 SQLite 数据库，例如复制到
  `/opt/lanius/data.sqlite3`，所有者和权限同上。

始终使用绝对路径。`lanius` 用户没有 home 目录，`~` 无法解析到有意义的位置。

Lanius 会把刷新后的 token 写回凭据文件（JSON 文件采用原子方式重写，需要对其所在
目录有写权限）。把文件放在 `/opt/lanius` 下，默认 unit 的沙箱配置就已允许写入。如果
文件必须放在其他位置，参见 [凭据位于 `/opt/lanius` 之外](#凭据位于-optlanius-之外)。

## 4. 启用前先验证

以服务用户身份、使用与服务相同的环境变量执行一次线上探测。这样可以在 systemd 反复
重启失败进程之前，先发现凭据或配置问题：

```sh
cd /opt/lanius
sudo -u lanius sh -c 'set -a; . /opt/lanius/lanius.env; set +a; exec /opt/lanius/lanius probe "hello"'
```

务必以 `lanius` 用户而不是 root 执行。否则 token 刷新可能会让凭据文件变成 root
所有，导致服务无法读取。

## 5. 安装并启动 systemd 服务

```sh
install -o root -g root -m 644 lanius.service /etc/systemd/system/lanius.service
systemctl daemon-reload
systemctl enable --now lanius
```

检查运行状态：

```sh
systemctl status lanius
journalctl -u lanius -f                     # 跟踪日志
curl -fsS http://127.0.0.1:8000/health      # 无需认证的健康检查
curl -fsS http://127.0.0.1:8000/v1/models \
  -H "Authorization: Bearer <PROXY_API_KEY>"
```

随附的 unit 文件（[`deploy/systemd/lanius.service`](../../deploy/systemd/lanius.service)）：

- 在网络就绪后启动，失败时自动重启；
- 以 `lanius:lanius` 身份运行，启用 `NoNewPrivileges`、`ProtectSystem=strict`、
  `ProtectHome`、`PrivateTmp`，仅 `/opt/lanius` 可写；
- 使用 `SIGINT` 停止，这是 Lanius 用于优雅关闭的信号（它不处理 systemd 默认发送的
  `SIGTERM`）。

日志输出到 stdout/stderr，由 journal 收集，不需要额外的日志文件。启用 `DEBUG_MODE`
时，调试转储写入 `DEBUG_DIR`（默认 `debug_logs`），相对于 `WorkingDirectory`，即
`/opt/lanius/debug_logs`。

### 自定义 unit

使用 drop-in 而不是直接修改 unit 文件，这样升级替换 `lanius.service` 时不会丢失
你的修改：

```sh
systemctl edit lanius
```

例如提高打开文件数上限：

```ini
[Service]
LimitNOFILE=65535
```

然后执行 `systemctl restart lanius`。

#### 凭据位于 `/opt/lanius` 之外

`ProtectSystem=strict` 会让整个文件系统只读，`ProtectHome=true` 会隐藏 `/home`、
`/root` 和 `/run/user`。如果 `KIRO_CREDS_FILE`/`KIRO_CLI_DB_FILE` 指向其他位置，
需要在 drop-in 中放行其所在目录（服务用户同时需要对其有正常的 Unix 权限）：

```ini
[Service]
ReadWritePaths=/srv/kiro
```

如果文件位于 `/home` 下，还需加上 `ProtectHome=false`（或 `ProtectHome=read-only`
配合 `ReadWritePaths=`）。通常把文件复制到 `/opt/lanius` 更简单。

#### 绑定 1024 以下的端口

保持非特权用户，只授予绑定端口的能力：

```ini
[Service]
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
```

## 升级

### 方式 A：`lanius update`（推荐）

二进制程序自带在线更新能力：

```sh
sudo lanius update           # 检查 GitHub Releases，确认后安装
sudo lanius update --check   # 只检查是否有新版本，不安装
sudo lanius update -y        # 跳过确认提示（例如在脚本中使用）
```

`update` 会从 GitHub 下载对应平台的发布包，用内置的 `minisign` 公钥（见
`deploy/lanius-release.pub`）验证签名，验证通过后原子替换正在运行的二进制文件；
如果签名不匹配，或 `/opt/lanius` 不可写，会直接拒绝执行。它不会修改
`lanius.service` 或 `lanius.env`，也**不会**自动重启服务，更新后需手动执行
`sudo systemctl restart lanius`。旧的二进制会保留一份备份在
`/opt/lanius/lanius.old`。在容器内运行时会拒绝更新，因为更新在容器重启后不会保留——
请改为重新构建或拉取镜像。

### 方式 B：手动升级

```sh
# 在新版本解压后的目录中执行：
install -o root -g root -m 755 lanius /opt/lanius/lanius.new
systemctl stop lanius
mv /opt/lanius/lanius /opt/lanius/lanius.old
mv /opt/lanius/lanius.new /opt/lanius/lanius
systemctl start lanius
systemctl status lanius
```

如果新版本修改了 `lanius.service`，在启动前先复制过去并执行
`systemctl daemon-reload`。如需回滚，把 `lanius.old` 移回原位并重启即可。

## 卸载

以下操作会删除服务、二进制程序，**以及** `/opt/lanius` 中的配置和凭据：

```sh
systemctl disable --now lanius
rm /etc/systemd/system/lanius.service
rm -rf /etc/systemd/system/lanius.service.d
systemctl daemon-reload
rm -f /usr/local/bin/lanius
rm -rf /opt/lanius
userdel lanius
```

## 故障排查

| 现象 | 可能原因 |
|---|---|
| `status=203/EXEC` | 二进制架构不匹配、缺少执行权限，或 `/opt` 以 `noexec` 挂载。用 `file /opt/lanius/lanius` 检查。 |
| `status=217/USER` | `lanius` 用户不存在。执行 `useradd` 那一步。 |
| 立即退出，日志提到 `PROXY_API_KEY` | 配置校验失败。该密钥为必填项且不能为空。 |
| 凭据文件报 `Permission denied` / `Read-only file system` | 文件不属于 `lanius`，或不在 `ReadWritePaths` 内。参见 [凭据位于 `/opt/lanius` 之外](#凭据位于-optlanius-之外)。 |
| `Address already in use` | 其他进程占用了 `SERVER_PORT`。用 `ss -ltnp` 查找。 |
| 日志中出现上游 401/403 | 凭据已过期或被吊销。在其他机器重新登录后复制新文件。 |
| 其他主机无法访问 | `SERVER_HOST=127.0.0.1`，或防火墙拦截了端口。 |

用 `journalctl -u lanius -n 200 --no-pager` 查看最近日志。在 `lanius.env` 中设置
`LOG_LEVEL=DEBUG` 并重启可获得更详细的信息。
