#!/usr/bin/env bash
#
# Legacy Telegram shell bot entrypoint.
#
# The Rust Telegram bot now handles multi-VPS selection and change confirmation.
# This script intentionally does not call old /login, /api/query_all, or
# /api/reconnect endpoints because changeIP failures may still consume quota.

set -euo pipefail

cat >&2 <<'MSG'
tg-bot.sh is deprecated and disabled.

Use the Rust bot instead:
  boil bot

Configure BOIL_SERVERS plus TG_TOKEN and TG_CHAT_ID in config.env.
MSG

exit 2
