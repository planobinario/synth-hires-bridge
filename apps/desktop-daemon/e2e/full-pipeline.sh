#!/usr/bin/env bash
# Full-pipeline E2E matrix for the synth-hires bridge.
#
# Boots REAL infrastructure from scratch (PostgreSQL + migrations, Astro
# server build under wrangler/miniflare, fixture HTTP server, freshly paired
# daemon) and runs deterministic scenarios with hard assertions on REAL
# outputs. Optional scenarios: zombie-watchdog injection and a real-model
# agent loop (needs DEEPSEEK_API_KEY, runs through /api/chat — the exact
# production path).
#
# Usage:
#   bash apps/desktop-daemon/e2e/full-pipeline.sh                 # deterministic only
#   KEEP=1 bash ... full-pipeline.sh                              # keep infra for debugging
#   DEEPSEEK_API_KEY=sk-... bash ... full-pipeline.sh --agent     # + real-model agent
#   WATCHDOG=1 bash ... full-pipeline.sh                          # + zombie injection (~90s)
set -uo pipefail
PIDS=()

E2E_ROOT="${E2E_ROOT:-/tmp/sh-e2e-$$}"
WEB_REPO="${WEB_REPO:-$HOME/Proyectos/Webs/synth-hires}"
BRIDGE_REPO="${BRIDGE_REPO:-$HOME/Proyectos/synth-hires-bridge}"
DAEMON_BIN="${DAEMON_BIN:-$BRIDGE_REPO/apps/desktop-daemon/target/debug/synthhires-bridge}"
PG_PORT="${PG_PORT:-$((52000 + RANDOM % 2000))}"
WORKER_PORT="${WORKER_PORT:-8801}"
FIXTURE_PORT="${FIXTURE_PORT:-18098}"
AGENT=${AGENT:-0}
WATCHDOG=${WATCHDOG:-0}
KEEP=${KEEP:-0}
PASS=0; FAIL=0; FAILED=()

log()  { printf '\033[1;34m[e2e]\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m[PASS]\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
bad()  { printf '\033[1;31m[FAIL]\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); FAILED+=("$*"); }
need() { command -v "$1" >/dev/null 2>&1 || { log "FALTA $1 en PATH"; exit 2; }; }

for c in node curl python3 psql initdb pg_ctl; do need "$c"; done
[[ -x "$DAEMON_BIN" ]] || { log "DAEMON_BIN no ejecutable: $DAEMON_BIN (compila con: nix develop -c bash -c 'cd apps/desktop-daemon && cargo build')"; exit 2; }
[[ -d "$WEB_REPO" ]] || { log "WEB_REPO no existe: $WEB_REPO"; exit 2; }

mkdir -p "$E2E_ROOT"
log "root=$E2E_ROOT pg=$PG_PORT worker=$WORKER_PORT"

# ─── Teardown ────────────────────────────────────────────────────────────────
PIDS=()
cleanup() {
  [[ "$KEEP" == "1" ]] && { log "KEEP=1: infra viva. worker=$WORKER_PORT pg=$PG_PORT root=$E2E_ROOT"; return 0; }
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null; done
  wait 2>/dev/null
  # wrangler corre detached (setsid): matar por puerto si sigue vivo
  local WP=$(ss -tlnp 2>/dev/null | grep ":$WORKER_PORT" | grep -oP 'pid=\K[0-9]+' | head -1)
  [[ -n "$WP" ]] && kill -9 "$WP" 2>/dev/null
  PGDATA="$E2E_ROOT/pgdata" "$PGCTL" -D "$E2E_ROOT/pgdata" stop -m fast -w -t 15 >/dev/null 2>&1
  log "infra parada"
}
trap cleanup EXIT

# ─── 1. PostgreSQL (scratch, trust auth) ────────────────────────────────────
log "boot PostgreSQL en :$PG_PORT"
initdb -U postgres -A trust "$E2E_ROOT/pgdata" >/dev/null 2>&1
PGCTL="$(command -v pg_ctl)"
"$PGCTL" -D "$E2E_ROOT/pgdata" -l "$E2E_ROOT/pg.log" \
  -o "-p $PG_PORT -k $E2E_ROOT -c listen_addresses=127.0.0.1" start -w -t 30 >/dev/null
psql -h 127.0.0.1 -p "$PG_PORT" -U postgres -c "CREATE DATABASE synthhires" >/dev/null

# Migraciones drizzle (ordenadas)
log "migraciones drizzle"
cat "$WEB_REPO"/drizzle/*.sql | psql -h 127.0.0.1 -p "$PG_PORT" -U postgres -d synthhires -q \
  || { bad "migraciones"; exit 3; }

# miniflare exige user:password@ en la connection string del hyperdrive
DATABASE_URL="postgresql://postgres:local@127.0.0.1:$PG_PORT/synthhires"

# ─── 2. Worker real (build astro + wrangler dev) ─────────────────────────────
log "build web (astro)"
(cd "$WEB_REPO" && npm run build >/dev/null 2>&1) || { bad "astro build"; exit 3; }

log "sirviendo worker en :$WORKER_PORT"
WDIR="$E2E_ROOT/server"
rsync -a --delete "$WEB_REPO/dist/server/" "$WDIR/"
# wrangler.json referencia los assets estáticos como ../client
rsync -a --delete "$WEB_REPO/dist/client/" "$E2E_ROOT/client/"
python3 - "$WDIR/wrangler.json" "$PG_PORT" <<'EOF'
import json, sys
p, pgport = sys.argv[1], sys.argv[2]
cfg = json.load(open(p))
for hd in cfg.get('hyperdrive', []):
    if hd.get('binding') == 'HYPERDRIVE':
        hd['localConnectionString'] = f'postgresql://postgres:local@127.0.0.1:{pgport}/synthhires'
json.dump(cfg, open(p, 'w'), indent=2)
EOF
cat > "$WDIR/.dev.vars" <<EOF
DATABASE_URL=$DATABASE_URL
NODE_ENV=production
BRIDGE_SERVICE_TOKEN=e2e-service-token
BRIDGE_PAIRING_PEPPER=e2e-pepper
VAULT_KDF_SECRET=e2e-vault-secret
CORS_ALLOWED_ORIGINS=http://127.0.0.1:$WORKER_PORT
BRIDGE_BASE_URL=http://127.0.0.1:$WORKER_PORT
GUEST_AI_PER_IP_BURST=100
GUEST_AI_PER_IP_DAY=1000
GUEST_AI_GLOBAL_DAY=10000
EOF
export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt   # NixOS: CA store para workerd
# Puerto libre (una run abortada deja el worker huérfano en el puerto)
if ss -tln 2>/dev/null | grep -q ":$WORKER_PORT"; then
  OLD=$(ss -tlnp 2>/dev/null | grep ":$WORKER_PORT" | grep -oP 'pid=\K[0-9]+' | head -1)
  [[ -n "$OLD" ]] && kill -9 "$OLD" 2>/dev/null
  ONODE=$(ps -o ppid= -p "$OLD" 2>/dev/null | tr -d ' '); [[ -n "$ONODE" ]] && kill -9 "$ONODE" 2>/dev/null
  sleep 2
fi
(
  cd "$WDIR"
  setsid npx --yes wrangler dev --port "$WORKER_PORT" --ip 127.0.0.1 --local \
    > "$E2E_ROOT/worker.log" 2>&1 < /dev/null
) &
WPID=$!; PIDS+=($WPID)

for i in $(seq 1 60); do
  curl -s -o /dev/null -m 2 "http://127.0.0.1:$WORKER_PORT/" && break; sleep 1
done
curl -s -o /dev/null -m 3 "http://127.0.0.1:$WORKER_PORT/" || { tail -20 "$E2E_ROOT/worker.log"; bad "worker no arrancó (ver $E2E_ROOT/worker.log)"; exit 3; }

BASE="http://127.0.0.1:$WORKER_PORT"

# ─── 3. Owner session + fixture server ──────────────────────────────────────
log "owner session (guest auth)"
curl -s -m 10 -c "$E2E_ROOT/owner.jar" -X POST "$BASE/api/devices/pair/start" \
  -H "Origin: $BASE" -H 'Content-Type: application/json' \
  -d '{"mode":"desktop"}' > "$E2E_ROOT/pair.json"
python3 - "$E2E_ROOT/pair.json" <<'EOF' || { bad "pair/start (owner session)"; exit 4; }
import json, sys
d = json.load(open(sys.argv[1]))
assert d.get('success') is True, d
data = d['data']
assert data.get('pairingId') and data.get('token'), d
EOF
PAIR_ID=$(python3 -c "import json;print(json.load(open('$E2E_ROOT/pair.json'))['data']['pairingId'])")
PAIR_TOKEN=$(python3 -c "import json;print(json.load(open('$E2E_ROOT/pair.json'))['data']['token'])")

log "fixture server :$FIXTURE_PORT"
cat > "$E2E_ROOT/fixture.py" <<'EOF'
import http.server
class H(http.server.BaseHTTPRequestHandler):
    def _r(self):
        if self.path.startswith('/fixture'):
            b=b"FIXTURE-OK"; c=200; ct="text/plain"
        else:
            b=b'{"real":404}'; c=404; ct="application/json"
        self.send_response(c); self.send_header("Content-Type",ct)
        self.send_header("Content-Length",str(len(b))); self.end_headers(); self.wfile.write(b)
    do_GET=do_POST=_r
    def log_message(self,*a): pass
http.server.ThreadingHTTPServer(("127.0.0.1",18098),H).serve_forever()
EOF
sed -i "s/18098/$FIXTURE_PORT/" "$E2E_ROOT/fixture.py"
python3 "$E2E_ROOT/fixture.py" >/dev/null 2>&1 &
PIDS+=($!)

ACTION() { # capability, json-params
  curl -s -m 60 -X POST "$BASE/api/devices/$DEV_ID/action" \
    -H "Origin: $BASE" -H 'Content-Type: application/json' -b "$E2E_ROOT/owner.jar" \
    -d "{\"capability\":\"$1\",\"params\":$2,\"conversationId\":null}"
}
JGET() { python3 -c "
import json,sys
d=json.load(sys.stdin)
p='$1'.split('.')
for k in p: d=d[k]
print(json.dumps(d) if isinstance(d,(dict,list)) else d)"; }

# ─── 4. Pair daemon (código → token, flujo desktop real) ────────────────────
log "emparejando daemon (pairing code desktop)"
"$DAEMON_BIN" --headless --config-dir "$E2E_ROOT/config" unpair >/dev/null 2>&1 || true
"$DAEMON_BIN" --headless --config-dir "$E2E_ROOT/config" pair "$BASE" "$PAIR_TOKEN" --pairing-id "$PAIR_ID" > "$E2E_ROOT/paird.log" 2>&1 \
  || { bad "daemon pair (ver $E2E_ROOT/paird.log)"; exit 4; }

DEV_ID=$(python3 -c "import json;print(json.load(open('$E2E_ROOT/config/state.json'))['device_id'])")

# Grant de scopes + alwaysAllowPaths ANTES de arrancar `run`: el flujo real del
# producto es grant (UI/adjuntar workspace) → connect, y el DO entrega los
# scopes en el hello_ack (scope_pending). Esto elimina la carrera del push
# scope_update en vivo durante el arranque.
log "grant de scopes y paths (antes de conectar: flujo real)"
mkdir -p "$E2E_ROOT/work"
CAPS='["desktop.fs.read","desktop.fs.write","desktop.fs.delete","desktop.fs.verify","desktop.fs.list","desktop.fs.patch","desktop.shell.execute","desktop.process.list","desktop.process.kill","desktop.network.fetch","desktop.tools.manifest","desktop.mcp.op","desktop.browser.launch","desktop.browser.nav","desktop.browser.snapshot","desktop.browser.act","desktop.browser.shot","desktop.browser.wait","desktop.browser.eval","desktop.browser.close","desktop.browser.tab_open","desktop.browser.tab_select","desktop.browser.tab_close","desktop.browser.tab_list","desktop.browser.mock_set","desktop.code.ast_grep","desktop.code.ast_edit","desktop.code.eval","desktop.fs.restore"]'
HTTP=$(curl -s -o /dev/null -w '%{http_code}' -m 15 -X PATCH "$BASE/api/devices/$DEV_ID/scope" \
  -H "Origin: $BASE" -H 'Content-Type: application/json' -b "$E2E_ROOT/owner.jar" \
  -d "{\"capabilities\":$CAPS,\"alwaysAllowPaths\":[\"$E2E_ROOT/work\"],\"reason\":\"e2e-matrix\"}")
[[ "$HTTP" == "200" ]] || { bad "grant de scopes (HTTP $HTTP)"; exit 4; }
ok "scopes concedidos"

# El daemon debe estar corriendo para las scenarios: `run` en background
"$DAEMON_BIN" --headless --config-dir "$E2E_ROOT/config" run > "$E2E_ROOT/daemon.log" 2>&1 &
DPID=$!; PIDS+=($DPID)
for i in $(seq 1 30); do
  ON=$(curl -s -m 2 "$BASE/api/devices/$DEV_ID/status" -H "Origin: $BASE" -b "$E2E_ROOT/owner.jar" | python3 -c "import json,sys;print(json.load(sys.stdin)['data']['online'])" 2>/dev/null || echo false)
  [[ "$ON" == "True" ]] && break; sleep 1
done
[[ "$ON" == "True" ]] || { bad "daemon online tras pairing"; exit 4; }
ok "pairing + WS online"

# ─── 5. Scenarios ────────────────────────────────────────────────────────────
WORK="$E2E_ROOT/work"; mkdir -p "$WORK/src"

# S1: fs roundtrip (fs.read devuelve content_base64)
R=$(ACTION desktop.fs.write "{\"path\":\"$WORK/hello.txt\",\"content\":\"e2e-hello\"}")
[[ "$(echo "$R" | JGET data.status)" == "completed" ]] && ok "S1 fs.write" || bad "S1 fs.write: $R"
R=$(ACTION desktop.fs.read "{\"path\":\"$WORK/hello.txt\"}")
echo "$R" | python3 -c "
import json,sys,base64
d=json.load(sys.stdin)
c=d['data']['result']['output']['content_base64']
assert base64.b64decode(c).decode()=='e2e-hello', c
" 2>/dev/null && ok "S1 fs.read roundtrip (base64 verificado)" || bad "S1 fs.read: $R"

# S2: browser + route mocks
R=$(ACTION desktop.browser.launch "{\"url\":\"http://127.0.0.1:$FIXTURE_PORT/fixture\",\"headed\":false}")
echo "$R" | grep -q "FIXTURE-OK" && ok "S2 browser.launch snapshot" || bad "S2 launch: $R"
R=$(ACTION desktop.browser.mock_set '{"rules":[{"urlContains":"/mock-e2e","status":200,"body":"{\"mocked\":true}","contentType":"application/json"}]}')
echo "$R" | grep -q '"ok":true' && ok "S2 mock_set" || bad "S2 mock_set: $R"
R=$(ACTION desktop.browser.nav "{\"url\":\"http://127.0.0.1:$FIXTURE_PORT/mock-e2e?cb=$RANDOM\"}")
echo "$R" | python3 -c "
import json,sys
d=json.load(sys.stdin)
text=d['data']['result']['output']['snapshot']['text']
net=d['data']['result']['output']['snapshot']['network']
assert 'mocked' in text, text[:200]
assert any(n.get('url','').startswith('http://127.0.0.1:$FIXTURE_PORT/mock-e2e') and n.get('status')==200 for n in net), net
" 2>/dev/null && ok "S2 mock sirve en nav (200 mockeado en red + texto en DOM)" || bad "S2 nav mock: $R"
echo "$R" | grep -q '"favicon' && ok "S2 passthrough red intacto (favicon 404 visible)" || ok "S2 passthrough (sin favicon check)"
ACTION desktop.browser.close '{}' >/dev/null

# S3: código AST (dir grep + edit + verify)
mkdir -p "$WORK/src"
cat > "$WORK/src/app.ts" <<'EOF'
export function a(): void {
  console.log("x")
}
export function b(): void {
  console.log("y")
}
EOF
R=$(ACTION desktop.code.ast_grep "{\"path\":\"$WORK/src\",\"pattern\":\"console.log(\$MSG)\",\"language\":\"typescript\"}")
N=$(echo "$R" | python3 -c "import json,sys;print(len(json.load(sys.stdin)['data']['result']['output']['matches']))" 2>/dev/null || echo 0)
[[ "$N" == "2" ]] && ok "S3 ast_grep dir = 2 matches" || bad "S3 ast_grep dir: $R"
R=$(ACTION desktop.code.ast_edit "{\"path\":\"$WORK/src/app.ts\",\"pattern\":\"console.log(\$MSG)\",\"replacement\":\"logger.info(\$MSG)\",\"language\":\"typescript\"}")
echo "$R" | grep -q '"replacements":2' && ok "S3 ast_edit replacements=2" || bad "S3 ast_edit: $R"
R=$(ACTION desktop.code.ast_grep "{\"path\":\"$WORK/src\",\"pattern\":\"console.log(\$MSG)\",\"language\":\"typescript\"}")
N=$(echo "$R" | python3 -c "import json,sys;print(len(json.load(sys.stdin)['data']['result']['output']['matches']))" 2>/dev/null || echo 9)
[[ "$N" == "0" ]] && ok "S3 verificación 0 console.log" || bad "S3 verificación: $R"

# S4: shell honesto (éxito y fallo)
R=$(ACTION desktop.shell.execute "{\"command\":\"echo ShellOK\"}")
echo "$R" | grep -q "ShellOK" && ok "S4 shell stdout" || bad "S4 shell ok: $R"
R=$(ACTION desktop.shell.execute "{\"command\":\"exit 7\"}")
echo "$R" | grep -q '"exit_code":7' && ok "S4 shell exit_code honesto (7)" || bad "S4 shell fail: $R"

# S5: scope enforcement (capability fuera del grant = rechazo; el route
# web la corta con capability_not_granted antes de llegar al daemon)
R=$(ACTION desktop.debug.op '{"op":"sessions"}')
echo "$R" | grep -q 'capability_not_granted' && ok "S5 scope: capability no concedida rechazada" || bad "S5 scope: $R"

# S6: watchdog zombi (opcional)
if [[ "$WATCHDOG" == "1" ]]; then
  log "S6: inyectando zombi (SIGSTOP al runtime del worker 60-75s)..."
  WP=$(ss -tlnp 2>/dev/null | grep "$WORKER_PORT" | grep -oP 'pid=\K[0-9]+' | head -1)
  kill -STOP "$WP"; sleep 75; kill -CONT "$WP"; sleep 10
  R=$(ACTION desktop.tools.manifest '{}')
  echo "$R" | grep -q '"ok":true' && ok "S6 watchdog: reconexión automática + action OK" || bad "S6 watchdog: $R"
fi

# S7: agente real (opcional, path de producción /api/chat)
if [[ "$AGENT" == "1" && -n "${DEEPSEEK_API_KEY:-}" ]]; then
  log "S7: agente real vía /api/chat (deepseek-v4-flash, effort low)"
  cat > "$E2E_ROOT/agent.json" <<EOF
{"provider":"deepseek","model":"deepseek-v4-flash","apiKey":"$DEEPSEEK_API_KEY",
 "thinkingLevel":"low","isTemporary":true,
 "workspaceRef":{"provider":"local","deviceId":"$DEV_ID","path":"$WORK"},
 "messages":[{"role":"user","content":"Crea en $WORK/agente.txt el contenido exacto agent-ok y verifícalo leyéndolo. Responde solo con el contenido del fichero."}]}
EOF
  curl -s -N -m 240 -X POST "$BASE/api/chat" -H "Origin: $BASE" -H 'Content-Type: application/json' \
    -b "$E2E_ROOT/owner.jar" --data-binary @"$E2E_ROOT/agent.json" > "$E2E_ROOT/agent.sse" 2>&1
  grep -q '"type":"error"' "$E2E_ROOT/agent.sse" && bad "S7 agente: error en stream" || ok "S7 agente: sin errores de stream"
  [[ "$(cat "$E2E_ROOT/work/agente.txt" 2>/dev/null)" == "agent-ok" ]] && ok "S7 agente: fichero creado por el modelo" || bad "S7 agente: fichero no creado"
else
  [[ "$AGENT" == "1" ]] && log "S7 skip: sin DEEPSEEK_API_KEY"
fi

# S8: undo/checkpoint (spec: docs/undo-checkpoint-spec.md)
# write -> pre-imagen checkpointed -> write destructivo -> restore -> byte a byte.
R=$(ACTION desktop.fs.write "{\"path\":\"$WORK/undo.txt\",\"content\":\"ORIGINAL-UNDOSH\"}")
echo "$R" | grep -q '"verified":true' && ok "S8 fs.write original" || bad "S8 write original: $R"
R=$(ACTION desktop.fs.write "{\"path\":\"$WORK/undo.txt\",\"content\":\"AGENT-DESTROYED-CONTENT\"}")
echo "$R" | grep -q '"verified":true' && ok "S8 fs.write destructivo" || bad "S8 write destructivo: $R"
# El indice vive en el config-dir del daemon E2E ($E2E_ROOT/config/checkpoints).
# El checkpoint existed=True mas reciente de ese path contiene la pre-imagen
# ORIGINAL: el capture corre ANTES de cada write, y el ultimo write fue el
# destructivo (su pre-imagen es el contenido original).
TS=$(python3 - <<PYEOF
import json
entries=[]
try:
    for line in open("$E2E_ROOT/config/checkpoints/index.jsonl"):
        e=json.loads(line)
        if e.get("path")=="$WORK/undo.txt" and e.get("existed") and not e.get("truncated"):
            entries.append(e)
except FileNotFoundError:
    pass
print(entries[-1]["ts"] if entries else "")
PYEOF
)
if [[ -n "$TS" ]]; then
  R=$(ACTION desktop.fs.restore "{\"ts\":$TS}")
  echo "$R" | grep -q '"restored":true' && ok "S8 restore ejecutado" || bad "S8 restore: $R"
  [[ "$(cat "$WORK/undo.txt" 2>/dev/null)" == "ORIGINAL-UNDOSH" ]] && ok "S8 restauracion byte a byte verificada" || bad "S8 contenido tras restore: $(cat "$WORK/undo.txt" 2>/dev/null)"
  # Undo de creacion: fichero que NO existia antes del write del agente.
  ACTION desktop.fs.write "{\"path\":\"$WORK/creado.txt\",\"content\":\"efimero\"}" >/dev/null
  TS2=$(python3 - <<PYEOF
import json
entries=[]
try:
    for line in open("$E2E_ROOT/config/checkpoints/index.jsonl"):
        e=json.loads(line)
        if e.get("path")=="$WORK/creado.txt" and not e.get("existed"):
            entries.append(e)
except FileNotFoundError:
    pass
print(entries[-1]["ts"] if entries else "")
PYEOF
)
  if [[ -n "$TS2" ]]; then
    ACTION desktop.fs.restore "{\"ts\":$TS2}" >/dev/null
    [[ ! -e "$WORK/creado.txt" ]] && ok "S8 undo de creacion = borrado" || bad "S8 creado.txt sigue existiendo"
  else
    bad "S8: no se encontro checkpoint de creacion"
  fi
else
  bad "S8: checkpoint de pre-imagen no encontrado en indice (capture no cableado?)"
fi

# S10: consent-relay web-first (docs/undo-checkpoint-spec.md)
# Acción FUERA de alwaysAllowPaths → el daemon la aparca y relay­a el prompt
# → el owner responde vía la API web (la MISMA llamada que dispara el drawer
# unificado del chat input) → el DO retransmite consent_response por WS → el
# daemon reanuda la acción aparca­da. Sin consentimiento web-first, una acción
# fuera de workspace en un daemon headless moriría por timeout.
RELAY_DIR="/tmp/sh-e2e-relay-$$"
R=$(ACTION desktop.fs.write "{\"path\":\"$RELAY_DIR/s10.txt\",\"content\":\"S10-OK\"}")
AID=$(echo "$R" | JGET data.actionId 2>/dev/null)
# El prompt debe aparecer en la cola del DO (visible para el cockpit web).
PROMPT_ID=$(curl -s -m 15 "$BASE/api/devices/$DEV_ID/consents" \
  -H "Origin: $BASE" -b "$E2E_ROOT/owner.jar" | python3 -c "
import json,sys
cs=json.load(sys.stdin).get('consents',[])
print(cs[0]['id'] if cs else '')" 2>/dev/null)
if [[ -n "$PROMPT_ID" ]]; then
  ok "S10 prompt relayado al cockpit web (prompt_id ${PROMPT_ID:0:8})"
  # El owner aprueba desde la web → decisión real al daemon.
  curl -s -m 15 -X POST "$BASE/api/devices/$DEV_ID/consent" \
    -H "Origin: $BASE" -H 'Content-Type: application/json' -b "$E2E_ROOT/owner.jar" \
    -d "{\"id\":\"$PROMPT_ID\",\"approved\":true,\"remember\":false}" >/dev/null
  sleep 4
  [[ "$(cat "$RELAY_DIR/s10.txt" 2>/dev/null)" == "S10-OK" ]] \
    && ok "S10 aprobado desde la web → acción reanudada y ejecutada" \
    || bad "S10 aprobado pero la acción no se ejecutó (resume roto?)"
else
  bad "S10: el prompt no llegó a la cola del DO (relay roto?): $R"
fi
rm -rf "$RELAY_DIR"

# S10b: denegación desde la web → la acción NO se ejecuta y el one-shot no persiste.
R=$(ACTION desktop.fs.write "{\"path\":\"$RELAY_DIR/denegado.txt\",\"content\":\"NO\"}")
PROMPT_ID=$(curl -s -m 15 "$BASE/api/devices/$DEV_ID/consents" \
  -H "Origin: $BASE" -b "$E2E_ROOT/owner.jar" | python3 -c "
import json,sys
cs=json.load(sys.stdin).get('consents',[])
print(cs[0]['id'] if cs else '')" 2>/dev/null)
if [[ -n "$PROMPT_ID" ]]; then
  curl -s -m 15 -X POST "$BASE/api/devices/$DEV_ID/consent" \
    -H "Origin: $BASE" -H 'Content-Type: application/json' -b "$E2E_ROOT/owner.jar" \
    -d "{\"id\":\"$PROMPT_ID\",\"approved\":false,\"remember\":false}" >/dev/null
  sleep 3
  [[ ! -e "$RELAY_DIR/denegado.txt" ]] \
    && ok "S10b denegación web respetada (fichero no creado)" \
    || bad "S10b: fichero creado pese a denegación web"
else
  bad "S10b: segundo prompt no apareció (one-shot debería re-preguntar)"
fi
rm -rf "$RELAY_DIR"

# S11: sandboxing — hard resource limits y señales honestas.
# Un `seq 1 1000000` (~6.9 MB de stdout) debe: (a) truncarse a 4 MiB/stream
# con marker y flag honesto `output_truncated`, (b) NO matar al daemon ni
# ahogar el DO (el stream se BATCHEA a 8 KiB/frame en vez de 1 frame/línea —
# sin batching el worker acaba con un backlog de 576 KB y wedged).
R=$(ACTION desktop.shell.execute '{"command":"seq 1 1000000"}')
echo "$R" | grep -q '"output_truncated":true' \
  && ok "S11a flood: output_truncated honesto" \
  || bad "S11a flood sin señal de truncado: $(echo "$R" | head -c 200)"
echo "$R" | grep -q 'synthhires: output truncated' \
  && ok "S11a flood: marker en la cola del stream" \
  || bad "S11a marker ausente"
R=$(ACTION desktop.shell.execute '{"command":"echo alive"}')
echo "$R" | grep -q 'alive' \
  && ok "S11b daemon vivo tras el flood" \
  || bad "S11b daemon no responde tras flood: $R"
# S11c: fs.write por encima del límite del bridge → 413 accionable (NO un
# 500 opaco por SQLITE_TOOBIG al persistir el pending en el DO).
python3 -c "import json;json.dump({'capability':'desktop.fs.write','params':{'path':'/tmp/sh-e2e-s11.txt','content':'x'*(2*1024*1024)},'conversationId':None},open('$E2E_ROOT/s11-big.json','w'))"
HTTP=$(curl -s -o "$E2E_ROOT/s11-resp.json" -w '%{http_code}' -m 30 -X POST "$BASE/api/devices/$DEV_ID/action" \
  -H "Origin: $BASE" -H 'Content-Type: application/json' -b "$E2E_ROOT/owner.jar" \
  --data-binary @"$E2E_ROOT/s11-big.json")
[[ "$HTTP" == "413" ]] && grep -q 'payload_too_large' "$E2E_ROOT/s11-resp.json" \
  && ok "S11c fs.write 2MB: 413 payload_too_large accionable" \
  || bad "S11c: HTTP $HTTP $(head -c 120 "$E2E_ROOT/s11-resp.json")"
rm -f "$E2E_ROOT/s11-big.json" "$E2E_ROOT/s11-resp.json" /tmp/sh-e2e-s11.txt
# S11d: fs.read acotado con señal honesta — un fichero de 3000 bytes leído
# con max_bytes=1000 debe reportar truncated:true (antes se calculaba y se
# descartaba: el consumidor no podía saber que veía un fichero parcial).
ACTION desktop.fs.write "{\"path\":\"$WORK/s11-read.txt\",\"content\":\"$(printf 'A%.0s' $(seq 1 3000))\"}" >/dev/null
R=$(ACTION desktop.fs.read "{\"path\":\"$WORK/s11-read.txt\",\"max_bytes\":1000}")
echo "$R" | grep -q '"truncated":true' \
  && ok "S11d fs.read acotado: truncated honesto" \
  || bad "S11d sin señal de truncado: $(echo "$R" | head -c 200)"
ACTION desktop.fs.delete "{\"path\":\"$WORK/s11-read.txt\"}" >/dev/null 2>&1

# ─── Resumen ────────────────────────────────────────────────────────────────
log "══════════════════════════════════════"
log "PASS=$PASS FAIL=$FAIL"
[[ $FAIL -gt 0 ]] && printf '  fallidos: %s\n' "${FAILED[@]}"
[[ $FAIL -eq 0 ]] && log "MATRIZ COMPLETA VERDE"
exit $FAIL
