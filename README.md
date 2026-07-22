# boil

为 Boil.network 拨号 VPS 设计的换 IP 工具，支持命令行、Telegram Bot 和按 VPS 独立定时任务。

当前 Rust 主路径使用新版 Boil Token API：

- `POST /api/v1/getIP`
- `POST /api/v1/changeIP/`
- `Authorization: Bearer <token>`

一个新版 token 对应一台 VPS。多台 VPS 需要配置多个 token。

## 功能

- `servers list` — 查看已配置 VPS，不显示 token
- `status` — 查询当前 IP，只调用 `getIP`
- `check` — 查询当前 IP 后检查 IP 质量和流媒体解锁
- `change` — 调用新版 `changeIP` 并轮询 `getIP` 确认结果
- `timer` — 查看每台 VPS 的独立定时配置
- Telegram Bot — VPS 选择、二次确认、换 IP 结果推送

`changeIP` 不会自动重试。Boil API 失败请求也可能消耗换 IP 次数，请不要重复点击或自行并发触发。

## 安装

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/install.sh | bash
```

支持平台：Linux x86_64 / aarch64。

安装完成后创建配置：

```bash
boil setup
```

也可以手动复制示例：

```bash
cp config.env.example config.env
```

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
```

规则：

- `id` 必须唯一，只能包含字母、数字、短横线和下划线。
- `id` 不得包含 token、IP、邮箱、账号等敏感信息。
- `token` 不会出现在日志、callback、结果结构或 Debug 输出中。
- 只有一台 enabled VPS 时，CLI 可省略 `--server`。
- 多台 enabled VPS 时，CLI 必须指定 `--server <id>` 或 `--all`。
- `--all` 按配置顺序执行，不并发调用 `changeIP`。

Telegram Bot 可选：

```bash
TG_TOKEN='your-bot-token'
TG_CHAT_ID='your-chat-id'
```

不要把真实 token 写入文档、Issue、日志或提交。

## CLI

```bash
boil servers list

boil status
boil status --server hk-01
boil status --all

boil check
boil check --server hk-01
boil check --all

boil change
boil change --server hk-01
boil change --all

boil timer
boil bot
boil daemon
boil setup
```

`--server` 和 `--all` 互斥。多台 enabled VPS 时未指定目标会报错并显示可用 server id 和 name，且不会发送 HTTP 请求。

## Telegram

Telegram 命令：

| 命令 | 说明 |
|------|------|
| `/status` | 单台 enabled VPS 时直接显示，多台时先选择 VPS |
| `/status hk-01` | 查看指定 VPS 当前 IP |
| `/check` | 单台 enabled VPS 时直接检测，多台时先选择 VPS |
| `/check hk-01` | 检查指定 VPS |
| `/change` | 选择 VPS 后二次确认；即使只有一台也需要确认 |
| `/change hk-01` | 对指定 VPS 发起二次确认 |
| `/timer` | 查看每台 VPS 的定时配置 |

换 IP callback 只包含操作类型、server id 和短期 nonce，不包含 token、Authorization Header、旧 router_id 或 interface。确认过期或重复点击不会重复执行 `changeIP`。

## Timer

Timer 绑定到每台 VPS 的 `server_id`，配置在对应 `BOIL_SERVERS` 对象内：

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

定时触发时会重新按 `server_id` 解析配置。server 不存在或已禁用时会跳过，不会 fallback 到第一台 VPS。进程内同一 `server_id` 的 reconnect 会串行化。

## 旧配置迁移

旧版配置已废弃：

```bash
BOIL_ACCOUNT='...'
BOIL_PASSWORD='...'
BOIL_ROUTER_ID='...'
BOIL_INTERFACE='...'
```

当前 Rust 主路径不再使用旧 `/login`、`/api/query_all`、`/api/reconnect`。请在 Boil 面板为每台 VPS 获取新版 token，并手动写入 `BOIL_SERVERS`。工具不会用旧账号密码自动换取 token。

`change-ip.sh` 和 `tg-bot.sh` 已标记为 legacy/deprecated，启动后只提示使用 Rust 主程序，不再调用旧 API。

## 从源码编译

```bash
git clone https://github.com/ImoLR/boilchangeip.git
cd boilchangeip
cargo build --release
./target/release/boil
```

## 来源

本项目 fork 自 [0xUnixIO/boil](https://github.com/0xUnixIO/boil)，感谢原作者的实现与开源贡献。
