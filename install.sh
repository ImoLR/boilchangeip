#!/usr/bin/env bash
# boilchangeip 首次安装入口。
# 用法: curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/install.sh | bash

set -euo pipefail

REPO_URL="${BOIL_REPO_URL:-https://github.com/ImoLR/boilchangeip.git}"
BRANCH="${BOIL_BRANCH:-main}"
VERSION="${BOIL_VERSION:-}"
TAG="${BOIL_TAG:-$VERSION}"
MANAGED_ROOT="${BOIL_MANAGED_ROOT:-/opt/boilchangeip}"
SOURCE_DIR="${BOIL_SOURCE_DIR:-$MANAGED_ROOT/source}"

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
  for command_name in git cargo systemctl install; do
    command -v "$command_name" >/dev/null 2>&1 || missing+=("$command_name")
  done

  if [[ "${#missing[@]}" -eq 0 ]]; then
    return
  fi

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

ensure_parent_dir() {
  local parent
  parent="$(dirname "$SOURCE_DIR")"
  if [[ -d "$parent" ]]; then
    return
  fi
  run_privileged mkdir -p "$parent"
  run_privileged chown "$(id -u):$(id -g)" "$parent"
}

ensure_clean_worktree() {
  [[ -z "$(git -C "$SOURCE_DIR" status --porcelain)" ]] ||
    die "源码目录存在未提交修改，拒绝覆盖: $SOURCE_DIR"
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

target_ref_description() {
  local tag
  tag="$(normalize_tag "$TAG")"
  if [[ -n "$tag" ]]; then
    echo "tag $tag"
  else
    echo "branch $BRANCH"
  fi
}

checkout_target_ref() {
  local tag
  tag="$(normalize_tag "$TAG")"
  git -C "$SOURCE_DIR" fetch origin --tags
  if [[ -n "$tag" ]]; then
    git -C "$SOURCE_DIR" rev-parse --verify --quiet "refs/tags/$tag" >/dev/null ||
      die "指定版本不存在: $tag"
    git -C "$SOURCE_DIR" checkout --detach "refs/tags/$tag"
  else
    git -C "$SOURCE_DIR" ls-remote --exit-code --heads origin "$BRANCH" >/dev/null ||
      die "远程分支不存在: $BRANCH"
    if git -C "$SOURCE_DIR" show-ref --verify --quiet "refs/heads/$BRANCH"; then
      git -C "$SOURCE_DIR" checkout "$BRANCH"
    else
      git -C "$SOURCE_DIR" checkout -B "$BRANCH" "origin/$BRANCH"
    fi
    git -C "$SOURCE_DIR" merge --ff-only "origin/$BRANCH"
  fi
}

prepare_source() {
  if [[ -d "$SOURCE_DIR/.git" ]]; then
    local origin_url
    origin_url="$(git -C "$SOURCE_DIR" remote get-url origin)"
    [[ "$origin_url" == "$REPO_URL" ]] ||
      die "源码目录 origin 为 $origin_url，预期为 $REPO_URL"
    ensure_clean_worktree
    checkout_target_ref
    return
  fi

  [[ ! -e "$SOURCE_DIR" ]] || die "源码目录已存在但不是 Git 仓库: $SOURCE_DIR"
  ensure_parent_dir
  echo "克隆源码: $REPO_URL ($(target_ref_description))"
  git clone "$REPO_URL" "$SOURCE_DIR"
  checkout_target_ref
}

main() {
  [[ "$(uname -s)" == "Linux" ]] || die "仅支持 Linux 系统"
  install_dependencies
  prepare_source

  # 公共函数来自安装器维护的源码目录，安装和更新共用同一套流程。
  # shellcheck source=install-common.sh
  source "$SOURCE_DIR/install-common.sh"
  install_from_source "$SOURCE_DIR"
  configure_if_missing
  install_systemd_service
  start_and_verify_service

  echo
  echo "安装完成。"
  print_install_summary "$SOURCE_DIR"
}

main "$@"
