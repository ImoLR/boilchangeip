#!/usr/bin/env bash
# boilchangeip 一键更新入口。
# 用法: curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/update.sh | sudo bash

set -Eeuo pipefail

REPO_SLUG="${BOIL_REPO_SLUG:-ImoLR/boilchangeip}"
REPO_URL="${BOIL_REPO_URL:-https://github.com/${REPO_SLUG}.git}"
BRANCH="${BOIL_BRANCH:-main}"
VERSION="${BOIL_VERSION:-}"
TAG="${BOIL_TAG:-$VERSION}"
BIN_NAME="${BOIL_BIN_NAME:-boil}"
SERVICE_NAME="${BOIL_SERVICE_NAME:-boil}"
INSTALL_DIR="${BOIL_INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="${BOIL_CONFIG_DIR:-/etc/boil}"
BACKUP_ROOT="${BOIL_BACKUP_ROOT:-/var/backups/boilchangeip}"
RELEASE_API_URL="${BOIL_RELEASE_API_URL:-https://api.github.com/repos/${REPO_SLUG}/releases/latest}"
RELEASE_DOWNLOAD_BASE="${BOIL_RELEASE_DOWNLOAD_BASE:-https://github.com/${REPO_SLUG}/releases/download}"
TMP_DIR=""
CARGO_BIN=""
CONFIG_BACKUP=""
BINARY_BACKUP=""
WAS_ACTIVE=false
INSTALL_DONE=false
ARTIFACT_PATH=""
ARTIFACT_SOURCE=""
ARTIFACT_REF=""

die() {
  echo "错误: $*" >&2
  exit 1
}

usage() {
  cat <<EOF
用法: update.sh [--help]

默认优先下载 GitHub Release 预编译二进制；当前架构没有 Release Asset 时才回退源码编译。

环境变量:
  BOIL_BRANCH=main|develop      默认 main；develop 会直接源码编译
  BOIL_VERSION=2.1.2            指定版本，自动补 v 前缀
  BOIL_TAG=v2.1.2               指定 tag，优先于 BOIL_VERSION
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

detect_arch() {
  case "$(uname -m)" in
    x86_64|amd64)
      echo "amd64"
      ;;
    *)
      return 1
      ;;
  esac
}

latest_release_tag() {
  curl -fsSL "$RELEASE_API_URL" |
    sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
    head -n 1
}

selected_release_tag() {
  local tag
  tag="$(normalize_tag "$TAG")"
  if [[ -n "$tag" ]]; then
    echo "$tag"
    return
  fi

  if [[ "$BRANCH" != "main" ]]; then
    return 1
  fi

  latest_release_tag
}

download_release_binary() {
  local tag="$1"
  local arch="$2"
  local asset="boil-linux-${arch}"
  local url="${RELEASE_DOWNLOAD_BASE}/${tag}/${asset}"
  local destination="$TMP_DIR/$asset"

  if curl -fL --silent --show-error "$url" -o "$destination"; then
    [[ -s "$destination" ]] || die "下载的 Release 二进制为空，已停止更新"
    chmod 0755 "$destination"
    "$destination" --version >/dev/null
    ARTIFACT_PATH="$destination"
    ARTIFACT_SOURCE="release"
    ARTIFACT_REF="$tag/$asset"
    return 0
  fi

  rm -f -- "$destination"
  return 1
}

find_cargo() {
  if command -v cargo >/dev/null 2>&1; then
    command -v cargo
    return
  fi

  if [[ -x /root/.cargo/bin/cargo ]]; then
    echo /root/.cargo/bin/cargo
    return
  fi

  if [[ -n "${SUDO_USER:-}" && -x "/home/${SUDO_USER}/.cargo/bin/cargo" ]]; then
    echo "/home/${SUDO_USER}/.cargo/bin/cargo"
    return
  fi

  return 1
}

require_source_tools() {
  require_command git
  CARGO_BIN="$(find_cargo)" || die "未检测到 Rust/Cargo。当前 Release 没有适配本机架构的预编译二进制，请先安装 Rust/Cargo 后重试。"
}

timestamp_utc() {
  date -u +"%Y%m%dT%H%M%SZ"
}

checkout_target_ref() {
  local source_dir="$1"
  local tag
  tag="$(normalize_tag "$TAG")"

  git -C "$source_dir" fetch origin --tags
  if [[ -n "$tag" ]]; then
    git -C "$source_dir" rev-parse --verify --quiet "refs/tags/$tag" >/dev/null ||
      die "指定版本不存在: $tag"
    git -C "$source_dir" checkout --quiet --detach "refs/tags/$tag"
  else
    git -C "$source_dir" ls-remote --exit-code --heads origin "$BRANCH" >/dev/null ||
      die "远程分支不存在: $BRANCH"
    git -C "$source_dir" checkout --quiet -B "$BRANCH" "origin/$BRANCH"
  fi
}

prepare_source_artifact() {
  require_source_tools

  local source_dir="$TMP_DIR/source"
  echo "未找到匹配的 Release 二进制，回退源码编译: $REPO_URL" >&2
  git clone --quiet "$REPO_URL" "$source_dir"
  checkout_target_ref "$source_dir"
  echo "编译 Release 版本..." >&2
  "$CARGO_BIN" build --release --manifest-path "$source_dir/Cargo.toml"

  ARTIFACT_PATH="$source_dir/target/release/$BIN_NAME"
  [[ -f "$ARTIFACT_PATH" ]] || die "未找到构建产物: $ARTIFACT_PATH"
  ARTIFACT_SOURCE="source"
  ARTIFACT_REF="$(git -C "$source_dir" rev-parse --short HEAD)"
}

prepare_artifact() {
  TMP_DIR="$(mktemp -d -t boilchangeip-update.XXXXXX)"

  local arch tag
  if arch="$(detect_arch)" && tag="$(selected_release_tag)" && [[ -n "$tag" ]]; then
    echo "尝试下载 Release 二进制: ${tag} (${arch})" >&2
    if download_release_binary "$tag" "$arch"; then
      return
    fi
  fi

  prepare_source_artifact
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
  [[ -f "$binary" ]] || return 0

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
  if [[ -n "$BINARY_BACKUP" ]]; then
    restore_binary || true
  fi
  restore_config || true
  if [[ "$WAS_ACTIVE" == true ]] && systemctl_available; then
    run_privileged systemctl restart "$SERVICE_NAME" || true
  fi
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
  run_privileged install -d -m 0755 "$INSTALL_DIR"
  run_privileged install -m 0755 "$ARTIFACT_PATH" "$INSTALL_DIR/$BIN_NAME"
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
  require_command curl
  require_command install
  require_command mktemp

  local current_version
  current_version="$(installed_version)"
  prepare_artifact

  backup_config_dir
  backup_binary
  trap rollback_on_error ERR

  stop_service_if_needed
  install_artifact
  "$INSTALL_DIR/$BIN_NAME" --version >/dev/null
  restart_service_if_needed

  trap - ERR
  echo
  echo "更新完成。"
  echo "更新来源: $ARTIFACT_SOURCE ($ARTIFACT_REF)"
  echo "更新前版本: $current_version"
  echo "当前版本: $("${INSTALL_DIR}/${BIN_NAME}" --version)"
  echo "配置目录已保留: $CONFIG_DIR"
  if [[ -n "$CONFIG_BACKUP" ]]; then
    echo "本次配置备份保留在: $CONFIG_BACKUP"
  fi
  if [[ -n "$BINARY_BACKUP" ]]; then
    echo "旧二进制备份保留在: $BINARY_BACKUP"
  fi
}

main "$@"
