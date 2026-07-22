#!/usr/bin/env bash
# boilchangeip 源码安装入口。
# 用法: curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/install.sh | bash

set -Eeuo pipefail

REPO_URL="${BOIL_REPO_URL:-https://github.com/ImoLR/boilchangeip.git}"
BRANCH="${BOIL_BRANCH:-main}"
DATA_DIR="${BOIL_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/boilchangeip}"
SOURCE_DIR="${BOIL_SOURCE_DIR:-$DATA_DIR/source}"

die() {
  echo "错误: $*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "缺少命令: $1"
}

check_environment() {
  [[ "$(uname -s)" == "Linux" ]] || die "仅支持 Linux 系统"
  require_command git
  require_command cargo
}

prepare_source() {
  if [[ -d "$SOURCE_DIR/.git" ]]; then
    local current_branch
    local origin_url

    origin_url="$(git -C "$SOURCE_DIR" remote get-url origin)"
    [[ "$origin_url" == "$REPO_URL" ]] ||
      die "源码目录 origin 为 $origin_url，预期为 $REPO_URL"
    current_branch="$(git -C "$SOURCE_DIR" branch --show-current)"
    [[ "$current_branch" == "$BRANCH" ]] ||
      die "源码目录当前分支为 $current_branch，预期为 $BRANCH"

    echo "更新源码: $SOURCE_DIR"
    git -C "$SOURCE_DIR" fetch origin "$BRANCH"
    git -C "$SOURCE_DIR" merge --ff-only "origin/$BRANCH"
    return
  fi

  [[ ! -e "$SOURCE_DIR" ]] || die "源码目录已存在但不是 Git 仓库: $SOURCE_DIR"
  mkdir -p "$(dirname "$SOURCE_DIR")"
  echo "克隆源码: $REPO_URL ($BRANCH)"
  git clone --branch "$BRANCH" --single-branch "$REPO_URL" "$SOURCE_DIR"
}

main() {
  check_environment
  prepare_source

  # 公共函数来自刚刚更新的仓库，安装和更新使用同一套构建安装流程。
  # shellcheck source=install-common.sh
  source "$SOURCE_DIR/install-common.sh"
  install_from_source "$SOURCE_DIR"

  echo
  echo "安装完成。"
  print_install_summary "$SOURCE_DIR"
}

main "$@"
