# E2E matrix — full bridge pipeline

Real end-to-end verification of the bridge stack: **worker (real Astro build
under wrangler/miniflare) → Durable Object → WebSocket → daemon → this
machine** (Chrome, filesystem, shell, AST tools), plus optional real-model
agent scenarios.

This is the harness that keeps the stack honest: every scenario asserts on
REAL outputs (page text, file contents, exit codes, match counts) — never on
dispatch acknowledgements.

## Prerequisites
- PostgreSQL binaries (`pg_ctl`, `initdb`, `psql`) on PATH or `$PG_BIN`.
- Node/pnpm with the web repo's deps installed (server build runs `npm run build`).
- A daemon binary: `$DAEMON_BIN` (defaults to the debug build), run under
  nix-ld wrapper when on NixOS (`$NIX_LD_LIBRARY_PATH`).
- For agent scenarios: `DEEPSEEK_API_KEY` (BYOK; skipped when absent).

## Usage

```bash
# Full matrix (infra + bridge + code scenarios; agent skipped without key)
bash apps/desktop-daemon/e2e/full-pipeline.sh

# Only infra + deterministic scenarios, keep infra alive after (debug)
KEEP=1 bash apps/desktop-daemon/e2e/full-pipeline.sh

# Include real-model agent scenarios (browser + code via /api/chat)
DEEPSEEK_API_KEY=sk-... bash apps/desktop-daemon/e2e/full-pipeline.sh --agent
```

## Scenarios
| # | Scenario | Asserts |
|---|----------|---------|
| 1 | pairing & WS | daemon online via DO status, `/status` ws.connected |
| 2 | fs roundtrip | list/write/read/verify through the full pipeline |
| 3 | browser + route mocks | mock serves locally on nav; passthrough favicon 404 |
| 4 | code AST (dir grep) | matches labeled with `file`; 0-match = empty, not error |
| 5 | code AST (edit) | replacements verified; adjacent structure intact |
| 6 | shell | exit codes/stdout honest (incl. failing command) |
| 7 | scope enforcement | ungranted capability rejected; consent required outside allow-paths |
| 8 | zombie watchdog (optional `--watchdog`) | SIGSTOP runtime → auto-reconnect → action OK |
| 9 | agent loop (optional, real model) | tool calls with correct deviceId; results on disk |

## Notes
- wrangler/miniflare on NixOS needs `SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt`
  for outbound TLS (the script sets it when the file exists).
- The script is self-contained: boots its own PG on a scratch port, fixture
  server, and worker; tears everything down unless `KEEP=1`.
