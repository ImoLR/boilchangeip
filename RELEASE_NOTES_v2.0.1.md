# boilchangeip v2.0.1

这是一次正式维护版本，重点完善 Telegram 定时换 IP 管理和安装升级体验。

## Highlights

- Telegram Bot 新增原生命令菜单，私聊输入框 Menu 会列出当前支持的 Bot 命令。
- `/timer` 新增可视化管理面板，支持新建、编辑、关闭和刷新定时换 IP。
- Timer 支持独立的“全部 Server”任务和每台 Server 的单独任务，两者可以同时存在。
- 安装、更新和卸载脚本重构，适合服务器长期维护部署。

## Telegram

- 菜单命令包含 `/start`、`/help`、`/status`、`/check`、`/change`、`/timer`。
- `/timer` 面板显示当前时区、全局任务、单机任务和每天执行时间。
- 全局任务作用于所有 `enabled=true` 的 Server。
- 单机任务只作用于指定 Server。
- 关闭任务不会删除配置，会保留已设置时间。

## 配置

新增可选配置：

```bash
BOIL_GLOBAL_TIMER='{
  "enabled": true,
  "cron": "30 3 * * *"
}'
```

已有 `BOIL_SERVERS[].timer` 继续兼容，不需要手动迁移。

## 安装和升级

安装：

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/install.sh | bash
```

更新：

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/update.sh | bash
```

卸载：

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/uninstall.sh | bash
```

## 安全说明

- `change_ip` 执行点增加进程内全局互斥，避免 CLI、Telegram 和 Timer 并发发出换 IP 请求。
- `update.sh` 不会覆盖安装器源码目录中的未提交修改。
- `uninstall.sh` 需要输入 `DELETE`，只删除安装器管理的路径和 `/etc/boil`。
- Token 不会写入 Telegram callback、日志、错误输出或 Debug 结构。
