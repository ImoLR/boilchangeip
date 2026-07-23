# Changelog

## v2.1.0 - 2026-07-23

### 新增

- Telegram Bot 新增一次性配对码流程，`boil setup` 生成 `/pair CODE`，成功后自动保存 `TG_CHAT_ID` 并同步 Menu。
- Telegram `/start` 改为 Inline Keyboard 主菜单，支持直接进入添加服务器、服务器列表、状态、换 IP、Timer 和帮助。
- Telegram 新增交互式添加服务器向导，支持名称、地址、Token 分步录入，并自动识别国家/地区和国旗。
- Telegram `/servers` 支持服务器管理按钮：状态、换 IP、定时任务、编辑、删除、上移和下移。
- Telegram 新增服务器编辑流程，支持修改名称、地址、Token 和重新验证。
- Telegram 新增服务器删除确认流程，删除服务器时同步删除该服务器自己的定时任务。
- Telegram `/status` 新增本地生成的图片卡片，显示服务器名称、地区、地址、状态和下次换 IP 倒计时。

### 改进

- 服务器展示统一为用户填写名称、国家/地区和地址，不显示内部 server id 或 Token。
- 图片卡片生成失败或 Telegram `sendPhoto` 失败时自动回退为安全文本。
- Bot 代码拆分为命令、callback、配对、添加向导、编辑、删除、列表、格式化、状态和 Timer UI 等模块。
- 状态卡倒计时不再引入额外 `chrono` 直接依赖，复用项目现有 HH:MM 解析逻辑。

### 兼容性

- v2.0.2 的 `BOIL_SERVERS` 配置可直接读取；新增的 `address`、`country`、`flag` 和 `resolved_ip` 字段均为可选。
- 已存在 `TG_CHAT_ID` 的配置继续有效，不需要重新配对。
- 重新保存配置时继续保留 Token、服务器名称、启用状态、全局定时和每台服务器定时。

### 安全

- 未完成配对前，除 `/start` 和 `/pair` 外的管理命令均拒绝访问。
- 已绑定后，其他 chat 的普通消息和 callback 都会被拒绝。
- Token、配对码和服务器地址不会进入 Telegram callback；Token 不会展示在状态卡或服务器列表中。

## v2.0.2 - 2026-07-23

### 改进

- `install.sh` 和 `update.sh` 支持 `BOIL_VERSION`、`BOIL_TAG` 指定版本，继续支持 `BOIL_BRANCH=develop`。
- `update.sh` 更新前会备份 `/etc/boil` 到带时间戳的安全目录，更新失败时恢复配置备份并尝试恢复服务。
- `update.sh` 指定不存在版本或分支时会明确报错，不覆盖当前安装。
- `uninstall.sh` 默认改为普通卸载，只删除 systemd 服务和二进制，保留 `/etc/boil`。
- `uninstall.sh --purge` 才会要求输入 `DELETE` 并彻底删除 `/etc/boil` 和安装器维护的 `/opt/boilchangeip`。
- README 补充普通卸载、彻底卸载、版本选择和更新备份说明。

### 安全

- 更新备份只输出备份目录路径，不打印配置文件内容，避免泄露 Token 或 Telegram Bot Token。
- 更新失败恢复配置时不会输出配置内容。
- 卸载脚本继续保留 Rust、Cargo、Git 和用户自己 clone 的仓库。

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
