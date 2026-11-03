# index-translate

用 Rust 编写的本地常驻代理，为 **沉浸式翻译 (Immersive Translate)** 暴露一个 OpenAI 兼容接口，请求转发到免费的 Index-Translate 公共 API。

行为对齐 `call_api.py --serve`：

- `GET /v1/models`：优先读取上游模型列表，上游不可用时返回内置列表（35B-A3B / 9B / 2B）
- `POST /v1/chat/completions`、`/v1/completions`、`/v1/responses`：原样转发到上游同名端点
- 自动补齐 `chat_template_kwargs.enable_thinking=false` 和 `temperature=0.0`
- SSE 流式响应逐块透传，不做缓冲
- 完整 CORS 支持（含 `OPTIONS` 预检）
- 上游 HTTP 错误状态码与响应体原样返回；连接失败返回 `502`，超时返回 `504`

## 构建

```bash
cargo build --release
install -Dm755 target/release/index-translate ~/.local/bin/index-translate
```

仅支持 Unix/Linux（后台化使用 `setsid` + PID 文件锁）。

## 内置后台模式（不使用 systemd）

```bash
index-translate start      # 后台启动并脱离终端
index-translate status     # 查看运行状态
index-translate restart    # 重启
index-translate stop       # 优雅停止 (SIGTERM)
```

默认值：

| 项目 | 默认值 |
| --- | --- |
| 监听地址 | `127.0.0.1:18080` |
| API URL | `http://127.0.0.1:18080/v1` |
| 默认模型 | `Index-Translate-35B-A3B` |
| PID 文件 | `$XDG_RUNTIME_DIR/index-translate.pid`，回退到 `~/.local/state/index-translate/index-translate.pid` |
| 日志 | `$XDG_STATE_HOME/index-translate/index-translate.log`，回退到 `~/.local/state/index-translate/index-translate.log` |
| 最大请求体 | 64 MiB |

`start` 会等待 `/health` 就绪后才返回；进程使用 PID 文件排他锁，重复启动会直接报错。日志只追加，不自动轮转。

## 沉浸式翻译配置

翻译服务选择 **自定义 (OpenAI 兼容)**：

| 字段 | 值 |
| --- | --- |
| API URL | `http://127.0.0.1:18080/v1` |
| 模型 | `Index-Translate-35B-A3B`（或 `Index-Translate-9B` / `Index-Translate-2B`） |
| API Key | 任意非空值，例如 `index` |

图片翻译同样可用：代理不解析消息体，图片 data URL 会原样转发。

## 前台运行

调试或交给 process supervisor 时使用前台模式：

```bash
index-translate serve
index-translate serve --access-log --log-level debug
```

`Ctrl-C` / `SIGINT` / `SIGTERM` 都会触发优雅退出（等待在途请求最多 8 秒），并清理 PID 文件。

## 常用参数

```bash
index-translate --help
```

| 参数 | 说明 |
| --- | --- |
| `--api-base <URL>` | 上游 API 根地址，默认 `https://index-translate.bilibili.com/v1`；环境变量 `INDEX_TRANSLATE_API_BASE` |
| `-H, --host <HOST>` | 监听网卡，默认 `127.0.0.1`；`0.0.0.0` 暴露到局域网 |
| `-p, --port <PORT>` | 本地端口，默认 `18080` |
| `-m, --model <MODEL>` | `/v1/models` 回退列表中的默认模型 |
| `--pid-file <PATH>` | 自定义 PID 文件 |
| `--log-file <PATH>` | 自定义后台日志文件 |
| `--log-level <LEVEL>` | `error` / `warn` / `info` / `debug` / `trace` 或 EnvFilter 指令；环境变量 `INDEX_TRANSLATE_LOG` |
| `--access-log` | 记录每个代理请求（沉浸式翻译轮询时会较吵） |
| `--max-body-mib <N>` | 最大请求体，默认 `64`，范围 1–1024 |

所有参数都是全局参数，可写在子命令前或后：

```bash
index-translate start --port 9000 --access-log
```

## 一键安装为 systemd 服务

```bash
# 用户服务（推荐）：登录后自动运行
index-translate install-service

# 同时允许开机即启动（无需先登录）
index-translate install-service --linger

# 系统级服务（需要 root）
sudo index-translate install-service --system
```

安装命令会：

1. 把当前可执行文件复制到稳定路径（用户级 `~/.local/bin/index-translate`，系统级 `/usr/local/bin/index-translate`）
2. 写入 unit（用户级 `~/.config/systemd/user/index-translate.service`，系统级 `/etc/systemd/system/index-translate.service`）
3. 停止通过 `index-translate start` 启动的旧实例，避免端口/PID 冲突
4. 执行 `systemctl daemon-reload`、`enable`、`restart`，并等待 `/health` 就绪

日常管理：

```bash
systemctl --user status index-translate
journalctl --user -u index-translate -f
systemctl --user restart index-translate
index-translate uninstall-service
```

系统级服务去掉 `--user`，并用 `sudo index-translate uninstall-service --system` 卸载。卸载只移除 unit，保留可执行文件和日志。

`install` / `uninstall` 分别是两个命令的别名。服务日志进入 journal；**不要同时使用** `index-translate start`（同一 PID 文件锁会阻止重复实例）。`--linger` 需要 `loginctl enable-linger` 权限；失败时只打印警告，服务仍会在下次登录时启动。

## 安全提示

- 默认只监听回环地址。代理本身没有任何鉴权，`--host 0.0.0.0` 会让同一网络内的任何人都能使用它并消耗上游免费额度。
- 上游是公开的免费服务，不保证可用性和速率；请勿用于高并发或敏感内容。
