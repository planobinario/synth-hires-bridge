## ⚠️ Compilación: usar CI, no local

Este proyecto se compila y valida exclusivamente vía GitHub Actions
(`.github/workflows/ci.yml` y `build.yml`). No asumir que el entorno local
tiene un toolchain de Rust funcional: ha mostrado corrupción de caché/ICE de
rustc recurrente en este equipo (ver historial). El CI en runners limpios es
la fuente de verdad para "esto compila".

## Verificación local (antes de push)

Solo las crates del núcleo (rápido, sin egui):

- `cargo check -p daemon-protocol -p daemon-core --all-features`
- `cargo clippy -p daemon-protocol -p daemon-core --all-features -- -D warnings`
- `cargo test -p daemon-protocol -p daemon-core`

El check del workspace completo puede crashear rustc en egui por stack
overflow — es un problema del toolchain local; el CI compila bien.

## Publicación de un binario nuevo

1. Trabaja en este repo (`synth-hires-bridge`), nunca en `synth-hires`.
2. Bump de versión en `apps/desktop-daemon/Cargo.toml` (y deja que Cargo
   actualice `Cargo.lock`).
3. Commit + push a `main`:
   - `ci.yml` compila (windows/macos/linux) y publica/actualiza el release
     **`edge`** (prerelease continuo). Es lo que la web sirve vía
     `/api/downloads/desktop` → `releases/download/edge/<asset>`. El binario
     nuevo queda disponible automáticamente, sin tocar nada en la web.
4. Opcional — release versionada (para descargas fijadas/manuales):
   `git tag vX.Y.Z && git push origin vX.Y.Z`
   - `build.yml` compila los 6 binarios (win x64, linux x64/arm64, macos
     x64/arm64) y crea/actualiza el release `vX.Y.Z` con los assets.

### Nombres de asset

La web espera estos nombres en el release `edge`:

- `synthhires-bridge-windows.exe`
- `synthhires-bridge-linux`
- `synthhires-bridge-macos`
- `synthhires-bridge-nixos`

Los releases versionados (`v*`) añaden las variantes de arquitectura:
`-x64` / `-arm64` (linux y macos) y `synthhires-bridge-nixos`. No renombrar los assets del release
`edge`: la URL de descarga de la web los referencia directamente.

### Soporte NixOS / Nix Flakes
El repo incluye `flake.nix` en la raíz. Puede ejecutarse directamente vía:
`nix run github:planobinario/synth-hires-bridge`
o añadirse como módulo systemd en NixOS (`services.synthhires-bridge.enable = true;`).
El CI compila el Flake y publica el binario empaquetado `synthhires-bridge-nixos`.

## Seguimiento del build

- `gh run watch` para seguir el build en vivo.
- Descargar binarios de un run: `gh run download <run-id>`.
- Ver el release `edge`: `gh release view edge`.

## Autenticación de GitHub CLI

No usar `gh auth login` interactivo (requiere navegador, no es delegable).
Usar `gh auth login --with-token` con un `GH_TOKEN` provisto como variable
de entorno. Si `gh auth status` falla, pedir al usuario el token UNA vez
(nunca el login completo).

## Contrato de protocolo

El wire format vive en dos lados en lockstep:

- Web: `src/lib/agent/bridge-protocol.ts` (repo synth-hires)
- Daemon: `apps/desktop-daemon/crates/daemon-protocol/src/lib.rs` (este repo)

Si cambias shapes, actualiza AMBOS (serde camelCase ↔ campos camelCase en
TypeScript) en el mismo cambio.
