#!/usr/bin/env bash
# boilchangeip 首次安装入口。
# 用法: curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/install.sh | sudo bash

set -euo pipefail

REPO_SLUG="${BOIL_REPO_SLUG:-ImoLR/boilchangeip}"
REPO_URL="${BOIL_REPO_URL:-https://github.com/${REPO_SLUG}.git}"
BRANCH="${BOIL_BRANCH:-main}"
VERSION="${BOIL_VERSION:-}"
TAG="${BOIL_TAG:-$VERSION}"
BIN_NAME="${BOIL_BIN_NAME:-boil}"
SERVICE_NAME="${BOIL_SERVICE_NAME:-boil}"
INSTALL_DIR="${BOIL_INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="${BOIL_CONFIG_DIR:-/etc/boil}"
SERVICE_PATH="/etc/systemd/system/${SERVICE_NAME}.service"
RELEASE_API_URL="${BOIL_RELEASE_API_URL:-https://api.github.com/repos/${REPO_SLUG}/releases/latest}"
RELEASE_DOWNLOAD_BASE="${BOIL_RELEASE_DOWNLOAD_BASE:-https://github.com/${REPO_SLUG}/releases/download}"
TMP_DIR=""
CARGO_BIN=""
ARTIFACT_PATH=""
ARTIFACT_SOURCE=""
ARTIFACT_REF=""

die() {
  echo "错误: $*" >&2
  exit 1
}

usage() {
  cat <<EOF
用法: install.sh [--help]

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

find_sha256_tool() {
  if command -v sha256sum >/dev/null 2>&1; then
    echo "sha256sum"
    return
  fi

  if command -v openssl >/dev/null 2>&1; then
    echo "openssl"
    return
  fi

  return 1
}

expected_sha256() {
  local asset="$1"
  local sums_file="$2"
  sed -n "s/^[[:space:]]*\\([0-9a-fA-F]\\{64\\}\\)[[:space:]]\\{1,\\}\\*\\{0,1\\}${asset}\$/\\1/p" "$sums_file" |
    head -n 1
}

verify_release_checksum() {
  local asset="$1"
  local sums_file="$2"
  local tool expected actual

  tool="$(find_sha256_tool)" || die "缺少 SHA256 校验工具。请安装 sha256sum 或 openssl 后重试。"
  expected="$(expected_sha256 "$asset" "$sums_file")"
  [[ -n "$expected" ]] || die "SHA256SUMS 中缺少 $asset 的校验记录"

  case "$tool" in
    sha256sum)
      printf '%s  %s\n' "$expected" "$asset" | (cd "$TMP_DIR" && sha256sum -c -) ||
        die "$asset SHA256 校验失败"
      ;;
    openssl)
      actual="$(openssl dgst -sha256 "$TMP_DIR/$asset" | sed -n 's/^.*= //p')"
      [[ "$actual" == "$expected" ]] || die "$asset SHA256 校验失败"
      ;;
    *)
      die "未知 SHA256 校验工具: $tool"
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

detect_arch() {
  case "$(uname -m)" in
    x86_64|amd64)
      echo "amd64"
      ;;
    aarch64|arm64)
      echo "arm64"
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
  local sums_url="${RELEASE_DOWNLOAD_BASE}/${tag}/SHA256SUMS"
  local destination="$TMP_DIR/$asset"
  local sums_file="$TMP_DIR/SHA256SUMS"

  if curl -fL --silent --show-error "$url" -o "$destination"; then
    curl -fL --silent --show-error "$sums_url" -o "$sums_file" ||
      die "无法下载 SHA256SUMS，已停止安装"
    verify_release_checksum "$asset" "$sums_file"
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
  TMP_DIR="$(mktemp -d -t boilchangeip-install.XXXXXX)"

  local arch tag
  if arch="$(detect_arch)" && tag="$(selected_release_tag)" && [[ -n "$tag" ]]; then
    echo "尝试下载 Release 二进制: ${tag} (${arch})" >&2
    if download_release_binary "$tag" "$arch"; then
      return
    fi
  fi

  prepare_source_artifact
}

install_artifact() {
  local destination="$INSTALL_DIR/$BIN_NAME"
  run_privileged install -d -m 0755 "$INSTALL_DIR"
  run_privileged install -m 0755 "$ARTIFACT_PATH" "$destination"
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
  echo
  echo "安装完成。"
  echo "程序路径: $INSTALL_DIR/$BIN_NAME"
  echo "配置目录: $CONFIG_DIR"
  echo "服务名称: $SERVICE_NAME"
  echo "安装来源: $ARTIFACT_SOURCE ($ARTIFACT_REF)"
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
  require_command curl
  require_command install
  require_command mktemp

  prepare_artifact
  install_artifact
  configure_if_missing
  install_systemd_service
  start_and_verify_service
  print_summary
}

main "$@"
