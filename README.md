# boilchangeip

基于 Boil Token API 的自动换 IP 工具，支持 CLI、Telegram Bot、Timer 和多 VPS。

## ✨ 功能

- 使用新版 Boil Token API，一个 Token 对应一台 VPS
- 支持多 VPS，并要求明确选择目标，避免误操作
- 提供 CLI 状态查询、IP 检测和换 IP 操作
- Telegram Bot 支持 VPS 选择与二次确认
- Telegram Bot 支持原生命令菜单和 `/timer` 可视化管理定时任务
- Telegram Bot 支持交互式配对、添加/编辑/删除/排序服务器
- `/status` 支持手机友好的图片卡片，失败时自动回退为文本
- Timer 支持独立的“全部 Server”任务和每台 VPS 的单独任务
- `changeIP` 每次操作只提交一次，验证阶段只轮询 `getIP`

## 🚀 Quick Start

安装脚本支持 Linux，默认下载 GitHub Release 预编译二进制，创建配置并启动
systemd 服务。只有当前架构没有官方二进制时，才回退到源码编译。

### 安装

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/install.sh | sudo bash
```

安装脚本会优先下载 `boil-linux-amd64` 或 `boil-linux-arm64`，安装到
`/usr/local/bin/boil`。普通用户不需要安装 Rust、Cargo、Git，也不需要本地源码目录。
它不会覆盖现有 `/etc/boil/config.env`。首次安装且没有配置时，会启动配置向导；
配置完成后自动创建并启动 `boil.service`。

后续升级请使用 `update.sh`。

### 更新

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/update.sh | sudo bash
```

更新过程默认下载最新正式 Release 二进制并替换程序，现有 `config.env` 保持不变。
更新脚本不依赖当前目录、本地源码、`/opt/boilchangeip/source`、Git 或 Cargo。
更新时会停止服务后替换二进制，避免 `Text file busy`，随后自动重启并验证服务。
更新前会把 `/etc/boil` 备份到带时间戳的安全目录，失败时会恢复旧二进制、配置备份并尝试恢复服务。

只有当前 Release 没有对应架构二进制时，才会回退到源码编译。源码编译时会依次查找：
`cargo`、`/root/.cargo/bin/cargo`、`/home/$SUDO_USER/.cargo/bin/cargo`。

### 卸载

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/uninstall.sh | sudo bash
```

普通卸载会删除 systemd 服务和二进制，但保留 `/etc/boil` 配置。彻底卸载需要
额外传入 `--purge`：

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/uninstall.sh | sudo bash -s -- --purge
```

`--purge` 会要求输入 `DELETE`，然后额外删除 `/etc/boil`。Rust、Cargo、Git
以及用户自己 clone 的仓库不会被删除。

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
# TG_CHAT_ID 由 boil setup 生成配对码后自动写入，请不要手动填写。

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

在 `config.env` 中配置 `TG_TOKEN` 后，先运行 `boil setup` 生成一次性配对码，再启动 Bot：

```bash
boil setup
# 按终端提示在 Telegram 中发送 /pair 配对码
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
| `/servers` | 查看服务器列表，并进入状态、更换 IP、定时、编辑、删除和排序操作 |
| `/addserver` | 启动添加服务器向导 |

Bot 启动时会同步 Telegram 原生命令菜单，私聊输入框左下角可直接打开 Menu。
菜单包含 `/start`、`/status`、`/change`、`/timer`、`/servers`、`/addserver` 和 `/help`。`/check` 仍可使用，但不显示在原生命令菜单中。

首次配对流程：

1. 运行 `boil setup`。
2. 终端显示一次性 `/pair CODE`，有效期 5 分钟。
3. 在 Telegram Bot 私聊中发送终端提示的 `/pair CODE`。
4. 配对成功后，程序自动保存 `TG_CHAT_ID`，同步命令菜单和 Menu 按钮。

未配对前，`/start` 只提示先完成配对，其他管理命令会拒绝访问。已绑定后，
`/pair` 不能覆盖现有绑定，其他聊天和 callback 都会被拒绝。

服务器管理：

- `/addserver` 会依次要求输入服务器名称、服务器地址和 Token。
- 服务器地址支持 IPv4、IPv6 和域名；输入 `http://` 或 `https://` 时会自动取主机名。
- Bot 会尝试识别国家/地区和国旗，失败时显示 `🌐 未知地区`，不阻止保存。
- `/servers` 按配置顺序展示服务器，支持查看状态、更换 IP、管理定时、编辑、删除和上移/下移。
- 编辑名称、地址、Token 后会立即刷新内存配置和 TimerManager，无需重启 Bot。
- 修改 Token 会先验证，验证失败不会覆盖旧 Token。
- 删除服务器需要二次确认，删除后只移除该服务器及其单机定时任务，不影响全局定时任务和其他服务器。

服务器展示统一使用：

```text
📡 服务器名称

🇭🇰 中国香港
203.0.113.10
```

不会显示内部 server id、Token 或旧版 router/interface 信息。

`/status` 默认发送每台服务器一张图片卡片，包含服务器名称、地区、地址、状态和下次换 IP 倒计时。
图片在本地临时生成，不依赖在线制图服务；图片生成或 Telegram `sendPhoto` 失败时，会自动回退为安全文本。

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
| systemd 服务 | `boil.service` |

可以在运行安装、更新或卸载脚本时通过环境变量覆盖路径：

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/install.sh |
  sudo env BOIL_INSTALL_DIR="$HOME/.local/bin" \
    BOIL_MANAGED_ROOT="$HOME/.local/share/boilchangeip" \
    bash
```

还可以使用 `BOIL_BRANCH=develop` 指定开发分支源码编译。默认 `main` 会优先使用
最新正式 Release 二进制；安装和更新脚本都不依赖调用者当前目录，也不依赖本地长期源码目录。
安装和更新也支持指定版本或 tag：

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/install.sh |
  sudo env BOIL_VERSION=2.1.2 bash

curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/update.sh |
  sudo env BOIL_TAG=v2.1.2 bash

curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/update.sh |
  sudo env BOIL_BRANCH=develop bash
```

指定不存在的版本或分支会明确报错，不会破坏当前安装。

从 v2.1.0 升级时，建议直接使用：

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/update.sh | sudo bash
```

v2.1.2 的 `update.sh` 默认使用官方 Release 二进制，不再要求用户安装 Rust、Cargo
或 Git。更新前会备份 `/etc/boil` 和旧二进制；如果更新失败，会恢复旧二进制和配置备份，并尝试恢复服务。

## 发布新版本

发布在 VPS 本地执行，不依赖 GitHub Actions：

```bash
./scripts/release.sh v2.2.1
```

脚本会检查当前分支、工作区状态、源码版本号、本地和远程 tag，然后构建并发布：

- `boil-linux-amd64`
- `boil-linux-arm64`

Release 只包含以上两个二进制文件。

## 旧配置迁移

从 v2.0.2 升级到 v2.1.0 不需要手动迁移 `BOIL_SERVERS`。新增的
`address`、`country`、`flag` 和 `resolved_ip` 字段都是可选字段；旧配置可直接读取。
重新保存配置时会保留服务器 Token、名称、启用状态、全局定时和每台服务器定时。

如果已经存在 `TG_CHAT_ID`，Bot 会继续使用现有绑定，不会要求重新配对。

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

boilchangeip 现作为独立项目维护。感谢早期上游实现与开源社区提供的基础思路。
