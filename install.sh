#!/usr/bin/env bash
# boilchangeip 首次安装入口。
# 用法: curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/install.sh | sudo bash

set -euo pipefail

REPO_URL="${BOIL_REPO_URL:-https://github.com/ImoLR/boilchangeip.git}"
BRANCH="${BOIL_BRANCH:-main}"
VERSION="${BOIL_VERSION:-}"
TAG="${BOIL_TAG:-$VERSION}"
BIN_NAME="${BOIL_BIN_NAME:-boil}"
SERVICE_NAME="${BOIL_SERVICE_NAME:-boil}"
INSTALL_DIR="${BOIL_INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="${BOIL_CONFIG_DIR:-/etc/boil}"
SERVICE_PATH="/etc/systemd/system/${SERVICE_NAME}.service"
TMP_DIR=""

die() {
  echo "错误: $*" >&2
  exit 1
}

usage() {
  cat <<EOF
用法: install.sh [--help]

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

detect_package_manager() {
  for manager in apt-get dnf yum pacman zypper; do
    if command -v "$manager" >/dev/null 2>&1; then
      echo "$manager"
      return
    fi
  done
  return 1
}

install_dependencies() {
  local missing=()
  for command_name in git cargo systemctl install mktemp; do
    command -v "$command_name" >/dev/null 2>&1 || missing+=("$command_name")
  done

  [[ "${#missing[@]}" -eq 0 ]] && return

  local manager
  manager="$(detect_package_manager)" || die "缺少依赖: ${missing[*]}，且无法识别包管理器"

  echo "安装缺失依赖: ${missing[*]}"
  case "$manager" in
    apt-get)
      run_privileged apt-get update
      run_privileged apt-get install -y git cargo build-essential pkg-config ca-certificates
      ;;
    dnf)
      run_privileged dnf install -y git cargo gcc make pkgconf-pkg-config ca-certificates
      ;;
    yum)
      run_privileged yum install -y git cargo gcc make pkgconfig ca-certificates
      ;;
    pacman)
      run_privileged pacman -Sy --needed --noconfirm git rust base-devel pkgconf ca-certificates
      ;;
    zypper)
      run_privileged zypper --non-interactive install git cargo gcc make pkg-config ca-certificates
      ;;
    *)
      die "不支持的包管理器: $manager"
      ;;
  esac
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

checkout_target_ref() {
  local source_dir="$1"
  local tag
  tag="$(normalize_tag "$TAG")"

  git -C "$source_dir" fetch origin --tags
  if [[ -n "$tag" ]]; then
    git -C "$source_dir" rev-parse --verify --quiet "refs/tags/$tag" >/dev/null ||
      die "指定版本不存在: $tag"
    git -C "$source_dir" checkout --detach "refs/tags/$tag"
  else
    git -C "$source_dir" ls-remote --exit-code --heads origin "$BRANCH" >/dev/null ||
      die "远程分支不存在: $BRANCH"
    git -C "$source_dir" checkout -B "$BRANCH" "origin/$BRANCH"
  fi
}

prepare_source() {
  TMP_DIR="$(mktemp -d -t boilchangeip-install.XXXXXX)"
  local source_dir="$TMP_DIR/source"
  echo "下载源码: $REPO_URL" >&2
  git clone --quiet "$REPO_URL" "$source_dir"
  checkout_target_ref "$source_dir"
  echo "$source_dir"
}

build_release() {
  local source_dir="$1"
  echo "编译 Release 版本..." >&2
  cargo build --release --manifest-path "$source_dir/Cargo.toml"
}

install_artifact() {
  local source_dir="$1"
  local artifact="$source_dir/target/release/$BIN_NAME"
  local destination="$INSTALL_DIR/$BIN_NAME"

  [[ -f "$artifact" ]] || die "未找到构建产物: $artifact"
  run_privileged install -d -m 0755 "$INSTALL_DIR"
  run_privileged install -m 0755 "$artifact" "$destination"
}

write_service_file() {
  local binary="$INSTALL_DIR/$BIN_NAME"
  local tmp
  tmp="$(mktemp)"
  cat >"$tmp" <<EOF
[Unit]
Description=boilchangeip daemon
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=$CONFIG_DIR
ExecStart=$binary daemon
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF
  run_privileged install -m 0644 "$tmp" "$SERVICE_PATH"
  rm -f -- "$tmp"
}

configure_if_missing() {
  run_privileged install -d -m 0750 "$CONFIG_DIR"
  if [[ -f "$CONFIG_DIR/config.env" ]]; then
    echo "已保留现有配置: $CONFIG_DIR/config.env"
    return
  fi

  if [[ ! -r /dev/tty ]]; then
    die "缺少 $CONFIG_DIR/config.env，且当前环境无法交互配置"
  fi

  echo "首次安装需要配置 Boil Token 和可选 Telegram 信息。"
  "$INSTALL_DIR/$BIN_NAME" setup </dev/tty >/dev/tty
  [[ -f "$CONFIG_DIR/config.env" ]] || die "配置向导未生成 $CONFIG_DIR/config.env"
}

install_systemd_service() {
  require_command systemctl
  write_service_file
  run_privileged systemctl daemon-reload
  run_privileged systemctl enable "$SERVICE_NAME"
}

start_and_verify_service() {
  require_command systemctl
  run_privileged systemctl restart "$SERVICE_NAME"
  sleep 2
  systemctl is-active --quiet "$SERVICE_NAME" ||
    die "服务启动失败，请运行 journalctl -u $SERVICE_NAME -n 80 --no-pager 查看日志"
}

print_summary() {
  local source_dir="$1"
  echo
  echo "安装完成。"
  echo "程序路径: $INSTALL_DIR/$BIN_NAME"
  echo "配置目录: $CONFIG_DIR"
  echo "服务名称: $SERVICE_NAME"
  echo "源码版本: $(git -C "$source_dir" rev-parse --short HEAD)"
  echo "程序版本: $("${INSTALL_DIR}/${BIN_NAME}" --version)"
  echo "后续升级: curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/update.sh | sudo bash"
  echo "查看服务: systemctl status $SERVICE_NAME"
  echo "查看日志: journalctl -fu $SERVICE_NAME"
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
  install_dependencies
  local source_dir
  source_dir="$(prepare_source)"
  build_release "$source_dir"
  install_artifact "$source_dir"
  configure_if_missing
  install_systemd_service
  start_and_verify_service
  print_summary "$source_dir"
}

main "$@"
