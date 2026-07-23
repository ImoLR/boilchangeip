# Changelog

## v2.0.1 - 2026-07-23

### 新增

- Telegram Bot 增加原生命令菜单，支持在私聊输入框 Menu 中选择 `/start`、`/help`、`/status`、`/check`、`/change` 和 `/timer`。
- Telegram `/timer` 增加可视化管理面板，支持新建、编辑、关闭和刷新定时换 IP。
- Timer 支持独立的全局任务 `BOIL_GLOBAL_TIMER`，可与每台 Server 自己的 `timer` 同时存在。
- 安装脚本支持首次安装、自动安装依赖、编译 Release、创建并启动 systemd 服务。
- 更新脚本支持一条命令升级、main/develop 分支、detached HEAD 处理、保留 `/etc/boil` 配置、停止服务后替换二进制并重启验证。
- 卸载脚本改为默认彻底卸载，输入 `DELETE` 后删除 systemd 服务、二进制、`/etc/boil` 和安装器维护的源码目录。

### 改进

- 全局 timer 触发时按当前 `enabled=true` Server 集合顺序执行，单机 timer 只处理对应 Server。
- `change_ip` 执行点增加进程内全局互斥，避免 CLI、Telegram 和 Timer 同时发出并发换 IP 请求。
- 配置持久化改为一次生成完整文件并 rename，保留 `config.env` 中其他字段和注释。
- README 和 `config.env.example` 更新为新版 Token API、多 VPS、Telegram timer 和安装脚本说明。

### 兼容性

- 旧的 `BOIL_SERVERS[].timer` 配置继续有效，不需要手动迁移。
- 未配置 `BOIL_GLOBAL_TIMER` 时，全局定时任务为 `None`。
- `BOIL_GLOBAL_TIMER` 为空、非法 JSON 或 cron 字段明显非法时会返回明确配置错误。

### 安全

- Token 不进入 Telegram callback、Debug、错误文案或结果结构。
- `update.sh` 拒绝覆盖安装器源码目录中的未提交修改。
- `uninstall.sh` 限定删除安装器维护路径，保留 Rust、Cargo、Git 和用户自己 clone 的仓库。

## v2.0.0 - 2026-07-23

### 新增

- 迁移到新版 Boil Token API。
- 支持多 VPS 配置、明确选择 Server 和顺序批量换 IP。
- 新增可 mock 的 HTTP Client 和新版 reconnect 服务层。

### 移除

- 移除旧账号密码、router_id/interface、`/login`、`/api/query_all` 和 `/api/reconnect` 主路径。
