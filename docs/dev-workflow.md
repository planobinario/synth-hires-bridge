# Dev Workflow — SynthHires Bridge

## ⚠️ Mandatory: `npm run build` after editing worker.ts

**Symptom**: Edits to `src/worker.ts` (consent-relay handler, API routes, DO
classes) have **no effect** in development until you rebuild the Astro output.

**Root cause**: `wrangler.json` mounts `dist/_worker.js` as the Worker entry
point. Astro's dev server (`astro dev`, port 4323) does **not** hot-reload
into `dist/_worker.js`. The `main` field in `wrangler.toml` only affects
Durable Object class bindings — API HTTP routes are delegated to the
stale `dist/_worker.js`.

**Fix**:

```bash
# After ANY change to src/worker.ts (or the Astro integration that
# generates _worker.js):
cd apps/synth-hires        # web repo root
npm run build              # regenerates dist/_worker.js
```

**Verification**:
```bash
# The worker log should show timestamps matching your build, not the
# 00:48 stale artifact:
ls -la dist/_worker.js
```

## inotify hot-reload limitations

The systemd unit's `inotify` watch on `src/worker.ts` does **not**
reliably trigger an Astro rebuild during `astro dev` + `wrangler dev`.
In tests, **0 reloads were detected** after editing `worker.ts` — the
only signal that changes took effect was an explicit `npm run build`.

**Recommendation**: Disable the systemd inotify trigger for `worker.ts`
or route it to `npm run build && wrangler dev` explicitly.

## Daemon-side dev

The Rust daemon (`apps/desktop-daemon/crates/`) hot-rebuilds via `cargo
build` or `cargo watch -x build`. No Astro rebuild is needed for daemon
crates; `cargo test` is the verification gate.

## Running tests

```bash
# Rust unit tests (daemon-core + daemon-cli)
cd apps/desktop-daemon
cargo test --workspace --lib

# Consent broker unit tests only
cargo test --package daemon-core --lib consent::tests

# Server host-guard unit tests only
cargo test --package daemon-cli --lib server::tests

# Full E2E (real infra)
bash apps/desktop-daemon/e2e/full-pipeline.sh
```
