# boilchangeip v2.0.2

这是一次发布脚本安全性和可维护性更新，重点完善更新备份、卸载模式和版本选择。

## Highlights

- `update.sh` 更新前自动备份 `/etc/boil`，失败时恢复配置并尝试恢复服务。
- `uninstall.sh` 默认保留 `/etc/boil`，只有 `--purge` 才彻底删除配置和安装器源码。
- `install.sh` 和 `update.sh` 支持通过 `BOIL_VERSION` 或 `BOIL_TAG` 指定版本。
- 默认仍使用 `main` 最新正式版，并保留 `BOIL_BRANCH=develop`。

## 安装和更新

默认安装 main 最新正式版：

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/install.sh | bash
```

指定版本：

```bash
BOIL_VERSION=2.0.2 bash install.sh
BOIL_TAG=v2.0.2 bash update.sh
```

使用 develop：

```bash
BOIL_BRANCH=develop bash update.sh
```

指定不存在的版本或分支时会明确报错，不会破坏当前安装。

## 更新备份

`update.sh` 会在更新前备份 `/etc/boil` 到：

```text
/var/backups/boilchangeip/config-<UTC 时间戳>
```

更新成功后保留本次备份，并在输出中显示备份位置。更新失败时会恢复原 `/etc/boil`，并尝试恢复服务。备份和恢复过程只输出路径，不打印配置内容，避免泄露 Token。

## 卸载

普通卸载：

```bash
curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/uninstall.sh | bash
```

删除：

- systemd 服务：`/etc/systemd/system/boil.service`
- 二进制：`/usr/local/bin/boil`

保留：

- 配置和运行数据：`/etc/boil`
- 安装器源码：`/opt/boilchangeip`
- Rust、Cargo、Git 和用户自己 clone 的仓库

彻底卸载：

```bash
./uninstall.sh --purge
```

`--purge` 需要输入 `DELETE`，并额外删除：

- `/etc/boil`
- `/opt/boilchangeip`
