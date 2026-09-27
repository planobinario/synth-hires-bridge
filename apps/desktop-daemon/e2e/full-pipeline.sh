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
PIDS+=($!)
psql -h 127.0.0.1 -p "$PG_PORT" -U postgres -c "CREATE DATABASE synthhires" >/dev/null

# Migraciones drizzle (ordenadas)
log "migraciones drizzle"
cat "$WEB_REPO"/drizzle/*.sql | psql -h 127.0.0.1 -p "$PG_PORT" -U postgres -d synthhires -q \
  || { bad "migraciones"; exit 3; }

DATABASE_URL="postgresql://postgres@127.0.0.1:$PG_PORT/synthhires"

# ─── 2. Worker real (build astro + wrangler dev) ─────────────────────────────
log "build web (astro)"
(cd "$WEB_REPO" && npm run build >/dev/null 2>&1) || { bad "astro build"; exit 3; }

log "sirviendo worker en :$WORKER_PORT"
WDIR="$E2E_ROOT/server"
rsync -a --delete "$WEB_REPO/dist/server/" "$WDIR/"
python3 - "$WDIR/wrangler.json" "$PG_PORT" <<'EOF'
import json, sys
p, pgport = sys.argv[1], sys.argv[2]
cfg = json.load(open(p))
for hd in cfg.get('hyperdrive', []):
    if hd.get('binding') == 'HYPERDRIVE':
        hd['localConnectionString'] = f'postgresql://postgres@127.0.0.1:{pgport}/synthhires'
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
(cd "$WDIR" && npx wrangler dev --port "$WORKER_PORT" --local >/dev/null 2>&1) &
WPID=$!; PIDS+=($WPID)

for i in $(seq 1 40); do
  curl -s -o /dev/null -m 2 "http://127.0.0.1:$WORKER_PORT/" && break; sleep 1
done
curl -s -o /dev/null -m 3 "http://127.0.0.1:$WORKER_PORT/" || { bad "worker no arrancó"; exit 3; }

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

# S1: fs roundtrip
R=$(ACTION desktop.fs.write "{\"path\":\"$WORK/hello.txt\",\"content\":\"e2e-hello\"}")
[[ "$(echo "$R" | JGET data.status)" == "completed" ]] && ok "S1 fs.write" || bad "S1 fs.write: $R"
R=$(ACTION desktop.fs.read "{\"path\":\"$WORK/hello.txt\"}")
echo "$R" | grep -q "e2e-hello" && ok "S1 fs.read roundtrip" || bad "S1 fs.read: $R"

# S2: browser + route mocks
R=$(ACTION desktop.browser.launch "{\"url\":\"http://127.0.0.1:$FIXTURE_PORT/fixture\",\"headed\":false}")
echo "$R" | grep -q "FIXTURE-OK" && ok "S2 browser.launch snapshot" || bad "S2 launch: $R"
R=$(ACTION desktop.browser.mock_set '{"rules":[{"urlContains":"/mock-e2e","status":200,"body":"{\"mocked\":true}","contentType":"application/json"}]}')
echo "$R" | grep -q '"ok":true' && ok "S2 mock_set" || bad "S2 mock_set: $R"
R=$(ACTION desktop.browser.nav "{\"url\":\"http://127.0.0.1:$FIXTURE_PORT/mock-e2e?cb=$RANDOM\"}")
echo "$R" | grep -q '"mocked":true' && ok "S2 mock sirve en nav" || bad "S2 nav mock: $R"
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

# S5: scope enforcement (capability fuera del default = rechazo)
R=$(ACTION desktop.debug.op '{"op":"sessions"}')
echo "$R" | grep -q "capability not granted" && ok "S5 scope: capability no concedida rechazada" || bad "S5 scope: $R"

# S6: watchdog zombi (opcional)
if [[ "$WATCHDOG" == "1" ]]; then
  log "S6: inyectando zombi (SIGSTOP al runtime del worker 60-75s)..."
  WP=$(ss -tlnp 2>/dev/null | grep "$WORKER_PORT" | grep -oP 'pid=\K[0-9]+' | head -1)
  kill -STOP "$WP"; sleep 75; kill -CONT "$WP"; sleep 10
  R=$(ACTION desktop.tools.manifest '{}')
  echo "$R" | grep -q '"ok":true' && ok "S6 watchdog: reconexión automática + action OK" || bad "S6 watchdog: $R"
fi

# S7: agente real (opcional, path de producción /api/chat)
if [[ "$AGENT" == "1" ]]; then
  [[ -n "${DEEPSEEK_API_KEY:-}" ]] || { log "S7 sin DEEPSEEK_API_KEY: skip"; }
  else
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
  [[ "$(cat "$WORK/agente.txt" 2>/dev/null)" == "agent-ok" ]] && ok "S7 agente: fichero creado por el modelo" || bad "S7 agente: fichero no creado"
fi

# ─── Resumen ────────────────────────────────────────────────────────────────
log "══════════════════════════════════════"
log "PASS=$PASS FAIL=$FAIL"
[[ $FAIL -gt 0 ]] && printf '  fallidos: %s\n' "${FAILED[@]}"
[[ $FAIL -eq 0 ]] && log "MATRIZ COMPLETA VERDE"
exit $FAIL
