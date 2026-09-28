# Undo / Checkpoint — Especificación

Estado: **IMPLEMENTADO (daemon-core 0.1.21) — CONTROL WEB-FIRST**. Desviación
declarada: blobs SIN comprimir (spec original decía zstd) para no añadir
dependencias — los límites duros acotan el almacén igualmente.

**Superficie de control (revisión web-first):** el cockpit es la WEB — el
daemon corre en background/tray y "se olvida"; el panel egui fue ELIMINADO.
La web lista el índice y restaura vía `GET/POST /api/devices/:id/checkpoints`
(mismo pipeline owner-scoped que las acciones del agente). El consentimiento
necesario (restore sin grant, acciones fuera de workspace) se RELAYA a la web:
prompt → DO storage → `GET /consents` → owner aprueba/deniega → `POST /consent`
→ DO retransmite `consent_response` por WS → el daemon REANUDA la acción
aparca­da. Primera respuesta gana (web o diálogo egui local, que queda como
fallback); el perdedor se descarta en el ConsentBroker. Crítico: el bucle WS
del daemon NUNCA se bloquea esperando a un humano — la acción se aparca en
`WsClient.parked` (expira a los 120s) y el `select!` la reanuda cuando llega
la decisión; bloquear el bucle lo convertía en un zombi autoinfligido (sin
heartbeats ni acks). Autorización one-shot: el path aprobado vale SOLO para
esa acción (`ScopeSnapshot.one_shot_paths`, verificación léxica que no anula
la aprobación cuando el target no existe aún); `remember=true` sí persiste
en el gate local. `skip_consent_prompt` NUNCA auto-ejecuta un restore.

Implementación: `daemon-core/src/checkpoint.rs`, gancho en
`WsClient::checkpoint_before` (invocado tras el gate y antes de mutar),
capability `desktop.fs.restore` con doble vía (grant explícito o
consentimiento one-shot web-first). Store: `<config-dir>/checkpoints`
(override `SYNTHHIRES_CHECKPOINTS=<dir>`, `off` desactiva). Escenario E2E de
restauración byte a byte en `e2e/full-pipeline.sh` (S8); batería de
consentimiento web (aparcar → aprobar → escribir / denegar → respetado /
remember → auto) verificada contra daemon real.

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
8. **UI (web-first)**: el control de checkpoints vive en la web
   (`device-checkpoints.tsx` en la ficha del dispositivo; polling 6s con
   guard de visibilidad) con botones Deshacer/Borrar según `existed`.
   `desktop.checkpoint.list` es de solo lectura (índice: paths + tamaños,
   nunca contenidos salvo restore explícito) y mapea al tier `desktop.fs.read`
   en servidor y daemon. El consentimiento pendiente se muestra en
   `device-consents.tsx` (mismo sheet, arriba del todo).

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
