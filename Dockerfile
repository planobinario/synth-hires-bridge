# Dockerfile — headless SynthHires Bridge for persistent VMs / containers.
#
# The daemon keeps secrets in the OS keyring (Linux: Secret Service), so a
# bare container WITHOUT dbus cannot persist the device token across
# restarts. Two supported shapes:
#
#   A. Full node (recommended for a persistent VM sidecar):
#        docker run -d --name bridge --restart unless-stopped \
#          -v bridge-data:/data \
#          -v "${XDG_RUNTIME_DIR}/bus:${XDG_RUNTIME_DIR}/bus" \
#          -e DBUS_SESSION_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR}/bus" \
#          -e SYNTHHIRES_BACKEND_URL=https://synthhires.com \
#          ghcr.io/planobinario/synthhires-bridge:edge
#      then pair once: docker exec bridge synthhires-bridge pair <backend> <code>
#
#   B. Ephemeral runner: same image, no volumes; pair → run → unpair per job.
#
# Build: docker build -t synthhires-bridge:edge --build-arg TAG=edge .
# Multi-arch: TARGETARCH (amd64/arm64) maps to the matching edge asset.
ARG TAG=edge
ARG TARGETARCH

FROM ubuntu:24.04 AS fetch
ARG TAG
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
  && rm -rf /var/lib/apt/lists/*
RUN case "${TARGETARCH:-amd64}" in \
      amd64) ASSET=synthhires-bridge-linux-x64 ;; \
      arm64) ASSET=synthhires-bridge-linux-arm64 ;; \
      *) echo "unsupported arch: ${TARGETARCH}" >&2; exit 1 ;; \
    esac && \
    curl -fsSL --retry 3 \
      -o /tmp/synthhires-bridge \
      "https://github.com/planobinario/synthhires-bridge/releases/download/${TAG}/${ASSET}" && \
    chmod +x /tmp/synthhires-bridge

FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates dbus libssl3 \
  && rm -rf /var/lib/apt/lists/* \
  && useradd -m -u 1000 bridge
COPY --from=fetch /tmp/synthhires-bridge /usr/local/bin/synthhires-bridge
# HOME=/data: the daemon stores state.json + log under the user config dir,
# so a single volume persists pairing across restarts on any feature set.
ENV HOME=/data \
    SYNTHHIRES_BACKEND_URL=https://synthhires.com
VOLUME /data
USER bridge
WORKDIR /data
ENTRYPOINT ["synthhires-bridge"]
CMD ["doctor"]
