# index-translate

基于 Rust 的本地常驻代理，为 **沉浸式翻译 (Immersive Translate)**、**陪读蛙 (Read Frog)** 等支持 OpenAI 兼容接口的工具提供 Index-Translate 翻译服务。

单可执行文件，无运行时依赖，支持前台运行、内置后台守护进程，以及一键安装为 Linux systemd 服务。

## 特性

- **OpenAI 兼容**：实现 `/v1/chat/completions`、`/v1/completions`、`/v1/responses`
- **流式透明转发**：SSE 响应逐块透传，不做缓冲，适合网页逐段翻译
- **自动参数修正**：未指定时补齐 `chat_template_kwargs.enable_thinking=false` 和 `temperature=0.0`
- **模型列表回退**：`/v1/models` 优先读取上游；上游不可用时返回内置 35B-A3B / 9B / 2B 列表
- **多模态透传**：图片 data URL 原样转发，不解析消息体
- **CORS 完整支持**：包含 `OPTIONS` 预检，浏览器扩展可直接访问
- **错误透传**：上游 HTTP 状态码与响应体原样返回；连接失败返回 `502`，超时返回 `504`
- **后台常驻**：内置 PID 锁、日志、`start/stop/restart/status`，或一键安装 systemd 服务
- **默认安全**：仅监听 `127.0.0.1`，不校验 API Key，不向上游泄漏客户端密钥

## 快速开始

### 构建

```bash
git clone https://github.com/penguinway/index-translate.git
cd index-translate
cargo build --release
```

产物：`target/release/index-translate`。

### 一键安装为 systemd 服务

```bash
# 用户服务：登录后自动运行
./target/release/index-translate install-service

# 开机即启动，无需先登录
./target/release/index-translate install-service --linger

# 系统级服务：需要 root
sudo ./target/release/index-translate install-service --system
```

安装命令会自动：

1. 复制可执行文件到稳定路径（用户级 `~/.local/bin/index-translate`，系统级 `/usr/local/bin/index-translate`）
2. 写入 unit（用户级 `~/.config/systemd/user/index-translate.service`，系统级 `/etc/systemd/system/index-translate.service`）
3. 停止已有的内置后台实例，避免端口/PID 冲突
4. 执行 `systemctl daemon-reload`、`enable`、`restart`，并等待 `/health` 就绪

### 或使用内置后台模式

```bash
./target/release/index-translate start      # 后台启动并脱离终端
./target/release/index-translate status     # 查看状态
./target/release/index-translate restart    # 重启
./target/release/index-translate stop       # 优雅停止
```

同时安装 systemd 服务与内置后台实例会触发 PID 文件锁；两者不要同时使用。

## 客户端配置

### 沉浸式翻译

翻译服务选择 **自定义 (OpenAI 兼容)**：

| 字段 | 值 |
| --- | --- |
| API URL | `http://127.0.0.1:18080/v1` |
| 模型 | `Index-Translate-35B-A3B`（或 `Index-Translate-9B` / `Index-Translate-2B`） |
| API Key | 任意非空值，例如 `index` |

### 陪读蛙

在 **选项 → API 服务商** 中添加 **OpenAI 兼容**：

| 字段 | 值 |
| --- | --- |
| Base URL | `http://127.0.0.1:18080/v1` |
| API Key | `index` |
| 模型 | `Index-Translate-35B-A3B`（或 `Index-Translate-9B` / `Index-Translate-2B`） |

> Base URL 末尾不要有空格或换行。带空格时请求会变成 `/v1%20/chat/completions`，返回 `404`。

如果选择的是 **Open Responses** 协议（不是“OpenAI 兼容”），URL 必须填写完整端点：

```text
http://127.0.0.1:18080/v1/responses
```

填成 `http://127.0.0.1:18080/v1` 会 POST 到只接受 GET 的 `/v1`，返回 `405 Method Not Allowed`。

## 服务管理

用户服务：

```bash
systemctl --user status index-translate
systemctl --user restart index-translate
systemctl --user stop index-translate
journalctl --user -u index-translate -f
```

系统服务去掉 `--user`，例如 `systemctl status index-translate`。

卸载：

```bash
./target/release/index-translate uninstall-service
# 或系统级
sudo ./target/release/index-translate uninstall-service --system
```

卸载只移除 unit，保留可执行文件和日志。

## 命令行

```bash
index-translate [OPTIONS] [COMMAND]
```

| 命令 | 说明 |
| --- | --- |
| `serve` | 前台运行（默认） |
| `start` | 启动内置后台守护进程 |
| `stop` | 停止内置后台守护进程 |
| `restart` | 重启内置后台守护进程 |
| `status` | 查看后台实例状态 |
| `install-service` / `install` | 安装并启动 systemd 服务 |
| `uninstall-service` / `uninstall` | 停止、禁用并移除 systemd 服务 |

常用参数（均为全局参数，可放在子命令前或后）：

| 参数 | 说明 |
| --- | --- |
| `--api-base <URL>` | 上游 API 根地址，默认 `https://index-translate.bilibili.com/v1`；环境变量 `INDEX_TRANSLATE_API_BASE` |
| `-H, --host <HOST>` | 监听网卡，默认 `127.0.0.1`；`0.0.0.0` 暴露到局域网 |
| `-p, --port <PORT>` | 本地端口，默认 `18080` |
| `-m, --model <MODEL>` | `/v1/models` 回退列表中的默认模型 |
| `--pid-file <PATH>` | 自定义 PID 文件 |
| `--log-file <PATH>` | 自定义后台日志文件 |
| `--log-level <LEVEL>` | `error` / `warn` / `info` / `debug` / `trace` 或 EnvFilter 指令；环境变量 `INDEX_TRANSLATE_LOG` |
| `--access-log` | 记录每个代理请求 |
| `--max-body-mib <N>` | 最大请求体，默认 `64`，范围 1–1024 |
| `--system` | 仅用于服务命令：安装/卸载系统级 unit |
| `--linger` | 仅用于 `install-service`：启用 `loginctl enable-linger` |

示例：

```bash
index-translate start --port 9000 --access-log
index-translate install-service --port 9000 --model Index-Translate-9B --linger
index-translate serve --log-level debug
```

## 默认路径

| 项目 | 默认值 |
| --- | --- |
| API URL | `http://127.0.0.1:18080/v1` |
| 上游 | `https://index-translate.bilibili.com/v1` |
| 默认模型 | `Index-Translate-35B-A3B` |
| PID（内置后台模式） | `$XDG_RUNTIME_DIR/index-translate.pid`，回退到 `~/.local/state/index-translate/index-translate.pid` |
| 日志（内置后台模式） | `$XDG_STATE_HOME/index-translate/index-translate.log`，回退到 `~/.local/state/index-translate/index-translate.log` |
| 最大请求体 | 64 MiB |

systemd 模式日志进入 journal，`serve` 的 `Ctrl-C` / `SIGINT` / `SIGTERM` 会触发优雅退出（最多等待在途请求 8 秒）并清理 PID 文件。

## API 行为

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/health` | 本地健康检查，返回服务名、版本、上游和默认模型 |
| `GET` | `/v1/models`、`/models` | 上游模型列表，失败时返回内置列表 |
| `POST` | `/v1/chat/completions` | 转发到上游 `/chat/completions` |
| `POST` | `/v1/completions` | 转发到上游 `/completions` |
| `POST` | `/v1/responses` | 转发到上游 `/responses` |
| `GET` | `/`、`/v1` | 返回就绪文本，便于探活 |

- 请求 JSON 会被解析以注入默认参数；非 JSON 请求体原样转发
- 响应状态码、`Content-Type`、缓存/重试相关头与 SSE 流原样透传
- 未知 `POST` 路径返回 `404`；未知 `GET` 路径返回就绪文本

## 开发

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo build --release
```

项目结构：

| 路径 | 说明 |
| --- | --- |
| `src/main.rs` | CLI、启动流程、内置后台进程管理 |
| `src/proxy.rs` | HTTP 路由、CORS、转发与 SSE |
| `src/upstream.rs` | 上游客户端与请求参数修正 |
| `src/daemon.rs` | PID 锁、信号、进程检测、健康探测 |
| `src/service.rs` | systemd unit 生成与安装/卸载 |
| `src/config.rs` | 配置与模型回退 |
| `tests/proxy.rs` | 代理集成测试 |
| `call_api.py` | 原始 Python 参考实现 |

## 平台要求

- Linux / Unix
- systemd（仅 `install-service` / `uninstall-service` 需要）
- Rust 1.85+（构建）
- 上游为公开免费服务，不保证可用性、速率或长期稳定性

## 安全提示

- 代理默认仅监听回环地址，自身没有鉴权；`--host 0.0.0.0` 会让同一网络内任何人都能使用并消耗上游额度。
- 客户端填写的 API Key 不会被转发到上游。
- 上游是公开免费接口，请勿用于高并发或敏感内容。

## License

[MIT](LICENSE) © 2026 penguinway
