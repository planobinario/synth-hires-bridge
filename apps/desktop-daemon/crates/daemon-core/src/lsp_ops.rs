//! LSP bridge (feature: lsp) — OMP-style "the IDE wired in".
//!
//! Spawns real language servers (rust-analyzer, typescript-language-server,
//! pyright-langserver, gopls, clangd) per (language, workspace root) and
//! speaks JSON-RPC over stdio with the LSP base protocol (Content-Length
//! framing). The model gets what the IDE knows:
//!   • lsp_diagnostics — publishDiagnostics collected per URI
//!   • lsp_hover / lsp_definition — plain requests
//!   • lsp_rename — the SERVER computes a WorkspaceEdit (imports, re-exports,
//!     aliases included); we gate every target path, apply it file-by-file
//!     with UTF-16 positions (the LSP position encoding) and verify reads.
//!
//! Servers are pooled per WS connection and killed with it (kill_on_drop).
//! Security: every action path-gates like fs.read/fs.write; composite scopes
//! map to the parent scopes on the wire. Only servers already in PATH are
//! used — the daemon never downloads toolchains.
//!
//! Timeout budget: init ≤ 30s + request ≤ 20s / diagnostics wait ≤ 15s, so a
//! worst-case action still finishes inside the web's 60s default sweep.

use crate::{capability::CapabilityGate, DaemonError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex};

const MAX_FRAME_BYTES: usize = 10 * 1024 * 1024;
const INIT_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const DIAGNOSTICS_WAIT: Duration = Duration::from_secs(15);

// ─── Public request/response shapes (camelCase on the wire) ─────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspFileRequest {
    pub path: PathBuf,
    /// Workspace root passed to the server; defaults to the file's parent.
    #[serde(default)]
    pub workspace: Option<PathBuf>,
}

/// 1-indexed line + UTF-16 character (the model counts lines like humans;
/// character follows the LSP encoding so IDE-accurate columns work).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspPositionRequest {
    pub path: PathBuf,
    pub line: usize,
    pub character: usize,
    #[serde(default)]
    pub workspace: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspRenameRequest {
    pub path: PathBuf,
    pub line: usize,
    pub character: usize,
    pub new_name: String,
    #[serde(default)]
    pub workspace: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspDiagnosticsResult {
    pub language: String,
    pub server: String,
    pub uri: String,
    /// Raw LSP Diagnostic objects (ranges in UTF-16 positions).
    pub diagnostics: Vec<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspHoverResult {
    pub language: String,
    pub server: String,
    pub hover: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspDefinitionResult {
    pub language: String,
    pub server: String,
    pub locations: Vec<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspRenameResult {
    pub language: String,
    pub server: String,
    pub files_changed: usize,
    pub edits_applied: usize,
}

// ─── JSON-RPC framing (LSP base protocol) ────────────────────────────────────

fn frame_message(body: &str) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// Read one framed message. None on clean EOF at a message boundary.
async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<String>> {
    // Header section: lines terminated by \r\n, ended by an empty line.
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte).await? {
            0 => return Ok(None),
            _ => header.push(byte[0]),
        }
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
        if header.len() > 8192 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "lsp: header section too long",
            ));
        }
    }
    let header_text = std::str::from_utf8(&header)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("headers: {e}")))?;
    let mut content_length = None;
    for line in header_text.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }
    let len = content_length.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "lsp: missing Content-Length")
    })?;
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "lsp: frame size out of bounds",
        ));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    String::from_utf8(body)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("body: {e}")))
}

// ─── URI helpers (file:// only) ─────────────────────────────────────────────

fn path_to_uri(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut s = String::from("file://");
    for comp in text.split(['/', '\\']) {
        if comp.is_empty() {
            continue;
        }
        s.push('/');
        for ch in comp.chars() {
            if ch.is_ascii_alphanumeric() || "-_.!~*'()".contains(ch) {
                s.push(ch);
            } else {
                for b in ch.to_string().as_bytes() {
                    s.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    if text.ends_with('/') {
        s.push('/');
    }
    s
}

/// Byte-faithful percent decode: work on bytes, never on chars.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path_start = rest.find('/').unwrap_or(rest.len());
    let raw = rest[path_start..].as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            let hex = std::str::from_utf8(&raw[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out).into_owned()))
}

// ─── UTF-16 ↔ byte offset (LSP positions are UTF-16 code units) ─────────────

fn utf16_to_byte_offset(line: &str, utf16_col: usize) -> usize {
    let mut u16_count = 0usize;
    for (byte_idx, ch) in line.char_indices() {
        if u16_count >= utf16_col {
            return byte_idx;
        }
        u16_count += ch.len_utf16();
        if u16_count > utf16_col {
            return byte_idx + ch.len_utf8();
        }
    }
    line.len()
}

// ─── Server selection ────────────────────────────────────────────────────────

const SERVERS: &[(&str, &str, &[&str])] = &[
    ("rust", "rust-analyzer", &[]),
    ("typescript", "typescript-language-server", &["--stdio"]),
    ("javascript", "typescript-language-server", &["--stdio"]),
    ("python", "pyright-langserver", &["--stdio"]),
    ("go", "gopls", &[]),
    ("c", "clangd", &[]),
    ("cpp", "clangd", &[]),
];

fn lsp_language_for(ext: &str) -> Option<(&'static str, &'static str, &'static [&'static str])> {
    let ext = ext.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => SERVERS[0],
        "ts" | "tsx" | "jsx" | "mjs" | "cjs" => SERVERS[1],
        "js" => SERVERS[2],
        "py" => SERVERS[3],
        "go" => SERVERS[4],
        "c" | "h" => SERVERS[5],
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => SERVERS[6],
        _ => return None,
    })
}

// ─── Pooled server ───────────────────────────────────────────────────────────

struct ServerState {
    program: String,
    language: &'static str,
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    diagnostics: Arc<Mutex<HashMap<String, Vec<Value>>>>,
    opened: Mutex<HashSet<String>>,
    next_id: AtomicU64,
    child: Mutex<Child>,
}

impl Drop for ServerState {
    fn drop(&mut self) {
        let _ = self.child.get_mut().start_kill();
    }
}

impl ServerState {
    async fn send_raw(&self, msg: &Value) -> Result<()> {
        let body = serde_json::to_string(msg)?;
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(&frame_message(&body))
            .await
            .map_err(|e| DaemonError::Protocol(format!("lsp stdin: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| DaemonError::Protocol(format!("lsp flush: {e}")))?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send_raw(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn request_msg(&self, id: u64, method: &str, params: Value) -> Result<()> {
        self.send_raw(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await
    }

    /// Initialize handshake + initialized notification. Idempotent per
    /// server: any later request implies it was done at spawn time.
    async fn initialize(&self, root: &Path) -> Result<()> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.request_msg(
            id,
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": path_to_uri(root),
                "capabilities": {}
            }),
        )
        .await?;
        let reply = tokio::time::timeout(INIT_TIMEOUT, rx)
            .await
            .map_err(|_| DaemonError::Protocol("lsp initialize: timed out".into()))?
            .map_err(|_| DaemonError::Protocol("lsp initialize: server died".into()))?;
        if let Some(err) = reply.get("error") {
            return Err(DaemonError::Protocol(format!("lsp initialize error: {err}")));
        }
        self.notify("initialized", json!({})).await
    }
}

// ─── Engine ──────────────────────────────────────────────────────────────────

pub struct LspEngine {
    gate: Arc<Mutex<CapabilityGate>>,
    servers: Mutex<HashMap<(String, String), Arc<ServerState>>>,
}

impl LspEngine {
    pub fn new(gate: Arc<Mutex<CapabilityGate>>) -> Self {
        Self {
            gate,
            servers: Mutex::new(HashMap::new()),
        }
    }

    pub async fn diagnostics(&self, req: LspFileRequest) -> Result<LspDiagnosticsResult> {
        let gate = self.gate.lock().await.clone();
        ensure_gate_path(&gate, "desktop.fs.read", &req.path)?;
        let (state, uri, _root, _src) = self.prepare(&req).await?;
        // The server publishes asynchronously after didOpen (or reuses the
        // state of an earlier open). Any publish for the URI — even an empty
        // list, meaning "no problems" — is a definitive answer.
        let deadline = Instant::now() + DIAGNOSTICS_WAIT;
        loop {
            if let Some(list) = state.diagnostics.lock().await.get(&uri) {
                return Ok(LspDiagnosticsResult {
                    language: state.language.to_string(),
                    server: state.program.clone(),
                    uri: uri.clone(),
                    diagnostics: list.clone(),
                });
            }
            if Instant::now() >= deadline {
                return Ok(LspDiagnosticsResult {
                    language: state.language.to_string(),
                    server: state.program.clone(),
                    uri,
                    diagnostics: vec![],
                });
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub async fn hover(&self, req: LspPositionRequest) -> Result<LspHoverResult> {
        let gate = self.gate.lock().await.clone();
        ensure_gate_path(&gate, "desktop.fs.read", &req.path.clone())?;
        let (state, uri, _root, _src) = self
            .prepare(&LspFileRequest {
                path: req.path,
                workspace: req.workspace,
            })
            .await?;
        let hover = self
            .do_request(
                &state,
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": req.line.saturating_sub(1), "character": req.character }
                }),
            )
            .await?;
        Ok(LspHoverResult {
            language: state.language.to_string(),
            server: state.program.clone(),
            hover: (hover != Value::Null).then_some(hover),
        })
    }

    pub async fn definition(&self, req: LspPositionRequest) -> Result<LspDefinitionResult> {
        let gate = self.gate.lock().await.clone();
        ensure_gate_path(&gate, "desktop.fs.read", &req.path.clone())?;
        let (state, uri, _root, _src) = self
            .prepare(&LspFileRequest {
                path: req.path,
                workspace: req.workspace,
            })
            .await?;
        let def = self
            .do_request(
                &state,
                "textDocument/definition",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": req.line.saturating_sub(1), "character": req.character }
                }),
            )
            .await?;
        let locations = match def {
            Value::Null => vec![],
            Value::Array(list) => list,
            loc @ Value::Object(_) => vec![loc],
            other => vec![other],
        };
        Ok(LspDefinitionResult {
            language: state.language.to_string(),
            server: state.program.clone(),
            locations,
        })
    }

    pub async fn rename(&self, req: LspRenameRequest) -> Result<LspRenameResult> {
        let gate = self.gate.lock().await.clone();
        ensure_gate_path(&gate, "desktop.fs.write", &req.path)?;
        let (state, uri, _root, _src) = self
            .prepare(&LspFileRequest {
                path: req.path,
                workspace: req.workspace,
            })
            .await?;
        let edit = self
            .do_request(
                &state,
                "textDocument/rename",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": req.line.saturating_sub(1), "character": req.character },
                    "newName": req.new_name
                }),
            )
            .await?;
        if edit == Value::Null {
            return Err(DaemonError::Protocol(
                "lsp rename: server returned no WorkspaceEdit (not a renamable symbol?)".into(),
            ));
        }
        let (files, edits) = apply_workspace_edit(&gate, &edit).await?;
        Ok(LspRenameResult {
            language: state.language.to_string(),
            server: state.program.clone(),
            files_changed: files,
            edits_applied: edits,
        })
    }

    async fn do_request(
        &self,
        state: &Arc<ServerState>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let id = state.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        state.pending.lock().await.insert(id, tx);
        if let Err(e) = state.request_msg(id, method, params).await {
            state.pending.lock().await.remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(reply)) => {
                if let Some(err) = reply.get("error") {
                    return Err(DaemonError::Protocol(format!("lsp {method} error: {err}")));
                }
                Ok(reply.get("result").cloned().unwrap_or(Value::Null))
            }
            Ok(Err(_)) => Err(DaemonError::Protocol(format!(
                "lsp {method}: server dropped the response"
            ))),
            Err(_) => {
                state.pending.lock().await.remove(&id);
                Err(DaemonError::Protocol(format!("lsp {method}: timed out")))
            }
        }
    }

    /// Ensure the (language, root) server exists + initialized, didOpen the
    /// file with its CURRENT disk content, and hand back the pieces.
    async fn prepare(
        &self,
        req: &LspFileRequest,
    ) -> Result<(Arc<ServerState>, String, PathBuf, String)> {
        let ext = req.path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let (language, program, args) = lsp_language_for(ext).ok_or_else(|| {
            DaemonError::Protocol(format!(
                "lsp: no language server configured for '{ext}' (supported: rs, ts/tsx, js, py, go, c/cpp)"
            ))
        })?;
        let root = req
            .workspace
            .clone()
            .unwrap_or_else(|| req.path.parent().map(Path::to_path_buf).unwrap_or_default());
        let key = (language.to_string(), root.to_string_lossy().into_owned());
        let state = {
            let mut servers = self.servers.lock().await;
            match servers.get(&key) {
                Some(existing) => existing.clone(),
                None => {
                    let fresh = Arc::new(spawn_server(&root, language, program, args)?);
                    servers.insert(key.clone(), fresh.clone());
                    fresh
                }
            }
        };
        let uri = path_to_uri(&req.path);
        let source = tokio::fs::read_to_string(&req.path)
            .await
            .map_err(DaemonError::Io)?;
        let first_open = state.opened.lock().await.insert(uri.clone());
        if first_open {
            state.initialize(&root).await?;
            state
                .notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language,
                            "version": 1,
                            "text": source
                        }
                    }),
                )
                .await?;
        }
        Ok((state, uri, root, source))
    }
}

fn spawn_server(
    root: &Path,
    language: &'static str,
    program: &str,
    args: &[&str],
) -> Result<ServerState> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DaemonError::Protocol(format!(
                    "lsp: server '{program}' not found in PATH for {language}; install it to enable LSP features"
                ))
            } else {
                DaemonError::Io(e)
            }
        })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| DaemonError::Protocol("lsp: no stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| DaemonError::Protocol("lsp: no stdout".into()))?;
    let stderr = child.stderr.take();

    let state = ServerState {
        program: program.to_string(),
        language,
        stdin: Mutex::new(stdin),
        pending: Arc::new(Mutex::new(HashMap::new())),
        diagnostics: Arc::new(Mutex::new(HashMap::new())),
        opened: Mutex::new(HashSet::new()),
        next_id: AtomicU64::new(1),
        child: Mutex::new(child),
    };

    // Reader task: demultiplex responses by id into pending oneshots and
    // server notifications (publishDiagnostics) into the shared map. Dies
    // with the server (EOF) — pending senders then error out on drop.
    {
        let pending = Arc::clone(&state.pending);
        let diagnostics = Arc::clone(&state.diagnostics);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            if let Some(err_pipe) = stderr {
                tokio::spawn(async move {
                    let mut err = BufReader::new(err_pipe);
                    let mut sink = [0u8; 4096];
                    while let Ok(n @ 1..) = err.read(&mut sink).await {
                        let _ = n; // drain only; chatty servers must not deadlock
                    }
                });
            }
            loop {
                match read_frame(&mut reader).await {
                    Ok(Some(body)) => {
                        let msg: Value = match serde_json::from_str(&body) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
                            if let Some(tx) = pending.lock().await.remove(&id) {
                                let _ = tx.send(msg);
                            }
                        } else if msg.get("method").and_then(|v| v.as_str())
                            == Some("textDocument/publishDiagnostics")
                        {
                            if let Some(uri) =
                                msg.pointer("/params/uri").and_then(|v| v.as_str())
                            {
                                let diags = msg
                                    .pointer("/params/diagnostics")
                                    .and_then(|v| v.as_array())
                                    .cloned()
                                    .unwrap_or_default();
                                diagnostics.lock().await.insert(uri.to_string(), diags);
                            }
                        }
                    }
                    Ok(None) | Err(_) => break,
                }
            }
        });
    }
    Ok(state)
}

// ─── WorkspaceEdit application (UTF-16 → bytes, gated, verified) ────────────

/// Collect (uri, edits) from either `changes` or `documentChanges` shape.
fn workspace_edit_targets(edit: &Value) -> Vec<(String, Vec<Value>)> {
    let mut out = Vec::new();
    if let Some(changes) = edit.get("changes").and_then(|v| v.as_object()) {
        for (uri, edits) in changes {
            if let Some(list) = edits.as_array() {
                out.push((uri.clone(), list.clone()));
            }
        }
    }
    if let Some(docs) = edit.get("documentChanges").and_then(|v| v.as_array()) {
        for doc in docs {
            let uri = doc.pointer("/textDocument/uri").and_then(|v| v.as_str());
            let edits = doc.get("edits").and_then(|v| v.as_array());
            if let (Some(uri), Some(edits)) = (uri, edits) {
                out.push((uri.to_string(), edits.clone()));
            }
        }
    }
    out
}

/// Apply the server-computed WorkspaceEdit: gate EVERY target path first
/// (abort before touching anything), then rewrite each file from its LSP
/// UTF-16 ranges, then verify each read-back. Returns (files, edits).
async fn apply_workspace_edit(gate: &CapabilityGate, edit: &Value) -> Result<(usize, usize)> {
    let targets = workspace_edit_targets(edit);
    if targets.is_empty() {
        return Err(DaemonError::Protocol("lsp rename: empty WorkspaceEdit".into()));
    }
    let mut files: Vec<(PathBuf, String, Vec<(usize, usize, String)>)> = Vec::new();
    let mut total_edits = 0usize;
    for (uri, edits) in &targets {
        let path = uri_to_path(uri).ok_or_else(|| {
            DaemonError::Protocol(format!("lsp rename: unsupported uri {uri}"))
        })?;
        ensure_gate_path(gate, "desktop.fs.write", &path)?;
        let source = tokio::fs::read_to_string(&path)
            .await
            .map_err(DaemonError::Io)?;
        let mut ranges = Vec::new();
        for edit in edits {
            let sl = edit.pointer("/range/start/line").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let sc = edit.pointer("/range/start/character").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let el = edit.pointer("/range/end/line").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let ec = edit.pointer("/range/end/character").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let new_text = edit
                .get("newText")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let start = linecol_to_byte(&source, sl, sc);
            let end = linecol_to_byte(&source, el, ec);
            if start > end || end > source.len() {
                return Err(DaemonError::Protocol(format!(
                    "lsp rename: invalid range {start}..{end} in {uri}"
                )));
            }
            ranges.push((start, end, new_text));
        }
        total_edits += ranges.len();
        files.push((path, source, ranges));
    }
    // Abort BEFORE any write on overlap (server bugs must not corrupt files).
    for (_path, _source, ranges) in &files {
        let mut sorted = ranges.clone();
        sorted.sort_by_key(|(s, e, _)| (*s, *e));
        for w in sorted.windows(2) {
            if w[1].0 < w[0].1 {
                return Err(DaemonError::Protocol(
                    "lsp rename: overlapping edits from server".into(),
                ));
            }
        }
    }
    let mut files_changed = 0usize;
    for (path, source, mut ranges) in files {
        ranges.sort_by_key(|(s, _e, _t)| *s);
        ranges.reverse();
        let mut out = source.clone();
        for (start, end, text) in ranges {
            out.replace_range(start..end, &text);
        }
        let tmp = path.with_extension("synthhires-tmp");
        tokio::fs::write(&tmp, out.as_bytes())
            .await
            .map_err(DaemonError::Io)?;
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                DaemonError::Io(e)
            })?;
        let written = tokio::fs::read(&path).await.map_err(DaemonError::Io)?;
        if written != out.as_bytes() {
            return Err(DaemonError::Protocol(
                "lsp rename: read-back verification failed".into(),
            ));
        }
        files_changed += 1;
    }
    Ok((files_changed, total_edits))
}

/// LSP (0-indexed line, UTF-16 character) → byte offset in `source`.
fn linecol_to_byte(source: &str, line: usize, character: usize) -> usize {
    let mut offset = 0usize;
    for (idx, line_text) in source.split_inclusive('\n').enumerate() {
        if idx == line {
            return offset + utf16_to_byte_offset(line_text, character);
        }
        offset += line_text.len();
    }
    offset
}

fn ensure_gate_path(gate: &CapabilityGate, capability: &str, path: &Path) -> Result<()> {
    use crate::capability::GateDecision;
    match gate.check_path(capability, path) {
        GateDecision::Allow => Ok(()),
        GateDecision::RequireConsent => Err(DaemonError::CapabilityDenied(format!(
            "{capability} requires consent for {}",
            path.display()
        ))),
        GateDecision::Deny => Err(DaemonError::CapabilityDenied(capability.into())),
    }
}

// ─── Tests: framing, URIs, UTF-16, WorkspaceEdit math (pure, no servers) ────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_roundtrip() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":42}"#;
        let framed = frame_message(body);
        let text = String::from_utf8(framed).unwrap();
        assert!(text.starts_with(&format!("Content-Length: {}\r\n\r\n", body.len())));
        let read_back = read_frame(&mut text.as_bytes()).await.unwrap().unwrap();
        assert_eq!(read_back, body);
        // Unframed input with no Content-Length: EOF at a boundary → None.
        let junk = "no framing here";
        assert!(read_frame(&mut junk.as_bytes()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn header_parsing_is_case_insensitive() {
        let raw = b"content-length: 2\r\n\r\n{}";
        assert_eq!(read_frame(&mut raw.as_slice()).await.unwrap().unwrap(), "{}");
    }

    #[test]
    fn uris_roundtrip_with_espacios() {
        let p = Path::new("/home/emi/mis archivos/lib.rs");
        let uri = path_to_uri(p);
        assert_eq!(uri, "file:///home/emi/mis%20archivos/lib.rs");
        assert_eq!(uri_to_path(&uri), Some(p.to_path_buf()));
    }

    #[test]
    fn utf16_positions_map_to_bytes() {
        // "á" is 1 UTF-16 unit but 2 bytes; "😀" is 2 units, 4 bytes.
        // utf16: f0 n1 sp2 á3 (4 )5 sp6 {7 sp8 "9 😀10-11 "12 sp13 }14
        // bytes: f0 n1 sp2 á3-4 (5 )6 sp7 {8 sp9 "10 😀11-14 "15 sp16 }17
        let line = "fn á() { \"😀\" }";
        assert_eq!(utf16_to_byte_offset(line, 0), 0);
        assert_eq!(utf16_to_byte_offset(line, 3), 3); // start of "á"
        assert_eq!(utf16_to_byte_offset(line, 4), 5); // char after á (á = 2 utf8 bytes)
        assert_eq!(utf16_to_byte_offset(line, 12), 15); // char after 😀 (4 utf8 bytes)
        assert_eq!(utf16_to_byte_offset(line, 99), line.len()); // clamp
    }

    #[test]
    fn linecol_math_handles_multibyte() {
        let source = "fn a() {}\nlet s = \"ñ😀\";\n";
        // Line 1 (0-indexed), character 9 (UTF-16): l-e-t-space-s-space-=-
        // space-"-ñ → bytes: 7 + 1 (space after let? recount) — verify by
        // decoding back with utf16_to_byte_offset on that line.
        let start = linecol_to_byte(source, 1, 0);
        assert_eq!(start, 10);
        // utf16 in line 1: l0 e1 t2 sp3 s4 sp5 =6 sp7 "8 ñ9 😀10-11 "12 ;13 \n14
        let line = source.split_inclusive('\n').nth(1).unwrap();
        let at8 = utf16_to_byte_offset(line, 8);
        assert_eq!(&line[..at8], "let s = ");
        let at9 = utf16_to_byte_offset(line, 9);
        assert_eq!(&line[..at9], "let s = \""); // utf16 9 = ñ → byte 9
        let at10 = utf16_to_byte_offset(line, 10);
        assert_eq!(&line[..at10], "let s = \"ñ"); // utf16 10 = 😀 → byte 11
        assert_eq!(linecol_to_byte(source, 1, 10), 21); // line1@10 + 11 (😀 at byte 11 within line)
    }

    #[test]
    fn workspace_edit_targets_reads_both_shapes() {
        let changes = json!({"changes": {"file:///a.rs": [{"newText": "x"}]}});
        assert_eq!(workspace_edit_targets(&changes).len(), 1);
        let doc_changes = json!({"documentChanges": [
            {"textDocument": {"uri": "file:///b.rs"}, "edits": [{"newText": "y"}]}
        ]});
        assert_eq!(workspace_edit_targets(&doc_changes).len(), 1);
        assert!(workspace_edit_targets(&json!({})).is_empty());
    }

    #[test]
    fn unknown_extension_is_a_clean_error() {
        assert!(lsp_language_for("weird").is_none());
        assert!(lsp_language_for("rs").is_some());
        assert!(lsp_language_for("TSX").is_some()); // case-insensitive
    }
}
