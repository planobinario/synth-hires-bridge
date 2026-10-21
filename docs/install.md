# Installing the Bridge

The bridge connects one of **your** machines to your SynthHires account so the
agent can act on it (shell, files, browser). Pairing **requires logging in on
the web first** — anonymous devices are rejected (`login_required`, guests get
`403` and can never own devices).

Binary naming: Cargo builds `synth-hires-bridge` (with hyphen); releases,
`install.sh` and Docker rename it to `synthhires-bridge` (no hyphen).
`nix run` uses the Cargo name.

## Pick your path

| You are on… | Do this |
|---|---|
| Windows | Download `synthhires-bridge-windows-x64-user.exe` (or the `synthhires-bridge-windows.exe` alias) from the [`edge` release](https://github.com/planobinario/synthhires-bridge/releases/tag/edge) and run it. `-system.exe` is the machine-wide variant. |
| macOS | Same release → `synthhires-bridge-macos` (universal x64+arm64). No `.dmg` yet. |
| Debian / Ubuntu | Same release → `.deb` (`synthhires-bridge-linux-x64/arm64.deb`), `sudo dpkg -i …`, or `install.sh` below. |
| Fedora | Same release → `.rpm` (`synthhires-bridge-linux-x64/arm64.rpm`). |
| NixOS | **Do not use the raw binary** (store paths are baked in). `nix run github:planobinario/synth-hires-bridge -- --help`, or enable `services.synth-hires-bridge` from `nixosModules.default` in `flake.nix` (`enable`, plus `headless=true` default, `backendUrl`, `configDir`, `localPort`). |
| Generic Linux / headless VM / WSL | `install.sh` (one line, no browser on the machine). |
| Container / persistent sidecar | `Dockerfile` in this repo (advanced — needs dbus, see below). |

> Android APK / iOS / `.dmg` are **not** published by CI today (`build.yml`
> has no such jobs). Ignore any older doc row promising them.

## Headless install (terminal, VM)

```bash
curl -fsSL https://raw.githubusercontent.com/planobinario/synthhires-bridge/main/install.sh | bash
# with extras:
curl -fsSL .../install.sh | bash -s -- --backend https://synthhires.com --systemd
```

Then, **logged in on the web**, open Connections → *Conectar equipo*, copy the
code, and pair this machine (the web tells you exactly which backend the code
belongs to — prod, staging, or local dev):

```bash
synthhires-bridge pair "https://synthhires.com" <CODE>
synthhires-bridge --headless run   # foreground on a VM; --systemd already did this
synthhires-bridge status           # health
```

Backend precedence (first wins): `--backend-url` flag ›
`SYNTHHIRES_BACKEND_URL` env › compiled default. The `pair` response also
stores the backend, so `run` keeps talking to the same web without flags.
`pair` validates the backend BEFORE touching the network (https/wss anywhere,
http/ws loopback-only) and probes the keyring BEFORE burning the single-use
code — a typo'd backend or a missing Secret Service fails fast, no ghost
device.

Pin reproducible hosts with `--tag vX.Y.Z` instead of the rolling `edge`.

## CLI reference

Global flags: `--backend-url` (`SYNTHHIRES_BACKEND_URL`), `--local-port`
(`SYNTHHIRES_LOCAL_PORT`, default `7333`), `--config-dir`, `--headless`.

| Command | What |
|---|---|
| `run` | Foreground daemon (same as no subcommand). |
| `status [--json]` | Pairing + WS + keyring + version. Exit `2` = paired but WS offline, `3` = daemon HTTP unreachable. |
| `doctor [--json]` | state.json, daemon HTTP, keyring token, WS. Exit `1` on any FAIL. |
| `verify <path> [--json]` | Empirical read+write probe of a path on THIS machine. Exit `1` if not usable. |
| `logs [--lines N]` | Tail the on-disk daemon log (default 100). |
| `pair <backend> <code> [--pairing-id ID]` | Exchange a web code for a device token (stored in OS keyring, never disk). `--pairing-id` is the desktop-mode id. |
| `unpair` | Clear keyring entry + state.json locally. |
| `stop` | Ask the running daemon (on `--local-port`) to shut down via its nonce-guarded local endpoint. |

## Pairing paths (local vs remote)

- **CLI `pair`** (above): works everywhere, including headless/remote. This is
  the ONLY path for a machine that is not the one running your browser.
- **Deep link `synthhires://pair?code=…&pairing_id=…&backend=…`**: same-machine
  only. The OS hands it to the running instance via the local IPC socket
  (`$XDG_RUNTIME_DIR/synthhires-bridge-ipc.sock`, fallback `/tmp/…`), which
  completes `pair/complete` itself. Legacy `pair?token=…` links still parse.
- **Web → `http://127.0.0.1:7333/pair`**: same-machine only, used by
  `/space/connections` auto-pair. Never expose this port; the daemon binds
  loopback, pins `Host: 127.0.0.1|localhost:<port>`, enforces the
  `Origin` allowlist, and requires a fresh single-use local nonce (120 s TTL)
  for `/pair`, `/unpair` AND `/shutdown`.

## Docker (advanced)

Prebuilt multi-arch image (amd64/arm64) published by CI on every `main` push
(after `publish`, so it bakes the fresh `edge` asset) and `v*` tag — no local
build needed:

```bash
docker pull ghcr.io/planobinario/synthhires-bridge:edge
docker run -d --name bridge --restart unless-stopped \
  -v bridge-data:/data \
  -v "${XDG_RUNTIME_DIR}/bus:${XDG_RUNTIME_DIR}/bus" \
  -e DBUS_SESSION_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR}/bus" \
  -e SYNTHHIRES_BACKEND_URL=https://synthhires.com \
  ghcr.io/planobinario/synthhires-bridge:edge --headless run
docker exec bridge synthhires-bridge pair https://synthhires.com <CODE>
```

(From source instead: `docker build -t synthhires-bridge:edge .` — the
Dockerfile downloads the matching release asset at build time.)

Why the dbus mount: the device token lives in the **OS keyring** (Secret
Service on Linux). Without it the daemon cannot persist pairing — that is a
security property, not a bug. If your host has no Secret Service, prefer
`install.sh` directly on the VM over Docker (ephemeral runner shape — pair →
run → unpair per job — also works without persistence).

## Uninstall / revoke

- Local: `bash install.sh --uninstall`, or `synthhires-bridge unpair`.
- Server-side (kills the token everywhere, closes the WS in ms): Connections
  → *Revoke* on the web.

## Updates

- `v*` releases ship a signed `release-manifest-<ver>.json` (Ed25519, key in
  CI secrets; pubkey baked into the binaries). The daemon verifies the
  signature before trusting hashes — a compromised mirror cannot push
  binaries. Rolling `edge` stays unsigned: pin `--tag vX.Y.Z` (or the
  `:vX.Y.Z` image) for verifiable hosts. The manifest carries one entry per
  target triple (the universal macOS binary covers both arch triples).
- NixOS follows nix, not auto-update: `nix flake update` in your config.

## Troubleshooting

| Symptom | Cause → fix |
|---|---|
| `pair` fails `keyring unavailable` | No Secret Service / dbus session (headless, Docker, WSL). Start one (gnome-keyring + `DBUS_SESSION_BUS_ADDRESS`) or use `install.sh` on the host. `doctor` shows `keyring token present [FAIL]`. |
| `pair` fails `invalid backend` | Backend must be https/wss (http/ws loopback-only). Copy the exact backend the web shows. |
| `status` exit 3 / `doctor` HTTP `[FAIL]` | Daemon not running, or listening on `--local-port` ≠ queried port. Pass the same `--local-port` (or `SYNTHHIRES_LOCAL_PORT`) to `status`/`doctor`/`stop`. |
| `status` exit 2 (paired, WS offline) | Token missing (keyring wiped?) or backend unreachable. `logs --lines 200` shows the WS error; re-`pair` if the token is gone. |
| Web auto-pair 403 `Invalid nonce` | Two clients raced the single-use local nonce — retry once (the web does automatically). Persistent 403s: another local process is pairing in a loop; `stop` it. |
| Web says daemon offline after pair | WS didn't connect in 15 s: backend mismatch (pair stored another origin?) or outbound wss blocked. `logs` first. |
| GUI won't start on a VM | Run `--headless` (or the systemd unit / NixOS module, which default to it). The tray needs GTK on the main thread. |
| Two daemons / port conflict | The single-instance lock refuses a second daemon (headless exits `1`). `stop` the old one first. |
