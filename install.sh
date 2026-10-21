#!/usr/bin/env bash
#
# install.sh — Installs the SynthHires Bridge daemon from the rolling `edge`
# release (or a pinned tag) without a browser.
#
#   curl -fsSL https://raw.githubusercontent.com/planobinario/synthhires-bridge/main/install.sh | bash
#   curl -fsSL .../install.sh | bash -s -- --backend https://app.example.com --systemd
#
# Flags:
#   --tag TAG        Release tag (default: edge). Pin e.g. --tag v0.1.20.
#   --backend URL    Backend origin shown in `pair` hints (default: https://synthhires.com).
#   --dir DIR        Install directory (default: ~/.local/bin).
#   --systemd        Also install+start a systemd user service (Linux only).
#   --no-doctor      Skip the post-install `doctor` check.
#   --uninstall      Remove the binary (+ user service if present) and exit.
#   -h, --help       This help.
#
# Security note: `edge` is a rolling release with no signed manifest yet
# (signed updates are tracked in the web repo's docs/bridge-design.md §8).
# Downloads run over HTTPS from github.com; pin --tag for reproducible hosts.
# Binary naming: Cargo builds `synth-hires-bridge` (with hyphen); releases,
# install.sh and Docker rename it to `synthhires-bridge` (no hyphen).
set -euo pipefail

REPO="planobinario/synthhires-bridge"
TAG="edge"
BACKEND="https://synthhires.com"
DIR="${HOME}/.local/bin"
SYSTEMD=0
DOCTOR=1
UNINSTALL=0

usage() { sed -n '2,/^set /p' "$0" | sed 's/^#\{0,1\} //'; }

while [ $# -gt 0 ]; do
  case "$1" in
    --tag) TAG="${2:?}"; shift 2 ;;
    --backend) BACKEND="${2:?}"; shift 2 ;;
    --dir) DIR="${2:?}"; shift 2 ;;
    --systemd) SYSTEMD=1; shift ;;
    --no-doctor) DOCTOR=0; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown flag: $1 (see --help)" >&2; exit 1 ;;
  esac
done

BIN="synthhires-bridge"
TARGET="${DIR}/${BIN}"

if [ "$UNINSTALL" -eq 1 ]; then
  systemctl --user disable --now synthhires-bridge.service 2>/dev/null || true
  rm -f "$TARGET" "${HOME}/.config/systemd/user/synthhires-bridge.service"
  systemctl --user daemon-reload 2>/dev/null || true
  echo "Uninstalled ${TARGET}."
  exit 0
fi

OS="$(uname -s)"
ARCH="$(uname -m)"
case "${OS}-${ARCH}" in
  Linux-x86_64)       ASSET="synthhires-bridge-linux-x64" ;;
  Linux-aarch64|Linux-arm64) ASSET="synthhires-bridge-linux-arm64" ;;
  Darwin-x86_64|Darwin-arm64) ASSET="synthhires-bridge-macos" ;;
  *) echo "Unsupported platform: ${OS}-${ARCH}." >&2
     echo "Windows: download the .exe from https://github.com/${REPO}/releases/tag/${TAG}." >&2
     exit 1 ;;
esac

if [ -f /etc/os-release ] && grep -qi '^ID=nixos' /etc/os-release; then
  cat >&2 <<'EOF'
NixOS detected: a raw edge binary will NOT run (store paths are baked in).
Use the first-class Nix paths instead:
  nix run github:planobinario/synthhires-bridge -- --help
  # or enable the module in your system config:
  #   services.synth-hires-bridge.enable = true;  (see flake.nix → nixosModules.default)
EOF
  exit 1
fi

need() { command -v "$1" >/dev/null 2>&1 || { echo "Missing required tool: $1" >&2; exit 1; }; }
need curl
mkdir -p "$DIR"

URL="https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"
echo "Downloading ${ASSET} (${TAG})..."
curl -fsSL --retry 3 -o "$TARGET" "$URL"
chmod +x "$TARGET"

case ":${PATH}:" in
  *":${DIR}:"*) ;;
  *) echo "NOTE: ${DIR} is not on your PATH. Add: export PATH=\"${DIR}:\$PATH\"" ;;
esac

if [ "$SYSTEMD" -eq 1 ]; then
  if [ "$OS" != "Linux" ]; then echo "--systemd is Linux-only; skipped." >&2
  elif ! command -v systemctl >/dev/null 2>&1; then echo "No systemctl; skipped user service." >&2
  else
    UNIT="${HOME}/.config/systemd/user/synthhires-bridge.service"
    mkdir -p "$(dirname "$UNIT")"
    cat > "$UNIT" <<EOF
[Unit]
Description=SynthHires Bridge Daemon
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=${TARGET} --headless --backend-url "${BACKEND}" run
Restart=always
RestartSec=5s

[Install]
WantedBy=default.target
EOF
    systemctl --user daemon-reload
    systemctl --user enable --now synthhires-bridge.service
    echo "Systemd user service installed and started."
  fi
fi

if [ "$DOCTOR" -eq 1 ]; then
  echo "--- doctor ---"
  "$TARGET" doctor || echo "(doctor reported issues — see above; pairing still possible)"
fi

cat <<EOF

Installed: ${TARGET}
Next (after logging in on the web):
  1. Open Connections → "Conectar equipo" and copy the code (it shows the backend: ${BACKEND}).
  2. Pair this machine:
       ${BIN} pair "${BACKEND}" <CODE>
     (non-interactive alternative: SYNTHHIRES_BACKEND_URL="${BACKEND}" ${BIN} pair ... )
  3. Run the daemon:  ${BIN} run   (foreground)  — or it is already running via --systemd.
  4. Health:  ${BIN} status   ·   ${BIN} logs
EOF
