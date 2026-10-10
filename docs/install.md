# Installing the Bridge

The bridge connects one of **your** machines to your SynthHires account so the
agent can act on it (shell, files, browser). Pairing **requires logging in on
the web first** — anonymous devices are rejected (`login_required`).

## Pick your path

| You are on… | Do this |
|---|---|
| Windows | Download the `.exe` from the [`edge` release](https://github.com/planobinario/synthhires-bridge/releases/tag/edge) and run it. |
| macOS | Same release → `synthhires-bridge-macos` (universal), or `.dmg` when published. |
| Debian / Ubuntu | Same release → `.deb`, `sudo dpkg -i …`, or `install.sh` below. |
| Fedora | Same release → `.rpm`. |
| NixOS | **Do not use the raw binary** (store paths are baked in). `nix run github:planobinario/synthhires-bridge -- --help`, or enable `services.synth-hires-bridge` from `nixosModules.default` in `flake.nix`. |
| Generic Linux / headless VM / WSL | `install.sh` (one line, no browser on the machine). |
| Container / persistent sidecar | `Dockerfile` in this repo (advanced — needs dbus, see below). |
| Android | APK from the `edge` release (sideload). |

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
synthhires-bridge run        # foreground; --systemd already did this for you
synthhires-bridge status     # health
```

Backend precedence (first wins): `--backend-url` flag ›
`SYNTHHIRES_BACKEND_URL` env › compiled default. The `pair` response also
stores the backend, so `run` keeps talking to the same web without flags.

Pin reproducible hosts with `--tag vX.Y.Z` instead of the rolling `edge`.

## Docker (advanced)

```bash
docker build -t synthhires-bridge:edge .
docker run -d --name bridge --restart unless-stopped \
  -v bridge-data:/data \
  -v "${XDG_RUNTIME_DIR}/bus:${XDG_RUNTIME_DIR}/bus" \
  -e DBUS_SESSION_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR}/bus" \
  -e SYNTHHIRES_BACKEND_URL=https://synthhires.com \
  synthhires-bridge:edge run
docker exec bridge synthhires-bridge pair https://synthhires.com <CODE>
```

Why the dbus mount: the device token lives in the **OS keyring** (Secret
Service on Linux). Without it the daemon cannot persist pairing — that is a
security property, not a bug. If your host has no Secret Service, prefer
`install.sh` directly on the VM over Docker.

## Uninstall / revoke

- Local: `bash install.sh --uninstall`, or `synthhires-bridge unpair`.
- Server-side (kills the token everywhere, closes the WS in ms): Connections
  → *Revoke* on the web.
