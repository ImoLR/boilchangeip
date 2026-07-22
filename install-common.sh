#!/usr/bin/env bash
# install.sh 与 update.sh 共用的构建和安装函数。

set -Eeuo pipefail

BIN_NAME="${BOIL_BIN_NAME:-boil}"
INSTALL_DIR="${BOIL_INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="${BOIL_CONFIG_DIR:-/etc/boil}"

common_die() {
  echo "错误: $*" >&2
  exit 1
}

common_require_command() {
  command -v "$1" >/dev/null 2>&1 || common_die "缺少命令: $1"
}

check_build_environment() {
  [[ "$(uname -s)" == "Linux" ]] || common_die "仅支持 Linux 系统"
  common_require_command cargo
}

ensure_install_dir() {
  if [[ -d "$INSTALL_DIR" ]]; then
    return
  fi

  if [[ -w "$(dirname "$INSTALL_DIR")" ]]; then
    mkdir -p "$INSTALL_DIR"
  elif command -v sudo >/dev/null 2>&1; then
    sudo mkdir -p "$INSTALL_DIR"
  else
    common_die "无法创建 $INSTALL_DIR，请使用有权限的账号或设置 BOIL_INSTALL_DIR"
  fi
}

# 安装任意已准备好的二进制产物；未来 Release 下载可直接复用此函数。
install_artifact() {
  local artifact="$1"
  local destination="$INSTALL_DIR/$BIN_NAME"

  [[ -f "$artifact" ]] || common_die "未找到构建产物: $artifact"
  ensure_install_dir

  if [[ -w "$INSTALL_DIR" ]]; then
    install -m 0755 "$artifact" "$destination"
  elif command -v sudo >/dev/null 2>&1; then
    sudo install -m 0755 "$artifact" "$destination"
  else
    common_die "无法写入 $INSTALL_DIR，请使用有权限的账号或设置 BOIL_INSTALL_DIR"
  fi
}

build_release() {
  local source_dir="$1"

  [[ -f "$source_dir/Cargo.toml" ]] || common_die "无效的源码目录: $source_dir"
  echo "编译 Release 版本..."
  cargo build --release --manifest-path "$source_dir/Cargo.toml"
}

install_from_source() {
  local source_dir="$1"

  check_build_environment
  build_release "$source_dir"
  install_artifact "$source_dir/target/release/$BIN_NAME"
}

print_install_summary() {
  local source_dir="$1"
  local binary="$INSTALL_DIR/$BIN_NAME"

  echo "程序路径: $binary"
  echo "源码版本: $(git -C "$source_dir" rev-parse --short HEAD)"
  echo "程序版本: $("$binary" --version)"

  if [[ -f "$CONFIG_DIR/config.env" ]]; then
    echo "已保留现有配置: $CONFIG_DIR/config.env"
  else
    echo "尚未检测到 $CONFIG_DIR/config.env，可运行: $BIN_NAME setup"
  fi
  echo "未自动安装或启动 systemd 服务。"
}
