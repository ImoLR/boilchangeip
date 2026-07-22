# boilchangeip

基于 Boil Token API 的自动换 IP 工具，支持 CLI、Telegram Bot、Timer 和多 VPS。

## ✨ 功能

- 使用新版 Boil Token API，一个 Token 对应一台 VPS
- 支持多 VPS，并要求明确选择目标，避免误操作
- 提供 CLI 状态查询、IP 检测和换 IP 操作
- Telegram Bot 支持 VPS 选择与二次确认
- Timer 支持为每台 VPS 配置独立定时任务
- `changeIP` 每次操作只提交一次，验证阶段只轮询 `getIP`

## 🚀 Quick Start

安装前请确认系统为 Linux，并已安装 `git` 和 Rust/Cargo 工具链。

### 安装

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/install.sh | bash
```

安装程序会把源码保存在用户数据目录，从源码编译 Release 二进制，并安装到
`/usr/local/bin/boil`。它不会覆盖现有 `config.env`，也不会自动安装或启动
systemd 服务。

首次安装后运行配置向导：

```bash
boil setup
```

### 更新

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/update.sh | bash
```

更新过程会拉取源码、重新编译并替换程序，现有 `config.env` 保持不变。

### 卸载

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/uninstall.sh | bash
```

默认只删除程序，保留配置和源码数据。需要彻底清理时，在本地仓库中运行：

```bash
./uninstall.sh --purge
```

`--purge` 会在确认后删除程序、配置目录和安装器管理的源码数据。

## 配置

主要配置项是 `BOIL_SERVERS`，内容为 JSON 数组：

```bash
BOIL_SERVERS='[
  {
    "id": "hk-01",
    "name": "Hong Kong 01",
    "token": "replace-with-real-token",
    "enabled": true,
    "timer": {
      "enabled": true,
      "cron": "0 0 */6 * * *"
    }
  },
  {
    "id": "jp-01",
    "name": "Japan 01",
    "token": "replace-with-real-token",
    "enabled": true
  }
]'

TG_TOKEN='your-bot-token'
TG_CHAT_ID='your-chat-id'
```

也可以从示例开始手动配置：

```bash
cp config.env.example config.env
```

配置规则：

- `id` 必须唯一，只能包含字母、数字、短横线和下划线。
- `id` 不得包含 Token、IP、邮箱、账号等敏感信息。
- 只有一台已启用 VPS 时，CLI 可以省略 `--server`。
- 多台已启用 VPS 时，必须指定 `--server <id>` 或 `--all`。
- `--all` 按配置顺序执行，不并发调用 `changeIP`。
- Token 不会进入日志、Telegram callback、结果结构或 Debug 输出。

程序按以下顺序查找配置：

1. `/etc/boil/config.env`
2. 可执行文件同目录下的 `config.env`
3. 当前工作目录下的 `config.env`

不要把真实 Token 写入文档、Issue、日志或 Git 提交。

## 使用

查看服务器与当前 IP：

```bash
boil servers list
boil status
boil status --server hk-01
boil status --all
```

检查 IP 质量和流媒体状态：

```bash
boil check
boil check --server hk-01
boil check --all
```

执行换 IP：

```bash
boil change
boil change --server hk-01
boil change --all
```

其他命令：

```bash
boil timer
boil bot
boil daemon
boil setup
```

`--server` 和 `--all` 互斥。多台已启用 VPS 时，如果未指定目标，程序会报错并列出可用的 server id 和 name，不会默认选择第一台，也不会发送换 IP 请求。

## Telegram

在 `config.env` 中配置 `TG_TOKEN` 和 `TG_CHAT_ID` 后运行：

```bash
boil bot
```

支持的 Telegram 命令：

| 命令 | 说明 |
| --- | --- |
| `/status` | 单台 VPS 时直接显示，多台时先选择 VPS |
| `/status hk-01` | 查看指定 VPS 当前 IP |
| `/check` | 单台 VPS 时直接检测，多台时先选择 VPS |
| `/check hk-01` | 检查指定 VPS |
| `/change` | 选择 VPS 后二次确认；即使只有一台也需要确认 |
| `/change hk-01` | 对指定 VPS 发起二次确认 |
| `/timer` | 查看每台 VPS 的定时配置 |

换 IP callback 只包含操作类型、server id 和短期 nonce，不包含 Token、Authorization Header、旧 router_id 或 interface。确认过期或重复点击不会再次执行 `changeIP`。

## Timer

Timer 绑定明确的 `server_id`，配置在对应的 `BOIL_SERVERS` 对象中：

```json
{
  "id": "hk-01",
  "name": "Hong Kong 01",
  "token": "replace-with-real-token",
  "enabled": true,
  "timer": {
    "enabled": true,
    "cron": "0 0 */6 * * *"
  }
}
```

定时触发时会重新按 `server_id` 解析配置。服务器不存在或已禁用时会跳过，不会 fallback 到第一台 VPS。进程内同一 `server_id` 的 reconnect 会串行执行。

## 安装目录与自定义

默认路径：

| 内容 | 路径 |
| --- | --- |
| 程序 | `/usr/local/bin/boil` |
| 配置 | `/etc/boil/config.env` |
| 安装器数据和源码 | `${XDG_DATA_HOME:-$HOME/.local/share}/boilchangeip` |

可以在运行安装、更新或卸载脚本时通过环境变量覆盖路径：

```bash
BOIL_INSTALL_DIR="$HOME/.local/bin" \
BOIL_DATA_DIR="$HOME/.local/share/boilchangeip" \
bash install.sh
```

还可以使用 `BOIL_BRANCH` 指定源码分支，默认是 `main`。安装器不会自动管理 systemd；需要服务时请在理解其行为后手动执行相关命令。

## 旧配置迁移

旧版 `BOIL_ACCOUNT`、`BOIL_PASSWORD`、`BOIL_ROUTER_ID` 和
`BOIL_INTERFACE` 已废弃。当前主路径不再使用旧 `/login`、
`/api/query_all` 或 `/api/reconnect`。

请在 Boil 面板为每台 VPS 获取新版 Token，并手动写入 `BOIL_SERVERS`。工具不会用旧账号密码自动换取 Token。

`change-ip.sh` 和 `tg-bot.sh` 是已禁用的 legacy 入口，只会提示使用 Rust 主程序，不会调用旧 API。

## 开发

从源码构建：

```bash
git clone https://github.com/ImoLR/boilchangeip.git
cd boilchangeip
cargo build --release
./target/release/boil --version
```

提交前运行：

```bash
cargo fmt --check
cargo check --all-targets --all-features
cargo test --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
```

所有外部 HTTP 测试都必须使用本地 Mock。禁止在测试中调用真实 `changeIP` 或消耗真实额度。

## 来源与致谢

本项目 fork 自 [0xUnixIO/boil](https://github.com/0xUnixIO/boil)，感谢原作者的实现与开源贡献。
