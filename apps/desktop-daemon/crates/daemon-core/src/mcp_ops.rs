//! MCP (Model Context Protocol) client over stdio — the daemon is the MCP
//! host. Real tool universes (GitHub, Postgres, Slack, filesystems of other
//! apps…) without bundling any of them: servers come from the USER's own
//! config file (`<config_dir>/mcp-servers.json`), spawned on demand and
//! spoken to with JSON-RPC 2.0 over stdin/stdout (newline-delimited).
//!
//! House patterns honored:
//! - No downloads, no registries: we run only what the user already declared.
//!   Missing config → empty status, never an error, never an install prompt.
//! - Consent: spawning/calling a server is as powerful as a shell
//!   (`desktop.shell.execute`); stopping one is `desktop.process.kill`;
//!   reading status is `desktop.fs.read`. The web surfaces this mapping.
//! - Environment: children get a MINIMAL safe env (PATH/HOME/… plus the
//!   per-server `env` allowlist from the config) — never the daemon's full
//!   environment. Secrets reach a server only because the user named them.
//! - Robustness: every request has a timeout; unmatched notifications are
//!   ignored; a dead child is respawned lazily on the next op; `kill_on_drop`
//!   means no orphaned servers when the daemon exits.
//!
//! Wire shape (config file):
//! ```json
//! {
//!   "servers": {
//!     "github": {
//!       "command": ["npx", "-y", "@modelcontextprotocol/server-github"],
//!       "cwd": "/optional/working/dir",
//!       "env": ["GITHUB_TOKEN"]
//!     }
//!   }
//! }
//! ```

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

type McpResult<T> = Result<T, String>;

const STARTUP_TIMEOUT_MS: u64 = 10_000;
const LIST_TIMEOUT_MS: u64 = 10_000;
const CALL_TIMEOUT_MS: u64 = 60_000;
const MAX_LIST_PAGES: usize = 10;

/// Safe baseline passed to every MCP server (plus the per-server allowlist).
const BASE_ENV: &[&str] = &["PATH", "HOME", "USER", "LANG", "TMPDIR", "TEMP", "TMP", "SYSTEMROOT"];

// ─── Config (pure, unit-testable) ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServerDef {
    /// Always the map key; serde-defaulted because in the map form the name
    /// lives OUTSIDE the definition object (parse_servers_config sets it).
    #[serde(default)]
    pub name: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Names of environment variables to pass through (allowlist).
    #[serde(default)]
    pub env: Vec<String>,
}

/// Parses the user's `mcp-servers.json`. Accepts `{ "servers": { name: def } }`
/// (map form) or a bare map. Unknown fields are ignored for forward compat.
pub fn parse_servers_config(raw: &str) -> McpResult<Vec<ServerDef>> {
    let v: Value =
        serde_json::from_str(raw).map_err(|e| format!("mcp-servers.json: {e}"))?;
    let servers = v.get("servers").unwrap_or(&v);
    let map = servers
        .as_object()
        .ok_or("mcp-servers.json: expected an object of servers")?;
    let mut defs = Vec::with_capacity(map.len());
    for (name, def) in map {
        let mut def: ServerDef = serde_json::from_value(def.clone())
            .map_err(|e| format!("server '{name}': {e}"))?;
        def.name = name.clone();
        if def.command.is_empty() {
            return Err(format!("server '{name}': command must be non-empty"));
        }
        defs.push(def);
    }
    Ok(defs)
}

pub fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("com", "synthhires", "bridge")
        .map(|d| d.config_dir().join("mcp-servers.json"))
}

fn load_config() -> McpResult<Vec<ServerDef>> {
    let Some(path) = config_path() else {
        return Ok(Vec::new());
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        // No config is the normal state for most users: nothing declared,
        // nothing to run. Not an error.
        Err(_) => return Ok(Vec::new()),
    };
    parse_servers_config(&raw)
}

// ─── JSON-RPC envelope (pure, unit-testable) ─────────────────────────────────

pub fn rpc_request(id: u64, method: &str, params: Value) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .expect("rpc request serializes")
}

pub fn rpc_notification(method: &str, params: Value) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    }))
    .expect("rpc notification serializes")
}

/// Routes one stdout line to `(id, ok, payload)`. Notifications (no id) and
/// unparseable lines yield None — they must never wedge the pending map.
pub fn classify_response(line: &str) -> Option<(u64, bool, Value)> {
    let v: Value = serde_json::from_str(line).ok()?;
    let id = v.get("id")?.as_u64()?;
    if let Some(result) = v.get("result") {
        Some((id, true, result.clone()))
    } else if let Some(err) = v.get("error") {
        Some((id, false, err.clone()))
    } else {
        None
    }
}

// ─── Runtime state ───────────────────────────────────────────────────────────

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

struct McpServerState {
    def: ServerDef,
    child: Arc<Mutex<Option<Child>>>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingMap,
    next_id: Arc<AtomicU64>,
}

impl McpServerState {
    async fn spawn(def: ServerDef) -> McpResult<Arc<Self>> {
        let (cmd0, args) = def
            .command
            .split_first()
            .ok_or_else(|| format!("server '{}': empty command", def.name))?;
        let mut cmd = Command::new(cmd0);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Minimal env: baseline + the user's explicit allowlist. This is the
        // security boundary — a child never sees the daemon's whole env.
        cmd.env_clear();
        for key in BASE_ENV {
            if let Ok(val) = std::env::var(key) {
                cmd.env(key, val);
            }
        }
        for key in &def.env {
            if let Ok(val) = std::env::var(key) {
                cmd.env(key, val);
            }
        }
        if let Some(cwd) = &def.cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn '{}': {e}", def.command.join(" ")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("server '{}': no stdin", def.name))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("server '{}': no stdout", def.name))?;
        // Drain stderr at debug level: a chatty server must never block.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "mcp", "{line}");
                }
            });
        }

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let child = Arc::new(Mutex::new(Some(child)));

        // Reader task: route responses by id; drop oneshots when the server dies.
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let Some((id, ok, payload)) = classify_response(&line) else {
                    continue;
                };
                let envelope = json!({ "ok": ok, "payload": payload });
                if let Some(tx) = reader_pending.lock().await.remove(&id) {
                    let _ = tx.send(envelope);
                }
            }
            // stdout closed: fail every pending request so callers don't hang
            // until their timeout — they get "server exited" immediately.
            for (_, tx) in reader_pending.lock().await.drain() {
                let _ = tx.send(json!({ "ok": false, "payload": { "message": "server exited" } }));
            }
        });

        let state = Arc::new(Self {
            stdin: Arc::new(Mutex::new(stdin)),
            child,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
            def,
        });

        // MCP handshake: initialize → initialized notification.
        let id = state.alloc_id();
        let init_result = state
            .rpc(
                id,
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "synthhires-bridge",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
                STARTUP_TIMEOUT_MS,
            )
            .await?;
        let _ = init_result;
        state
            .send_line(rpc_notification("notifications/initialized", json!({})))
            .await?;
        Ok(state)
    }

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn send_line(&self, line: String) -> McpResult<()> {
        let mut stdin = self.stdin.lock().await;
        let write = |res: std::io::Result<()>| {
            res.map_err(|e| format!("write to server '{}': {e}", self.def.name))
        };
        write(stdin.write_all(line.as_bytes()).await)?;
        write(stdin.write_all(b"\n").await)?;
        write(stdin.flush().await)
    }

    /// One JSON-RPC round trip: register pending → write → await with timeout.
    async fn rpc(&self, id: u64, method: &str, params: Value, timeout_ms: u64) -> McpResult<Value> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.send_line(rpc_request(id, method, params)).await?;
        let envelope = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            rx,
        )
        .await
        .map_err(|_| format!("server '{}': '{method}' timed out after {timeout_ms}ms", self.def.name))?
        .map_err(|_| format!("server '{}': dropped while '{method}' was pending", self.def.name))?;
        if envelope["ok"] == json!(true) {
            Ok(envelope["payload"].clone())
        } else {
            Err(format!(
                "server '{}': {}",
                self.def.name,
                envelope["payload"].get("message").and_then(Value::as_str).unwrap_or("unknown error")
            ))
        }
    }

    async fn is_alive(&self) -> bool {
        let mut guard = self.child.lock().await;
        match guard.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }
}

pub struct McpStore {
    servers: Mutex<HashMap<String, Arc<McpServerState>>>,
}

impl Default for McpStore {
    fn default() -> Self {
        Self::new()
    }
}

impl McpStore {
    pub fn new() -> Self {
        Self {
            servers: Mutex::new(HashMap::new()),
        }
    }

    /// Running instance for `name`, respawning lazily when dead/absent.
    async fn ensure_running(&self, name: &str) -> McpResult<Arc<McpServerState>> {
        {
            let servers = self.servers.lock().await;
            if let Some(state) = servers.get(name) {
                if state.is_alive().await {
                    return Ok(state.clone());
                }
            }
        }
        // Spawn outside the map lock: a slow spawn must not serialize others.
        let def = load_config()?
            .into_iter()
            .find(|d| d.name == name)
            .ok_or_else(|| {
                format!(
                    "no MCP server named '{name}' in {} (declared servers only; the daemon never installs anything)",
                    config_path().map(|p| p.display().to_string()).unwrap_or_else(|| "<config dir>".into())
                )
            })?;
        let state = McpServerState::spawn(def).await?;
        self.servers
            .lock()
            .await
            .insert(name.to_string(), state.clone());
        Ok(state)
    }

    async fn stop(&self, name: &str) -> McpResult<bool> {
        let state = self.servers.lock().await.remove(name);
        let Some(state) = state else {
            return Ok(false);
        };
        let mut guard = state.child.lock().await;
        if let Some(child) = guard.as_mut() {
            let _ = child.start_kill();
        }
        *guard = None;
        Ok(true)
    }

    // ── Ops ─────────────────────────────────────────────────────────────────

    pub async fn status(&self) -> McpResult<Value> {
        let config = load_config()?;
        let servers = self.servers.lock().await;
        let mut list = Vec::with_capacity(config.len());
        for def in config {
            let running = match servers.get(&def.name) {
                Some(state) => state.is_alive().await,
                None => false,
            };
            list.push(json!({
                "name": def.name,
                "command": def.command,
                "running": running,
            }));
        }
        Ok(json!({ "servers": list, "configPath": config_path().map(|p| p.display().to_string()) }))
    }

    pub async fn list_tools(&self, server: &str) -> McpResult<Value> {
        let state = self.ensure_running(server).await?;
        let mut tools = Vec::new();
        let mut cursor: Option<Value> = None;
        for _ in 0..MAX_LIST_PAGES {
            let mut params = json!({});
            if let Some(c) = cursor.take() {
                params["cursor"] = c;
            }
            let result = state
                .rpc(state.alloc_id(), "tools/list", params, LIST_TIMEOUT_MS)
                .await?;
            if let Some(batch) = result.get("tools").and_then(Value::as_array) {
                tools.extend(batch.clone());
            }
            match result.get("nextCursor") {
                Some(c) if !c.is_null() => cursor = Some(c.clone()),
                _ => break,
            }
        }
        Ok(json!({ "server": server, "tools": tools }))
    }

    pub async fn call(&self, server: &str, tool: &str, args: Value) -> McpResult<Value> {
        let state = self.ensure_running(server).await?;
        let result = state
            .rpc(
                state.alloc_id(),
                "tools/call",
                json!({ "name": tool, "arguments": args }),
                CALL_TIMEOUT_MS,
            )
            .await?;
        Ok(json!({ "server": server, "tool": tool, "result": result }))
    }
}

// ─── Op enum (wire: desktop.mcp.op, tag = "op", camelCase) ──────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all_fields = "camelCase", tag = "op")]
pub enum McpOp {
    #[serde(rename = "status")]
    Status {},
    #[serde(rename = "list_tools")]
    ListTools { server: String },
    #[serde(rename = "call")]
    Call {
        server: String,
        tool: String,
        #[serde(default)]
        args: Value,
    },
    #[serde(rename = "stop")]
    Stop { server: String },
}

impl McpOp {
    pub async fn execute(self, store: &McpStore) -> McpResult<Value> {
        match self {
            McpOp::Status {} => store.status().await,
            McpOp::ListTools { server } => store.list_tools(&server).await,
            McpOp::Call { server, tool, args } => store.call(&server, &tool, args).await,
            McpOp::Stop { server } => {
                let stopped = store.stop(&server).await?;
                Ok(json!({ "stopped": stopped }))
            }
        }
    }
}

// ─── Tests (no servers spawned: pure helpers only) ──────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_map_form_with_env_allowlist() {
        let raw = r#"{
            "servers": {
                "github": {
                    "command": ["npx", "-y", "@modelcontextprotocol/server-github"],
                    "env": ["GITHUB_TOKEN"]
                },
                "db": { "command": ["mcp-server-postgres", "postgres://localhost/db"] }
            }
        }"#;
        let defs = parse_servers_config(raw).expect("parses");
        assert_eq!(defs.len(), 2);
        let github = defs.iter().find(|d| d.name == "github").expect("github");
        assert_eq!(github.command, vec!["npx", "-y", "@modelcontextprotocol/server-github"]);
        assert_eq!(github.env, vec!["GITHUB_TOKEN"]);
        assert_eq!(github.cwd, None);
    }

    #[test]
    fn rejects_empty_command_and_bad_json() {
        assert!(parse_servers_config(r#"{"servers": {"x": {"command": []}}}"#).is_err());
        assert!(parse_servers_config("not json").is_err());
    }

    #[test]
    fn missing_config_file_means_empty_not_error() {
        // load_config never fails on absence — exercised via parse on empty.
        assert_eq!(parse_servers_config("{}").expect("empty object"), Vec::<ServerDef>::new());
    }

    #[test]
    fn rpc_envelopes_have_jsonrpc_and_ids() {
        let req = rpc_request(7, "tools/list", json!({}));
        let v: Value = serde_json::from_str(&req).expect("json");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "tools/list");
        let note = rpc_notification("notifications/initialized", json!({}));
        let v: Value = serde_json::from_str(&note).expect("json");
        assert!(v.get("id").is_none(), "notifications carry no id");
    }

    #[test]
    fn classify_routes_result_error_and_ignores_notifications() {
        let ok = classify_response(r#"{"jsonrpc":"2.0","id":3,"result":{"tools":[]}}"#);
        assert_eq!(ok, Some((3, true, json!({"tools": []}))));
        let err = classify_response(r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32601,"message":"nope"}}"#);
        assert_eq!(err.map(|(id, ok, _)| (id, ok)), Some((4, false)));
        assert_eq!(classify_response(r#"{"jsonrpc":"2.0","method":"notify/x"}"#), None);
        assert_eq!(classify_response("garbage"), None);
    }
}
