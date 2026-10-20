# ⛔ REFERENCE PROTOTYPE — NOT A SHIPPED DAEMON

`index.ts` here is a **protocol reference**, not a product component.

It is **not** built, packaged, published or installed by anything in this
repository: no Cargo manifest, no CI workflow, no Dockerfile and no
`install.sh` path references it. It exists so the wire format can be read
in one file.

## Why it must never run against a real backend

This prototype has **no security boundary at all**:

| Capability | What this file does | What the Rust daemon does |
|---|---|---|
| `desktop.shell.execute` | `exec(command)` with an arbitrary `cwd` | `CapabilityGate` + `path_gate` + consent |
| `desktop.fs.read` | `fs.readFile(params.path)` | canonicalised path, symlink-escape check, `alwaysAllowPaths` prefix match |
| `desktop.fs.write` | `fs.writeFile(params.path)` | same, plus atomic write and the write scope |
| consent | **none** — every action runs immediately | `ConsentBroker` with a web-first relay and a local egui fallback |
| audit | plain-text log | AES-256-GCM encrypted, append-only, redacted |

Pointing it at a real account would hand that account a device with full,
unprompted read/write/exec access to the machine — including `~/.ssh`,
`~/.aws` and the wallet files. The Rust daemon
(`crates/daemon-core`) is the one that implements the real boundary.

## The guard

Since the file stays in the tree for reference, it refuses to start
unless the operator opts in explicitly:

```bash
SYNTHHIRES_ALLOW_UNSAFE_REFERENCE_DAEMON=1 node index.ts
```

Without that variable it exits with a message instead of connecting.
