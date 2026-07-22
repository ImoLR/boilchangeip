#!/usr/bin/env bash
# boilchangeip 卸载脚本。默认保留配置和安装器数据。

set -Eeuo pipefail

BIN_NAME="${BOIL_BIN_NAME:-boil}"
INSTALL_DIR="${BOIL_INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="${BOIL_CONFIG_DIR:-/etc/boil}"
DATA_DIR="${BOIL_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/boilchangeip}"
PURGE=false

die() {
  echo "错误: $*" >&2
  exit 1
}

run_privileged() {
  if [[ "$EUID" -eq 0 ]]; then
    "$@"
  elif command -v sudo >/dev/null 2>&1; then
    sudo "$@"
  else
    die "操作需要管理员权限，但未找到 sudo"
  fi
}

remove_binary() {
  local binary="$INSTALL_DIR/$BIN_NAME"

  if [[ ! -e "$binary" && ! -L "$binary" ]]; then
    echo "程序不存在，跳过: $binary"
  elif [[ -w "$INSTALL_DIR" ]]; then
    rm -f -- "$binary"
    echo "已删除程序: $binary"
  else
    run_privileged rm -f -- "$binary"
    echo "已删除程序: $binary"
  fi
}

confirm_purge() {
  local answer
  local prompt="将删除配置 $CONFIG_DIR 和数据 $DATA_DIR，确认继续？[y/N] "

  if [[ -r /dev/tty ]]; then
    read -r -p "$prompt" answer </dev/tty
  else
    die "--purge 需要交互确认，请在终端中运行本地 uninstall.sh"
  fi

  [[ "$answer" == "y" || "$answer" == "Y" ]] || {
    echo "已取消彻底卸载。"
    exit 0
  }
}

validate_purge_path() {
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

remove_tree() {
  local path="$1"

  if [[ -w "$(dirname "$path")" ]]; then
    rm -rf -- "$path"
  else
    run_privileged rm -rf -- "$path"
  fi
}

purge_files() {
  local config_path
  local data_path

  config_path="$(validate_purge_path "$CONFIG_DIR")"
  data_path="$(validate_purge_path "$DATA_DIR")"

  if [[ -e "$config_path" ]]; then
    remove_tree "$config_path"
    echo "已删除配置: $config_path"
  fi
  if [[ -e "$data_path" ]]; then
    remove_tree "$data_path"
    echo "已删除数据: $data_path"
  fi
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --purge)
        PURGE=true
        ;;
      -h|--help)
        echo "用法: ./uninstall.sh [--purge]"
        echo "默认只删除程序；--purge 同时删除配置和数据。"
        exit 0
        ;;
      *)
        die "未知参数: $1"
        ;;
    esac
    shift
  done
}

main() {
  [[ "$(uname -s)" == "Linux" ]] || die "仅支持 Linux 系统"
  parse_args "$@"

  if [[ "$PURGE" == true ]]; then
    confirm_purge
  fi

  remove_binary

  if [[ "$PURGE" == true ]]; then
    purge_files
    echo "彻底卸载完成。"
  else
    echo "卸载完成，配置和数据已保留。"
    echo "配置: $CONFIG_DIR"
    echo "数据: $DATA_DIR"
  fi

  echo "未自动停止或删除 systemd 服务。"
}

main "$@"
