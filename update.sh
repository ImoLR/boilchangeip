#!/usr/bin/env bash
# boilchangeip 一键更新入口。
# 用法: curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/update.sh | bash

set -euo pipefail

REPO_URL="${BOIL_REPO_URL:-https://github.com/ImoLR/boilchangeip.git}"
MANAGED_ROOT="${BOIL_MANAGED_ROOT:-/opt/boilchangeip}"
SOURCE_DIR="${BOIL_SOURCE_DIR:-$MANAGED_ROOT/source}"
REQUESTED_BRANCH="${BOIL_BRANCH:-}"
VERSION="${BOIL_VERSION:-}"
TAG="${BOIL_TAG:-$VERSION}"
CONFIG_DIR="${BOIL_CONFIG_DIR:-/etc/boil}"
BACKUP_ROOT="${BOIL_BACKUP_ROOT:-/var/backups/boilchangeip}"
BACKUP_PATH=""
WAS_ACTIVE=false
SOURCE_CHANGED=false

die() {
  echo "错误: $*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "缺少命令: $1"
}

timestamp_utc() {
  date -u +"%Y%m%dT%H%M%SZ"
}

normalize_tag() {
  local tag="$1"
  if [[ -z "$tag" ]]; then
    return
  fi
  if [[ "$tag" == v* ]]; then
    echo "$tag"
  else
    echo "v$tag"
  fi
}

backup_config_dir_for_update() {
  local backup_path
  backup_path="$BACKUP_ROOT/config-$(timestamp_utc)"

  if [[ ! -d "$CONFIG_DIR" ]]; then
    echo "未检测到配置目录，跳过配置备份。" >&2
    echo ""
    return
  fi

  if [[ -w "$(dirname "$BACKUP_ROOT")" ]]; then
    mkdir -p "$BACKUP_ROOT"
    chmod 0700 "$BACKUP_ROOT"
    cp -a -- "$CONFIG_DIR" "$backup_path"
  else
    run_privileged mkdir -p "$BACKUP_ROOT"
    run_privileged chmod 0700 "$BACKUP_ROOT"
    run_privileged cp -a -- "$CONFIG_DIR" "$backup_path"
  fi

  echo "已备份配置目录: $backup_path" >&2
  echo "$backup_path"
}

restore_config_dir_for_update() {
  local backup_path="$1"

  [[ -n "$backup_path" ]] || return
  [[ -d "$backup_path" ]] || {
    echo "配置备份不存在，无法恢复: $backup_path" >&2
    return
  }

  echo "恢复配置目录: $CONFIG_DIR"
  if [[ -w "$(dirname "$CONFIG_DIR")" ]]; then
    rm -rf -- "$CONFIG_DIR"
    cp -a -- "$backup_path" "$CONFIG_DIR"
  else
    run_privileged rm -rf -- "$CONFIG_DIR"
    run_privileged cp -a -- "$backup_path" "$CONFIG_DIR"
  fi
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

checkout_branch() {
  local branch="$1"
  git -C "$SOURCE_DIR" ls-remote --exit-code --heads origin "$branch" >/dev/null ||
    die "远程分支不存在: $branch"
  if git -C "$SOURCE_DIR" show-ref --verify --quiet "refs/heads/$branch"; then
    git -C "$SOURCE_DIR" checkout "$branch"
  else
    git -C "$SOURCE_DIR" checkout -B "$branch" "origin/$branch"
  fi
}

checkout_tag() {
  local tag="$1"
  git -C "$SOURCE_DIR" rev-parse --verify --quiet "refs/tags/$tag" >/dev/null ||
    die "指定版本不存在: $tag"
  git -C "$SOURCE_DIR" checkout --detach "refs/tags/$tag"
}

update_source() {
  local before
  local after
  local tag

  git -C "$SOURCE_DIR" fetch origin --tags
  before="$(git -C "$SOURCE_DIR" rev-parse HEAD)"
  tag="$(normalize_tag "$TAG")"

  if [[ -n "$tag" ]]; then
    checkout_tag "$tag"
  else
    local branch
    branch="$(detect_target_branch)"
    checkout_branch "$branch"
    git -C "$SOURCE_DIR" merge --ff-only "origin/$branch"
  fi

  after="$(git -C "$SOURCE_DIR" rev-parse HEAD)"
  if [[ "$before" == "$after" ]]; then
    echo "源码已是最新版: $(git -C "$SOURCE_DIR" rev-parse --short HEAD)"
    SOURCE_CHANGED=false
    return
  fi

  echo "源码已更新: $(git -C "$SOURCE_DIR" rev-parse --short "$before") -> $(git -C "$SOURCE_DIR" rev-parse --short "$after")"
  SOURCE_CHANGED=true
}

restore_after_failure() {
  echo "更新失败，开始恢复..."
  restore_config_dir_for_update "$BACKUP_PATH" || true
  if [[ "$WAS_ACTIVE" == true ]]; then
    restart_service_if_enabled || true
  fi
}

main() {
  [[ "$(uname -s)" == "Linux" ]] || die "仅支持 Linux 系统"
  require_command git
  require_command cargo

  ensure_managed_source
  # shellcheck source=install-common.sh
  source "$SOURCE_DIR/install-common.sh"

  if service_is_active; then
    WAS_ACTIVE=true
  fi

  BACKUP_PATH="$(backup_config_dir_for_update)"
  trap restore_after_failure ERR

  update_source
  # 源码更新后重新加载公共函数，确保使用目标版本的构建和安装逻辑。
  # shellcheck source=install-common.sh
  source "$SOURCE_DIR/install-common.sh"

  install_dependencies
  build_release "$SOURCE_DIR"

  stop_service_if_present
  install_artifact "$SOURCE_DIR/target/release/$BIN_NAME"
  restart_service_if_enabled

  if service_is_active; then
    echo "服务已运行: $SERVICE_NAME"
  elif [[ "$WAS_ACTIVE" == true ]]; then
    die "服务重启后未处于 active 状态"
  else
    echo "服务未运行，如需启动请执行: systemctl start $SERVICE_NAME"
  fi

  trap - ERR

  echo
  if [[ "$SOURCE_CHANGED" == true ]]; then
    echo "更新完成。"
  else
    echo "已是最新版，二进制已重新验证安装。"
  fi
  if [[ -n "$BACKUP_PATH" ]]; then
    echo "本次配置备份保留在: $BACKUP_PATH"
  fi
  print_install_summary "$SOURCE_DIR"
}

main "$@"
