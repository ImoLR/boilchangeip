#!/usr/bin/env bash
# boilchangeip 一键更新入口。
# 用法: curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/update.sh | bash

set -euo pipefail

REPO_URL="${BOIL_REPO_URL:-https://github.com/ImoLR/boilchangeip.git}"
MANAGED_ROOT="${BOIL_MANAGED_ROOT:-/opt/boilchangeip}"
SOURCE_DIR="${BOIL_SOURCE_DIR:-$MANAGED_ROOT/source}"
REQUESTED_BRANCH="${BOIL_BRANCH:-}"

die() {
  echo "错误: $*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "缺少命令: $1"
}

ensure_managed_source() {
  [[ -d "$SOURCE_DIR/.git" ]] || die "未找到安装器维护的源码目录，请先运行 install.sh: $SOURCE_DIR"
  local origin_url
  origin_url="$(git -C "$SOURCE_DIR" remote get-url origin)"
  [[ "$origin_url" == "$REPO_URL" ]] ||
    die "源码目录 origin 为 $origin_url，预期为 $REPO_URL"
  [[ -z "$(git -C "$SOURCE_DIR" status --porcelain)" ]] ||
    die "源码目录存在未提交修改，拒绝更新: $SOURCE_DIR"
}

remote_branch_exists() {
  local branch="$1"
  git -C "$SOURCE_DIR" show-ref --verify --quiet "refs/remotes/origin/$branch"
}

detect_target_branch() {
  if [[ -n "$REQUESTED_BRANCH" ]]; then
    echo "$REQUESTED_BRANCH"
    return
  fi

  local current_branch
  current_branch="$(git -C "$SOURCE_DIR" branch --show-current)"
  if [[ "$current_branch" == "main" || "$current_branch" == "develop" ]]; then
    echo "$current_branch"
    return
  fi

  if remote_branch_exists main && git -C "$SOURCE_DIR" merge-base --is-ancestor HEAD origin/main; then
    echo "main"
    return
  fi
  if remote_branch_exists develop && git -C "$SOURCE_DIR" merge-base --is-ancestor HEAD origin/develop; then
    echo "develop"
    return
  fi

  if remote_branch_exists main; then
    echo "main"
  elif remote_branch_exists develop; then
    echo "develop"
  else
    die "远程没有 main 或 develop 分支"
  fi
}

checkout_target_branch() {
  local branch="$1"
  local current_branch

  current_branch="$(git -C "$SOURCE_DIR" branch --show-current)"
  if [[ "$current_branch" == "$branch" ]]; then
    return
  fi

  echo "切换源码分支: $branch"
  if git -C "$SOURCE_DIR" show-ref --verify --quiet "refs/heads/$branch"; then
    git -C "$SOURCE_DIR" checkout "$branch"
  else
    git -C "$SOURCE_DIR" checkout -B "$branch" "origin/$branch"
  fi
}

update_source() {
  local branch="$1"
  local before
  local after

  git -C "$SOURCE_DIR" fetch origin --tags
  remote_branch_exists "$branch" || die "远程不存在分支: $branch"
  checkout_target_branch "$branch"

  before="$(git -C "$SOURCE_DIR" rev-parse HEAD)"
  git -C "$SOURCE_DIR" merge --ff-only "origin/$branch"
  after="$(git -C "$SOURCE_DIR" rev-parse HEAD)"

  if [[ "$before" == "$after" ]]; then
    echo "源码已是最新版: $branch ($(git -C "$SOURCE_DIR" rev-parse --short HEAD))"
    return 1
  fi

  echo "源码已更新: $(git -C "$SOURCE_DIR" rev-parse --short "$before") -> $(git -C "$SOURCE_DIR" rev-parse --short "$after")"
  return 0
}

main() {
  [[ "$(uname -s)" == "Linux" ]] || die "仅支持 Linux 系统"
  require_command git
  require_command cargo

  ensure_managed_source
  # shellcheck source=install-common.sh
  source "$SOURCE_DIR/install-common.sh"

  local branch
  local changed=true
  git -C "$SOURCE_DIR" fetch origin --tags
  branch="$(detect_target_branch)"
  update_source "$branch" || changed=false

  install_dependencies
  build_release "$SOURCE_DIR"

  local was_active=false
  if service_is_active; then
    was_active=true
  fi

  stop_service_if_present
  trap 'if [[ "$was_active" == true ]]; then echo "更新失败，尝试恢复服务..."; restart_service_if_enabled || true; fi' ERR
  install_artifact "$SOURCE_DIR/target/release/$BIN_NAME"
  restart_service_if_enabled
  trap - ERR

  if service_is_active; then
    echo "服务已运行: $SERVICE_NAME"
  elif [[ "$was_active" == true ]]; then
    die "服务重启后未处于 active 状态"
  else
    echo "服务未运行，如需启动请执行: systemctl start $SERVICE_NAME"
  fi

  echo
  if [[ "$changed" == true ]]; then
    echo "更新完成。"
  else
    echo "已是最新版，二进制已重新验证安装。"
  fi
  print_install_summary "$SOURCE_DIR"
}

main "$@"
