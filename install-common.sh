#!/usr/bin/env bash
# install.sh 与 update.sh 共用的安装函数。

set -euo pipefail

BIN_NAME="${BOIL_BIN_NAME:-boil}"
SERVICE_NAME="${BOIL_SERVICE_NAME:-boil}"
INSTALL_DIR="${BOIL_INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="${BOIL_CONFIG_DIR:-/etc/boil}"
MANAGED_ROOT="${BOIL_MANAGED_ROOT:-/opt/boilchangeip}"
SOURCE_DIR="${BOIL_SOURCE_DIR:-$MANAGED_ROOT/source}"
BACKUP_ROOT="${BOIL_BACKUP_ROOT:-/var/backups/boilchangeip}"
SERVICE_PATH="/etc/systemd/system/${SERVICE_NAME}.service"

common_die() {
  echo "错误: $*" >&2
  exit 1
}

common_require_command() {
  command -v "$1" >/dev/null 2>&1 || common_die "缺少命令: $1"
}

run_privileged() {
  if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
    "$@"
  elif command -v sudo >/dev/null 2>&1; then
    sudo "$@"
  else
    common_die "操作需要管理员权限，但未找到 sudo"
  fi
}

write_privileged_file() {
  local path="$1"
  local content="$2"
  local tmp

  tmp="$(mktemp)"
  printf '%s' "$content" >"$tmp"
  if [[ -w "$(dirname "$path")" ]]; then
    install -m 0644 "$tmp" "$path"
  else
    run_privileged install -m 0644 "$tmp" "$path"
  fi
  rm -f -- "$tmp"
}

timestamp_utc() {
  date -u +"%Y%m%dT%H%M%SZ"
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
  manager="$(detect_package_manager)" || common_die "缺少依赖: ${missing[*]}，且无法识别包管理器"

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
      common_die "不支持的包管理器: $manager"
      ;;
  esac
}

ensure_directory() {
  local path="$1"
  if [[ -d "$path" ]]; then
    return
  fi
  if [[ -w "$(dirname "$path")" ]]; then
    mkdir -p "$path"
  else
    run_privileged mkdir -p "$path"
  fi
}

ensure_install_dir() {
  ensure_directory "$INSTALL_DIR"
}

ensure_config_dir() {
  ensure_directory "$CONFIG_DIR"
  if [[ -w "$CONFIG_DIR" ]]; then
    chmod 0750 "$CONFIG_DIR"
  else
    run_privileged chmod 0750 "$CONFIG_DIR"
  fi
}

build_release() {
  local source_dir="$1"

  [[ -f "$source_dir/Cargo.toml" ]] || common_die "无效的源码目录: $source_dir"
  echo "编译 Release 版本..."
  cargo build --release --manifest-path "$source_dir/Cargo.toml"
}

install_artifact() {
  local artifact="$1"
  local destination="$INSTALL_DIR/$BIN_NAME"

  [[ -f "$artifact" ]] || common_die "未找到构建产物: $artifact"
  ensure_install_dir

  if [[ -w "$INSTALL_DIR" ]]; then
    install -m 0755 "$artifact" "$destination"
  else
    run_privileged install -m 0755 "$artifact" "$destination"
  fi
}

install_from_source() {
  local source_dir="$1"

  common_require_command cargo
  build_release "$source_dir"
  install_artifact "$source_dir/target/release/$BIN_NAME"
}

systemctl_available() {
  command -v systemctl >/dev/null 2>&1 && [[ -d /run/systemd/system ]]
}

service_is_active() {
  systemctl_available && systemctl is-active --quiet "$SERVICE_NAME"
}

service_exists() {
  systemctl_available &&
    (systemctl list-unit-files "${SERVICE_NAME}.service" >/dev/null 2>&1 ||
      systemctl status "$SERVICE_NAME" >/dev/null 2>&1)
}

stop_service_if_present() {
  if systemctl_available && systemctl list-unit-files "${SERVICE_NAME}.service" >/dev/null 2>&1; then
    if systemctl is-active --quiet "$SERVICE_NAME"; then
      echo "停止服务: $SERVICE_NAME"
      run_privileged systemctl stop "$SERVICE_NAME"
    fi
  fi
}

restart_service_if_enabled() {
  if systemctl_available && systemctl is-enabled --quiet "$SERVICE_NAME" 2>/dev/null; then
    echo "重启服务: $SERVICE_NAME"
    run_privileged systemctl restart "$SERVICE_NAME"
  fi
}

remove_systemd_service() {
  if service_exists; then
    run_privileged systemctl disable --now "$SERVICE_NAME" || true
  fi

  if [[ -e "$SERVICE_PATH" || -L "$SERVICE_PATH" ]]; then
    if [[ -w "$(dirname "$SERVICE_PATH")" ]]; then
      rm -f -- "$SERVICE_PATH"
    else
      run_privileged rm -f -- "$SERVICE_PATH"
    fi
    echo "已删除服务文件: $SERVICE_PATH"
  fi

  if systemctl_available; then
    run_privileged systemctl daemon-reload
    run_privileged systemctl reset-failed "$SERVICE_NAME" || true
  fi
}

service_unit_content() {
  local binary="$INSTALL_DIR/$BIN_NAME"
  cat <<EOF
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
}

install_systemd_service() {
  common_require_command systemctl
  ensure_config_dir
  write_privileged_file "$SERVICE_PATH" "$(service_unit_content)"
  run_privileged systemctl daemon-reload
  run_privileged systemctl enable "$SERVICE_NAME"
}

start_and_verify_service() {
  common_require_command systemctl
  run_privileged systemctl restart "$SERVICE_NAME"
  sleep 2
  systemctl is-active --quiet "$SERVICE_NAME" ||
    common_die "服务启动失败，请运行 journalctl -u $SERVICE_NAME -n 80 --no-pager 查看日志"
}

configure_if_missing() {
  ensure_config_dir
  if [[ -f "$CONFIG_DIR/config.env" ]]; then
    echo "已保留现有配置: $CONFIG_DIR/config.env"
    return
  fi

  if [[ ! -r /dev/tty ]]; then
    common_die "缺少 $CONFIG_DIR/config.env，且当前环境无法交互配置"
  fi

  echo "首次安装需要配置 Boil Token 和可选 Telegram 信息。"
  "$INSTALL_DIR/$BIN_NAME" setup </dev/tty >/dev/tty
  [[ -f "$CONFIG_DIR/config.env" ]] || common_die "配置向导未生成 $CONFIG_DIR/config.env"
}

backup_config_dir() {
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

restore_config_dir() {
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

print_install_summary() {
  local source_dir="$1"
  local binary="$INSTALL_DIR/$BIN_NAME"

  echo "程序路径: $binary"
  echo "源码目录: $source_dir"
  echo "配置目录: $CONFIG_DIR"
  echo "服务名称: $SERVICE_NAME"
  echo "源码版本: $(git -C "$source_dir" rev-parse --short HEAD)"
  echo "程序版本: $("$binary" --version)"
  echo "后续升级: curl -fsSL https://raw.githubusercontent.com/ImoLR/boilchangeip/main/update.sh | bash"
  echo "查看服务: systemctl status $SERVICE_NAME"
  echo "查看日志: journalctl -fu $SERVICE_NAME"
}
