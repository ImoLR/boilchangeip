#!/usr/bin/env bash
# boilchangeip 卸载脚本。默认保留 /etc/boil，--purge 才彻底删除配置和安装器源码。

set -euo pipefail

BIN_NAME="${BOIL_BIN_NAME:-boil}"
SERVICE_NAME="${BOIL_SERVICE_NAME:-boil}"
INSTALL_DIR="${BOIL_INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="${BOIL_CONFIG_DIR:-/etc/boil}"
MANAGED_ROOT="${BOIL_MANAGED_ROOT:-/opt/boilchangeip}"
SERVICE_PATH="/etc/systemd/system/${SERVICE_NAME}.service"
PURGE=false

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

service_exists() {
  systemctl_available &&
    (systemctl list-unit-files "${SERVICE_NAME}.service" >/dev/null 2>&1 ||
      systemctl status "$SERVICE_NAME" >/dev/null 2>&1)
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --purge)
        PURGE=true
        ;;
      -h|--help)
        echo "用法: ./uninstall.sh [--purge]"
        echo "默认删除程序和 systemd 服务，保留 $CONFIG_DIR。"
        echo "--purge 会额外删除 $CONFIG_DIR 和 $MANAGED_ROOT。"
        exit 0
        ;;
      *)
        die "未知参数: $1"
        ;;
    esac
    shift
  done
}

confirm_purge() {
  local answer

  echo "将彻底删除 boilchangeip 运行数据："
  echo "  配置和运行数据: $CONFIG_DIR"
  echo "  安装器维护的源码目录: $MANAGED_ROOT"
  echo
  echo "Rust、Cargo、Git 和用户自己 clone 的仓库仍会保留。"
  echo

  if [[ -r /dev/tty ]]; then
    read -r -p "如确认彻底卸载，请输入 DELETE: " answer </dev/tty
  else
    die "--purge 需要交互确认，请在终端中运行本地 uninstall.sh"
  fi

  [[ "$answer" == "DELETE" ]] || {
    echo "输入不匹配，已取消彻底卸载。"
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
  if service_exists; then
    run_privileged systemctl disable --now "$SERVICE_NAME" || true
  fi

  remove_path "$SERVICE_PATH"

  if systemctl_available; then
    run_privileged systemctl daemon-reload
    run_privileged systemctl reset-failed "$SERVICE_NAME" || true
  fi
}

main() {
  [[ "$(uname -s)" == "Linux" ]] || die "仅支持 Linux 系统"
  parse_args "$@"

  if [[ "$PURGE" == true ]]; then
    confirm_purge
  fi

  local binary
  binary="$(validate_remove_path "$INSTALL_DIR/$BIN_NAME")"

  remove_service
  remove_path "$binary"

  if [[ "$PURGE" == true ]]; then
    local config_path
    local managed_path
    config_path="$(validate_remove_path "$CONFIG_DIR")"
    managed_path="$(validate_remove_path "$MANAGED_ROOT")"
    remove_path "$config_path"
    remove_path "$managed_path"
    echo "彻底卸载完成。"
  else
    echo "卸载完成，已保留配置: $CONFIG_DIR"
    echo "如需删除配置和安装器源码，请运行: ./uninstall.sh --purge"
  fi

  echo "Rust、Cargo、Git 和用户自己 clone 的仓库已保留。"
}

main "$@"
