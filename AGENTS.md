# AGENTS.md

This repository is a long-term maintained fork of a Boil.network IP change tool.

Agents working in this repository must treat real reconnect operations, real
quota consumption, local credentials, and system services as protected resources.
Default to read-only analysis until the requested change is understood.

## Hard Safety Rules

- Never call the real Boil reconnect API.
- Never run any command or code path that may consume real IP change quota.
- Never automatically start, enable, restart, stop, or install systemd services.
- Never modify `config.env`.
- Follow the explicit-authorization requirements in the Git Policy below.
- Never install dependencies unless the user explicitly asks for it.
- Never start the application unless the user explicitly asks for it and the
  command is confirmed not to call real reconnect or consume quota.

## Required Workflow

Before modifying code:

1. Read the relevant repository files.
2. Identify the affected modules and call paths.
3. Check whether the change can touch reconnect, quota, config, systemd, or
   external HTTP behavior.
4. For new features, present the design first and wait for user approval before
   editing code.

After modifying code:

1. Run `cargo fmt`.
2. Run `cargo clippy`.
3. Run `cargo test`.
4. Report any command that could not be run and why.

Do not make unrelated refactors. Keep changes scoped to the user's request.

## External HTTP Policy

All external HTTP behavior must be mockable.

This includes:

- Boil.network login, query, and reconnect APIs.
- Telegram Bot API calls.
- Public IP discovery services.
- IP quality services.
- Streaming unlock checks.
- GitHub release/download calls in install tooling.

New code must not hard-code HTTP behavior in a way that prevents testing without
real network access. Prefer injectable clients, small traits, mock servers, or
clearly separated transport layers.

Tests must not call real external HTTP services.

## Reconnect And Quota Policy

The real reconnect endpoint is dangerous because it can consume limited daily
quota. Any code touching reconnect must be reviewed as quota-sensitive.

Protected paths include:

- `BoilClient::change_ip`
- `reconnect::reconnect_one`
- `reconnect::reconnect_selected`
- CLI `change`
- Telegram `/change` and callback handlers
- Timer-triggered auto change
- Legacy shell scripts that are disabled to avoid quota-sensitive API calls

When changing these areas, use mocks, dry-run behavior, or static analysis only.
Do not execute them against the real Boil service.

## Configuration Policy

Do not modify `config.env`.

Allowed configuration-related files include examples, documentation, and test
fixtures, provided they do not contain real secrets.

Credentials and tokens must never be printed in logs, test output, error
messages, commits, or documentation examples.

## Systemd Policy

Do not run commands that install, enable, disable, restart, or stop services.

Forbidden examples:

- `boil service install`
- `boil service uninstall`
- `systemctl enable --now boil`
- `systemctl restart boil`
- `systemctl stop boil`

Reviewing `service.rs` or install scripts is allowed. Editing them is allowed
only after analysis and, for new features, an approved design.

## Git Policy

Agents may inspect git status and diffs.

- Without an explicit user request, do not run `git add`, `git commit`,
  `git commit --amend`, `git rebase`, `git reset`, `git push`, or otherwise
  modify git history.
- When the user explicitly requests a Git operation, execute only the specific
  operation requested.
- Authorization to commit does not authorize pushing. A push requires separate,
  explicit user authorization.
- Never force-push.
- Show `git status` before and after executing an authorized Git operation.

## Documentation And Install Scripts

This is a fork intended for long-term maintenance. Documentation and install
scripts must not accidentally direct users to the original upstream repository
when the intended target is this fork.

When editing install flows, verify repository URLs, artifact names, and service
side effects. Do not run install scripts unless explicitly requested and safe.

## Legacy Shell Scripts

`change-ip.sh` and `tg-bot.sh` contain real API paths and can trigger reconnect.
Treat them as quota-sensitive. Do not execute their change paths.

If these scripts remain supported, keep their safety behavior aligned with the
Rust implementation.
