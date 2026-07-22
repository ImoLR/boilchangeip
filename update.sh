#!/usr/bin/env bash
# boilchangeip 源码更新入口。
# 用法: curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/update.sh | bash

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

main() {
  [[ "$(uname -s)" == "Linux" ]] || die "仅支持 Linux 系统"
  require_command git
  require_command cargo
  [[ -d "$SOURCE_DIR/.git" ]] || die "未找到已安装源码，请先运行 install.sh: $SOURCE_DIR"

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

  # 更新源码后再加载公共函数，确保使用仓库中的最新安装流程。
  # shellcheck source=install-common.sh
  source "$SOURCE_DIR/install-common.sh"
  install_from_source "$SOURCE_DIR"

  echo
  echo "更新完成。"
  print_install_summary "$SOURCE_DIR"
}

main "$@"
