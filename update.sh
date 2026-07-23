#!/usr/bin/env bash
# boilchangeip 一键更新入口。
# 用法: curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/update.sh | sudo bash

set -euo pipefail

REPO_URL="${BOIL_REPO_URL:-https://github.com/ImoLR/boilchangeip.git}"
BRANCH="${BOIL_BRANCH:-main}"
VERSION="${BOIL_VERSION:-}"
TAG="${BOIL_TAG:-$VERSION}"
BIN_NAME="${BOIL_BIN_NAME:-boil}"
SERVICE_NAME="${BOIL_SERVICE_NAME:-boil}"
INSTALL_DIR="${BOIL_INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="${BOIL_CONFIG_DIR:-/etc/boil}"
BACKUP_ROOT="${BOIL_BACKUP_ROOT:-/var/backups/boilchangeip}"
SERVICE_PATH="/etc/systemd/system/${SERVICE_NAME}.service"
TMP_DIR=""
CONFIG_BACKUP=""
BINARY_BACKUP=""
WAS_ACTIVE=false
INSTALL_DONE=false

die() {
  echo "错误: $*" >&2
  exit 1
}

usage() {
  cat <<EOF
用法: update.sh [--help]

环境变量:
  BOIL_BRANCH=main|develop      默认 main
  BOIL_VERSION=2.1.1            指定版本，自动补 v 前缀
  BOIL_TAG=v2.1.1               指定 tag，优先于 BOIL_VERSION
  BOIL_INSTALL_DIR=/usr/local/bin
  BOIL_CONFIG_DIR=/etc/boil
  BOIL_SERVICE_NAME=boil
EOF
}

cleanup() {
  if [[ -n "$TMP_DIR" ]]; then
    rm -rf -- "$TMP_DIR"
  fi
  return 0
}
trap cleanup EXIT

run_privileged() {
  if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
    "$@"
  elif command -v sudo >/dev/null 2>&1; then
    sudo "$@"
  else
    die "操作需要管理员权限，但未找到 sudo"
  fi
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "缺少命令: $1"
}

systemctl_available() {
  command -v systemctl >/dev/null 2>&1 && [[ -d /run/systemd/system ]]
}

service_exists() {
  systemctl_available &&
    (systemctl list-unit-files "${SERVICE_NAME}.service" >/dev/null 2>&1 ||
      systemctl status "$SERVICE_NAME" >/dev/null 2>&1)
}

service_is_active() {
  systemctl_available && systemctl is-active --quiet "$SERVICE_NAME"
}

normalize_tag() {
  local tag="$1"
  [[ -z "$tag" ]] && return
  if [[ "$tag" == v* ]]; then
    echo "$tag"
  else
    echo "v$tag"
  fi
}

timestamp_utc() {
  date -u +"%Y%m%dT%H%M%SZ"
}

prepare_source() {
  TMP_DIR="$(mktemp -d -t boilchangeip-update.XXXXXX)"
  local source_dir="$TMP_DIR/source"
  echo "下载源码: $REPO_URL" >&2
  git clone --quiet "$REPO_URL" "$source_dir"
  git -C "$source_dir" fetch origin --tags

  local tag
  tag="$(normalize_tag "$TAG")"
  if [[ -n "$tag" ]]; then
    git -C "$source_dir" rev-parse --verify --quiet "refs/tags/$tag" >/dev/null ||
      die "指定版本不存在: $tag"
    git -C "$source_dir" checkout --quiet --detach "refs/tags/$tag"
  else
    git -C "$source_dir" ls-remote --exit-code --heads origin "$BRANCH" >/dev/null ||
      die "远程分支不存在: $BRANCH"
    git -C "$source_dir" checkout --quiet -B "$BRANCH" "origin/$BRANCH"
  fi

  echo "$source_dir"
}

backup_config_dir() {
  local backup_path
  backup_path="$BACKUP_ROOT/config-$(timestamp_utc)"

  if [[ ! -d "$CONFIG_DIR" ]]; then
    echo "未检测到配置目录，跳过配置备份。"
    return
  fi

  run_privileged install -d -m 0700 "$BACKUP_ROOT"
  run_privileged cp -a -- "$CONFIG_DIR" "$backup_path"
  CONFIG_BACKUP="$backup_path"
  echo "已备份配置目录: $CONFIG_BACKUP"
}

backup_binary() {
  local binary="$INSTALL_DIR/$BIN_NAME"
  [[ -f "$binary" ]] || return

  local backup_path
  backup_path="$BACKUP_ROOT/${BIN_NAME}-$(timestamp_utc)"
  run_privileged install -d -m 0700 "$BACKUP_ROOT"
  run_privileged cp -a -- "$binary" "$backup_path"
  BINARY_BACKUP="$backup_path"
  echo "已备份旧二进制: $BINARY_BACKUP"
}

restore_config() {
  [[ -n "$CONFIG_BACKUP" ]] || return
  [[ -d "$CONFIG_BACKUP" ]] || return

  echo "恢复配置目录: $CONFIG_DIR"
  run_privileged rm -rf -- "$CONFIG_DIR"
  run_privileged cp -a -- "$CONFIG_BACKUP" "$CONFIG_DIR"
}

restore_binary() {
  [[ -n "$BINARY_BACKUP" ]] || return
  [[ -f "$BINARY_BACKUP" ]] || return

  echo "恢复旧二进制: $INSTALL_DIR/$BIN_NAME"
  run_privileged install -d -m 0755 "$INSTALL_DIR"
  run_privileged install -m 0755 "$BINARY_BACKUP" "$INSTALL_DIR/$BIN_NAME"
}

rollback_on_error() {
  echo "更新失败，开始恢复..." >&2
  if [[ "$INSTALL_DONE" == true ]]; then
    restore_binary || true
  fi
  restore_config || true
  if [[ "$WAS_ACTIVE" == true ]] && systemctl_available; then
    run_privileged systemctl restart "$SERVICE_NAME" || true
  fi
}

build_release() {
  local source_dir="$1"
  echo "编译 Release 版本..." >&2
  cargo build --release --manifest-path "$source_dir/Cargo.toml"
}

stop_service_if_needed() {
  if service_is_active; then
    WAS_ACTIVE=true
    echo "停止服务: $SERVICE_NAME"
    run_privileged systemctl stop "$SERVICE_NAME"
  fi
}

restart_service_if_needed() {
  if [[ "$WAS_ACTIVE" == true ]]; then
    echo "重启服务: $SERVICE_NAME"
    run_privileged systemctl restart "$SERVICE_NAME"
    sleep 2
    systemctl is-active --quiet "$SERVICE_NAME" ||
      die "服务重启后未处于 active 状态"
  elif service_exists; then
    echo "服务当前未运行，已保留未运行状态。"
  fi
}

install_artifact() {
  local source_dir="$1"
  local artifact="$source_dir/target/release/$BIN_NAME"
  [[ -f "$artifact" ]] || die "未找到构建产物: $artifact"

  run_privileged install -d -m 0755 "$INSTALL_DIR"
  run_privileged install -m 0755 "$artifact" "$INSTALL_DIR/$BIN_NAME"
  INSTALL_DONE=true
}

installed_version() {
  local binary="$INSTALL_DIR/$BIN_NAME"
  if [[ -x "$binary" ]]; then
    "$binary" --version
  else
    echo "未安装"
  fi
}

main() {
  case "${1:-}" in
    -h|--help)
      usage
      exit 0
      ;;
    "")
      ;;
    *)
      die "未知参数: $1"
      ;;
  esac

  [[ "$(uname -s)" == "Linux" ]] || die "仅支持 Linux 系统"
  require_command git
  require_command cargo
  require_command install
  require_command mktemp

  local source_dir
  source_dir="$(prepare_source)"
  local target_commit
  target_commit="$(git -C "$source_dir" rev-parse HEAD)"
  local current_version
  current_version="$(installed_version)"

  backup_config_dir
  backup_binary
  trap rollback_on_error ERR

  build_release "$source_dir"
  stop_service_if_needed
  install_artifact "$source_dir"
  restart_service_if_needed

  trap - ERR
  echo
  echo "更新完成。"
  echo "目标提交: $(git -C "$source_dir" rev-parse --short "$target_commit")"
  echo "更新前版本: $current_version"
  echo "当前版本: $("${INSTALL_DIR}/${BIN_NAME}" --version)"
  echo "配置目录已保留: $CONFIG_DIR"
  [[ -n "$CONFIG_BACKUP" ]] && echo "本次配置备份保留在: $CONFIG_BACKUP"
  [[ -n "$BINARY_BACKUP" ]] && echo "旧二进制备份保留在: $BINARY_BACKUP"
}

main "$@"
