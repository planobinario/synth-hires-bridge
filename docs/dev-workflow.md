# Dev Workflow — synth-hires-bridge

> Source of truth for build/verify/release rules:
> [`apps/desktop-daemon/AGENTS.md`](apps/desktop-daemon/AGENTS.md).
> This file is a shim so deep links and old habits land somewhere true.

## The one rule

**Compile and validate via GitHub Actions (`ci.yml`, `build.yml`) — not
locally.** This machine's local Rust toolchain has a history of cache
corruption / rustc ICE (notably a stack overflow compiling the full
workspace with egui). Runners are clean; CI is the verdict.

## Allowed local checks (fast, no egui)

```bash
cd apps/desktop-daemon
cargo check -p daemon-protocol -p daemon-core --all-features
cargo clippy -p daemon-protocol -p daemon-core --all-features -- -D warnings
cargo test -p daemon-protocol -p daemon-core
```

The full-workspace check (`cargo check --workspace`) may crash rustc
locally — known toolchain issue, not a code bug. Never "fix" code to
appease a local-only ICE; check CI first.

Nix users: `nix build -L` + `nix flake check` exercise the flake,
including `cargo test -p daemon-protocol -p daemon-core` inside the
sandbox.

## Release flow (summary — details in AGENTS.md)

1. Bump `apps/desktop-daemon/Cargo.toml` (+ `Cargo.lock` via Cargo).
2. Push to `main` → CI rebuilds the rolling **`edge`** release (what the
   web serves at `/api/downloads/desktop` — no web-side change needed).
3. Optional pinned release: `git tag vX.Y.Z && git push origin vX.Y.Z`.

## Repo layout reminder

- `apps/desktop-daemon/crates/daemon-protocol` — wire format (lockstep
  with the web's `src/lib/agent/bridge-protocol.ts` in the **other** repo).
- `apps/desktop-daemon/crates/daemon-core` — pairing, WS client,
  capability gate, fs/shell/proc/browser/dap/mcp ops.
- `apps/desktop-daemon/crates/daemon-cli` — egui GUI + headless CLI +
  loopback pairing server (`127.0.0.1:7333`).
- Root `flake.nix` / `install.sh` / `Dockerfile` / `docs/install.md` —
  the install matrix (NixOS is first-class via the flake module).
- The web app lives in a SEPARATE repo (`synth-hires`). Never edit
  daemon code there; never edit web code here. Protocol changes ship
  in lockstep on both sides.
