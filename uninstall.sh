#!/usr/bin/env bash
# boilchangeip 彻底卸载脚本。

set -euo pipefail

BIN_NAME="${BOIL_BIN_NAME:-boil}"
SERVICE_NAME="${BOIL_SERVICE_NAME:-boil}"
INSTALL_DIR="${BOIL_INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="${BOIL_CONFIG_DIR:-/etc/boil}"
MANAGED_ROOT="${BOIL_MANAGED_ROOT:-/opt/boilchangeip}"
SERVICE_PATH="/etc/systemd/system/${SERVICE_NAME}.service"

die() {
  echo "错误: $*" >&2
  exit 1
}

run_privileged() {
  if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
    "$@"
  elif command -v sudo >/dev/null 2>&1; then
    sudo "$@"
  else
    die "操作需要管理员权限，但未找到 sudo"
  fi
}

systemctl_available() {
  command -v systemctl >/dev/null 2>&1 && [[ -d /run/systemd/system ]]
}

confirm_delete() {
  local answer

  echo "将彻底删除 boilchangeip："
  echo "  systemd 服务: $SERVICE_NAME"
  echo "  二进制: $INSTALL_DIR/$BIN_NAME"
  echo "  配置和运行数据: $CONFIG_DIR"
  echo "  安装器维护的源码目录: $MANAGED_ROOT"
  echo
  echo "不会删除 Rust、Cargo、Git，也不会删除用户自己 clone 的仓库。"
  echo

  if [[ -r /dev/tty ]]; then
    read -r -p "如确认卸载，请输入 DELETE: " answer </dev/tty
  else
    die "彻底卸载需要交互确认，请在终端中运行本地 uninstall.sh"
  fi

  [[ "$answer" == "DELETE" ]] || {
    echo "输入不匹配，已取消卸载。"
    exit 0
  }
}

validate_remove_path() {
  local path="$1"
  local resolved

  [[ -n "$path" ]] || die "拒绝删除空路径"
  command -v readlink >/dev/null 2>&1 || die "缺少命令: readlink"
  resolved="$(readlink -m -- "$path")"
  case "$resolved" in
    /|/bin|/boot|/dev|/etc|/home|/lib|/lib64|/opt|/proc|/root|/run|/sbin|/srv|/sys|/tmp|/usr|/var|"$HOME")
      die "拒绝删除不安全路径: $resolved"
      ;;
  esac

  echo "$resolved"
}

remove_path() {
  local path="$1"

  if [[ ! -e "$path" && ! -L "$path" ]]; then
    echo "不存在，跳过: $path"
    return
  fi

  if [[ -w "$(dirname "$path")" ]]; then
    rm -rf -- "$path"
  else
    run_privileged rm -rf -- "$path"
  fi
  echo "已删除: $path"
}

remove_service() {
  if systemctl_available; then
    if systemctl list-unit-files "${SERVICE_NAME}.service" >/dev/null 2>&1 ||
      systemctl status "$SERVICE_NAME" >/dev/null 2>&1; then
      run_privileged systemctl disable --now "$SERVICE_NAME" || true
    fi
  fi

  remove_path "$SERVICE_PATH"

  if systemctl_available; then
    run_privileged systemctl daemon-reload
    run_privileged systemctl reset-failed "$SERVICE_NAME" || true
  fi
}

main() {
  [[ "$(uname -s)" == "Linux" ]] || die "仅支持 Linux 系统"
  confirm_delete

  local binary
  local config_path
  local managed_path

  binary="$(validate_remove_path "$INSTALL_DIR/$BIN_NAME")"
  config_path="$(validate_remove_path "$CONFIG_DIR")"
  managed_path="$(validate_remove_path "$MANAGED_ROOT")"

  remove_service
  remove_path "$binary"
  remove_path "$config_path"
  remove_path "$managed_path"

  echo
  echo "boilchangeip 已彻底卸载。"
  echo "Rust、Cargo、Git 和用户自己 clone 的仓库已保留。"
}

main "$@"
