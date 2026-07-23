# boilchangeip

基于 Boil Token API 的自动换 IP 工具，支持 CLI、Telegram Bot、Timer 和多 VPS。

## ✨ 功能

- 使用新版 Boil Token API，一个 Token 对应一台 VPS
- 支持多 VPS，并要求明确选择目标，避免误操作
- 提供 CLI 状态查询、IP 检测和换 IP 操作
- Telegram Bot 支持 VPS 选择与二次确认
- Telegram Bot 支持原生命令菜单和 `/timer` 可视化管理定时任务
- Timer 支持独立的“全部 Server”任务和每台 VPS 的单独任务
- `changeIP` 每次操作只提交一次，验证阶段只轮询 `getIP`

## 🚀 Quick Start

安装脚本支持 Linux，会自动安装所需依赖、拉取源码、编译 Release 二进制、
创建配置并启动 systemd 服务。

### 安装

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/install.sh | bash
```

安装程序会把源码保存在 `/opt/boilchangeip/source`，从源码编译 Release 二进制，并安装到
`/usr/local/bin/boil`。它不会覆盖现有 `/etc/boil/config.env`。首次安装且没有
配置时，会启动配置向导；配置完成后自动创建并启动 `boil.service`。

后续升级请使用 `update.sh`。

### 更新

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/update.sh | bash
```

更新过程会拉取源码、重新编译并替换程序，现有 `config.env` 保持不变。
更新时会停止服务后替换二进制，避免 `Text file busy`，随后自动重启并验证服务。

### 卸载

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/uninstall.sh | bash
```

卸载脚本默认彻底卸载，会要求输入 `DELETE` 确认，然后删除 systemd 服务、
二进制、`/etc/boil` 配置和安装器维护的源码目录。Rust、Cargo、Git 以及用户
自己 clone 的仓库不会被删除。

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
      "cron": "0 8 * * *"
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

# 可选：独立的“全部 Server”定时任务。
# 触发时会按配置顺序顺序处理所有 enabled=true 的 Server。
BOIL_GLOBAL_TIMER='{
  "enabled": true,
  "cron": "30 3 * * *"
}'
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
- `BOIL_GLOBAL_TIMER` 是独立的全局定时任务，不会覆盖每台 Server 自己的 `timer`。

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
| `/timer` | 管理全部 Server 和单台 VPS 的定时换 IP |

Bot 启动时会同步 Telegram 原生命令菜单，私聊输入框左下角可直接打开 Menu。
菜单包含 `/start`、`/help`、`/status`、`/check`、`/change` 和 `/timer`。

`/timer` 面板显示当前时区、全局任务、每台 Server 的定时状态和每天执行时间。
支持新建、编辑、关闭和刷新。关闭只会设置 `enabled=false`，保留原时间，后续
重新开启或编辑可继续使用。

换 IP callback 只包含操作类型、server id 和短期 nonce，不包含 Token、Authorization Header、旧 router_id 或 interface。确认过期或重复点击不会再次执行 `changeIP`。

## Timer

Timer 分为两类，可以同时存在：

- `BOIL_GLOBAL_TIMER`：独立的“全部 Server”任务，触发时处理所有 `enabled=true` 的 VPS。
- `BOIL_SERVERS[].timer`：每台 VPS 自己的独立任务。

全局任务示例：

```bash
BOIL_GLOBAL_TIMER='{
  "enabled": true,
  "cron": "30 3 * * *"
}'
```

单机任务示例：

```json
{
  "id": "hk-01",
  "name": "Hong Kong 01",
  "token": "replace-with-real-token",
  "enabled": true,
  "timer": {
    "enabled": true,
    "cron": "0 8 * * *"
  }
}
```

定时触发时会重新按 `server_id` 解析配置。服务器不存在或已禁用时会跳过，不会 fallback 到第一台 VPS。进程内同一 `server_id` 的 reconnect 会串行执行。
全局任务触发时会重新读取当前 `enabled=true` 的 Server 集合，按配置顺序串行执行。
全局任务和单机任务如果同一分钟触发，会通过进程内执行锁排队，避免并发
`changeIP`。

## 安装目录与自定义

默认路径：

| 内容 | 路径 |
| --- | --- |
| 程序 | `/usr/local/bin/boil` |
| 配置 | `/etc/boil/config.env` |
| 安装器维护的源码 | `/opt/boilchangeip/source` |

可以在运行安装、更新或卸载脚本时通过环境变量覆盖路径：

```bash
BOIL_INSTALL_DIR="$HOME/.local/bin" \
BOIL_MANAGED_ROOT="$HOME/.local/share/boilchangeip" \
bash install.sh
```

还可以使用 `BOIL_BRANCH` 指定源码分支，默认是 `main`。`update.sh` 支持
`main` 和 `develop`，并会拒绝覆盖安装器源码目录中的未提交修改。

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
