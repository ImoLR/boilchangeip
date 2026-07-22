#!/usr/bin/env bash
#
# Legacy shell entrypoint.
#
# The Rust binary now uses the Boil Token API and multi-VPS server selection.
# This script intentionally does not call old /login, /api/query_all, or
# /api/reconnect endpoints because changeIP failures may still consume quota.

set -euo pipefail

cat >&2 <<'MSG'
change-ip.sh is deprecated and disabled.

Use the Rust CLI instead:
  boil servers list
  boil status --server <server-id>
  boil change --server <server-id>
  boil change --all

Configure BOIL_SERVERS in config.env. One Boil Token corresponds to one VPS.
MSG

exit 2
