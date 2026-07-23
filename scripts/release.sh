#!/usr/bin/env bash
# 本地发布脚本：构建 Linux 二进制并创建 GitHub Release。

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION_TAG="${1:-}"
AMD64_TARGET="x86_64-unknown-linux-musl"
AMD64_ASSET="boil-linux-amd64"
TMP_DIR=""
MUSL_CC=""

die() {
  echo "错误: $*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "缺少命令: $1"
}

find_musl_cc() {
  if command -v x86_64-linux-musl-gcc >/dev/null 2>&1; then
    command -v x86_64-linux-musl-gcc
    return
  fi

  if command -v musl-gcc >/dev/null 2>&1; then
    command -v musl-gcc
    return
  fi

  return 1
}

cleanup() {
  if [[ -n "$TMP_DIR" ]]; then
    rm -r -- "$TMP_DIR"
  fi
}
trap cleanup EXIT

usage() {
  cat <<EOF
用法: ./scripts/release.sh vX.Y.Z

示例:
  ./scripts/release.sh v2.2.1
EOF
}

source_version() {
  sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)"/\1/p' "$ROOT_DIR/Cargo.toml" |
    head -n 1
}

check_clean_worktree() {
  if [[ -n "$(git status --porcelain)" ]]; then
    git status --short
    die "工作区不干净，请先提交或清理后再发布"
  fi
}

check_tag_absent() {
  local tag="$1"

  if git tag --list "$tag" | grep -qx "$tag"; then
    die "本地 tag 已存在: $tag"
  fi

  if git ls-remote --tags origin "refs/tags/$tag" | grep -q .; then
    die "远程 tag 已存在: $tag"
  fi
}

build_asset() {
  local target="$1"
  local asset="$2"

  CC_x86_64_unknown_linux_musl="$MUSL_CC" cargo build --release --locked --target "$target"
  cp "$ROOT_DIR/target/$target/release/boil" "$TMP_DIR/$asset"
  chmod 0755 "$TMP_DIR/$asset"
}

validate_amd64() {
  local expected="$1"
  local actual

  actual="$("$TMP_DIR/$AMD64_ASSET" --version)"
  [[ "$actual" == "boil $expected" ]] ||
    die "$AMD64_ASSET 版本不匹配，期望: boil $expected，实际: $actual"
}

main() {
  if [[ "$VERSION_TAG" == "-h" || "$VERSION_TAG" == "--help" ]]; then
    usage
    exit 0
  fi

  [[ "$VERSION_TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
    die "版本参数格式必须为 vX.Y.Z，例如 v2.2.1"

  cd "$ROOT_DIR"

  require_command git
  require_command cargo
  require_command rustup
  require_command gh
  MUSL_CC="$(find_musl_cc)" || die "缺少 x86_64 musl C 编译器，请安装 musl-tools"

  local branch version commit release_url
  branch="$(git branch --show-current)"
  [[ "$branch" == "main" ]] || die "当前分支必须是 main，实际: $branch"

  check_clean_worktree

  version="${VERSION_TAG#v}"
  [[ "$(source_version)" == "$version" ]] ||
    die "源码版本号与参数不一致，Cargo.toml=$(source_version)，参数=$version"

  check_tag_absent "$VERSION_TAG"

  TMP_DIR="$(mktemp -d -t boil-release.XXXXXX)"

  rustup target add "$AMD64_TARGET"

  build_asset "$AMD64_TARGET" "$AMD64_ASSET"

  validate_amd64 "$version"

  git push origin main
  git tag -a "$VERSION_TAG" -m "Boil $VERSION_TAG"
  git push origin "$VERSION_TAG"

  gh release create "$VERSION_TAG" \
    "$TMP_DIR/$AMD64_ASSET" \
    --title "Boil $VERSION_TAG" \
    --generate-notes

  commit="$(git rev-parse HEAD)"
  release_url="$(gh release view "$VERSION_TAG" --json url --jq '.url')"

  echo
  echo "发布完成。"
  echo "Commit: $commit"
  echo "Tag: $VERSION_TAG"
  echo "Release: $release_url"
  echo "Assets:"
  echo "  - $AMD64_ASSET"
}

main "$@"
