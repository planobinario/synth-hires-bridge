# Undo / Checkpoint — Especificación

Estado: **IMPLEMENTADO (daemon-core 0.1.21)**. Desviación declarada: blobs SIN
comprimir (spec original decía zstd) para no añadir dependencias — los límites
duros acotan el almacén igualmente; comprimir es un cambio puramente local y
retrocompatible con el índice. Implementación: `daemon-core/src/checkpoint.rs`,
gancho en `WsClient::checkpoint_before` (invocado tras el gate y antes de
mutar), capability `desktop.fs.restore` con doble vía (grant explícito o
diálogo de consentimiento one-shot; `skip_consent_prompt` NUNCA lo autoriza),
panel "Cambios del agente (deshacer)" en la pestaña Actividad del daemon UI.
Store: `<config-dir>/checkpoints` (override `SYNTHHIRES_CHECKPOINTS=<dir>`,
`off` desactiva). Escenario E2E de restauración byte a byte en
`e2e/full-pipeline.sh` (S9).

Motivación: hoy `fs_write`, `fs_patch` y `fs_delete` sobrescriben/borran sin
pre-imagen recuperable. Un agente que se equivoca destruye trabajo del usuario
sin red de seguridad. El undo convierte "confía en el agente" en "el agente
puede equivocarse sin pérdida".

## Decisiones de diseño

1. **Checkpoint ANTES de mutar, nunca después.** La pre-imagen se captura en
   `path_gate()` (daemon-core/ws_client.rs) para toda acción `desktop.fs.write`,
   `desktop.fs.patch`, `desktop.fs.delete` autorizada. Capturar después de la
   escritura es inútil: la imagen original ya se perdió.
2. **Almacén local, fuera del workspace escaneado**: `$CONFIG_DIR/checkpoints/`
   (mismo nivel que `state.json`), contenido comprimido (zstd), index en
   `checkpoints/index.jsonl` (append-only, una línea por checkpoint).
3. **Límites duros**: `MAX_CHECKPOINTS = 200` (FIFO), `MAX_TOTAL_BYTES = 512 MiB`
   (evict por antigüedad), `MAX_FILE_BYTES = 32 MiB` (archivos mayores → checkpoint
   de metadatos + contenido NO guardado, marcado `truncated`).
4. **Dominio de archivos, no de acciones**: checkpoint por (device, path, epoch_millis)
   con hash SHA-256 de la pre-imagen. Restaurar un checkpoint N no requiere
   deshacer N-1: cada path tiene su pila propia. Los actions llevan `action_id`
   → el índice registra `{action_id, path, pre_hash, blob_path, ts}` para
   correlación UI ("deshacer exactamente la acción X").
5. **fs_delete guarda la imagen completa** (es la más destructiva). fs_write con
   `isEdit`/target guarda la pre-imagen del archivo entero (no del fragmento).
6. **Restauración con consentimiento**: `desktop.fs.restore` es una capability
   nueva, NO incluida en ningún preset de scopes; requiere grant explícito o
   consentimiento interactivo. Restaurar sin preguntar sería sustituir un riesgo
   por otro.
7. **Sobrescritura segura del restore**: antes de restaurar, la versión ACTUAL
   se checkpointea también (el undo es reversible en ambos sentidos).
8. **UI**: panel "Actividad" (daemon-cli/ui.rs) lista los últimos checkpoints
   con origen (action_id, capability, path) y botón restaurar; el web muestra el
   mismo índice vía nuevo endpoint `GET /api/devices/:id/checkpoints` (solo
   índice, contenido nunca viaja por red salvo restore explícito).

## No-goals explícitos

- Sin deduplicación entre checkpoints (coste: espacio; beneficio: simpleza y
  corrección evidente).
- Sin sincronización cloud de checkpoints (privacidad: la red de seguridad es
  local por diseño).
- Sin undo de `shell` (ejecutar `rm -rf` es irreversible por naturaleza; la
  mitigación correcta ahí es el interceptor de comandos destructivos, que ya
  existe en el system prompt del agente y en la política de consentimiento).

## Plan de implementación (orden)

1. `daemon-core/src/checkpoint.rs`: almacén + límites + tests de propiedades
   (roundtrip, evict FIFO, truncado, restauración reversible).
2. Gancho en `path_gate()` + `handle_action` de ws_client.rs (solo write/patch/delete).
3. `desktop.fs.restore` con gate de consentimiento reutilizando ConsentBroker.
4. Índice en UI (ui.rs) + endpoint web de solo-lectura.
5. Matriz E2E: nuevo escenario en full-pipeline.sh — write → checkpoint existe →
   restore → contenido original verificado byte a byte.

## Métrica de éxito

- Toda mutación fs del agente es reversible dentro de los límites declarados.
- El usuario puede ver y deshacer cada cambio del agente desde el daemon UI.
- Cero aumento del tiempo de acción perceptible (< 5 ms por checkpoint p95).
