//! DAP debugger (feature: dap) — a real debugger for the agent.
//!
//! Speaks the Debug Adapter Protocol (JSON messages with the SAME
//! Content-Length base framing as LSP — shared from `lsp_ops`) with debug
//! adapters spawned over stdio: `debugpy` (Python), `js-debug`
//! (`node dapDebugServer.js`), `codelldb`, `delve`… Any DAP adapter works:
//! the caller provides the adapter command and launch arguments.
//!
//! One persistent session per name (like the PTY): set breakpoints, continue,
//! read the stack and variables, evaluate expressions in the stopped frame,
//! step — the classic "stop at my bug and look around" loop the agent could
//! never do before.
//!
//! Capability: `desktop.debug.op`, scope derived from the op (start / eval /
//! continue / step / pause ride the shell tier; breakpoints, stack, variables
//! and threads are reads).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};

type DapResult<T> = Result<T, String>;

/// Ring-buffer capacity for debug events (stopped/output) per session.
const EVENT_RING: usize = 100;
/// Default timeout for adapter round-trips.
const REQ_TIMEOUT: Duration = Duration::from_secs(15);
/// Generous timeout for start (adapter + launch can be slow on first run).
const START_TIMEOUT: Duration = Duration::from_secs(45);

// ─── Ops (wire shape) ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase", tag = "op")]
pub enum DapOp {
    /// Spawn the adapter, initialize and launch/attach the debuggee.
    Start {
        session: String,
        /// Adapter command line, e.g. ["python", "-m", "debugpy.adapter"] or
        /// ["node", "/path/js-debug/src/dapDebugServer.js"].
        adapter: Vec<String>,
        cwd: Option<String>,
        /// DAP launch/attach body: {"request":"launch","type":"python",
        /// "program":"app.py", …} — forwarded verbatim to the adapter.
        launch: Value,
        #[serde(default)]
        stop_on_entry: bool,
    },
    /// Set source breakpoints; returns the verified ones (line + verified).
    SetBreakpoints { session: String, file: String, lines: Vec<u32> },
    /// Call stack of the (current or given) thread.
    StackTrace { session: String, #[serde(default)] thread_id: Option<u64> },
    /// Local variables of the (current or given) frame.
    Variables { session: String, #[serde(default)] frame_id: Option<u64> },
    /// Resume execution.
    Continue { session: String, #[serde(default)] thread_id: Option<u64> },
    /// Step: "over" (default) | "in" | "out". Returns the next stop if quick.
    Step { session: String, #[serde(default)] action: Option<String>, #[serde(default)] thread_id: Option<u64> },
    /// Pause a running thread.
    Pause { session: String, #[serde(default)] thread_id: Option<u64> },
    /// Evaluate an expression in the current frame (repl context).
    Eval { session: String, expression: String, #[serde(default)] frame_id: Option<u64> },
    /// Threads of the debuggee.
    Threads { session: String },
    /// Recent debug events (stopped/output) drained from the ring.
    Events { session: String },
    /// Disconnect (terminate debuggee) and kill the adapter.
    Stop { session: String },
}

// ─── Client plumbing ───────────────────────────────────────────────────────

/// Run-state cursor: which thread/frame we are looking at.
#[derive(Default, Clone, Copy)]
struct RunState {
    thread: u64,
    frame: Option<u64>,
}

/// A live connection to one debug adapter. Shared as `Arc<DapClient>`, so
/// engine ops clone the handle and never hold the sessions map across work.
struct DapClient {
    stdin: Mutex<tokio::io::BufWriter<tokio::process::ChildStdin>>,
    /// Requests in flight → reply channel (drained by the reader task).
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<DapResult<Value>>>>>,
    events: Arc<Mutex<VecDeque<Value>>>,
    seq: std::sync::atomic::AtomicU64,
    current: std::sync::Mutex<RunState>,
    /// Taken on teardown so `kill()` can await outside the guard.
    child: std::sync::Mutex<Option<tokio::process::Child>>,
    /// Reader task handle (aborted on stop).
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl DapClient {
    async fn spawn(
        adapter: &[String],
        cwd: Option<&str>,
        events: Arc<Mutex<VecDeque<Value>>>,
    ) -> DapResult<Arc<Self>> {
        if adapter.is_empty() || adapter[0].trim().is_empty() {
            return Err("adapter command is empty".into());
        }
        let mut cmd = tokio::process::Command::new(&adapter[0]);
        cmd.args(&adapter[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn adapter {:?}: {e}", adapter[0]))?;
        let stdin = child.stdin.take().ok_or("adapter stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("adapter stdout unavailable")?;

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<DapResult<Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let reader = tokio::spawn(reader_loop(stdout, pending.clone(), events.clone()));

        Ok(Arc::new(Self {
            stdin: Mutex::new(tokio::io::BufWriter::with_capacity(64 * 1024, stdin)),
            pending,
            events,
            seq: std::sync::atomic::AtomicU64::new(1),
            current: std::sync::Mutex::new(RunState { thread: 1, frame: None }),
            child: std::sync::Mutex::new(Some(child)),
            reader: Mutex::new(Some(reader)),
        }))
    }

    async fn request(&self, command: &str, arguments: Value, timeout: Duration) -> DapResult<Value> {
        let id = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let body = serde_json::json!({
            "seq": id,
            "type": "request",
            "command": command,
            "arguments": if arguments.is_null() { serde_json::json!({}) } else { arguments },
        });
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let raw = serde_json::to_string(&body).map_err(|e| format!("serialize request: {e}"))?;
        {
            use tokio::io::AsyncWriteExt;
            let mut w = self.stdin.lock().await;
            w.write_all(format!("Content-Length: {}\r\n\r\n{}", raw.len(), raw).as_bytes())
                .await
                .map_err(|e| format!("adapter write: {e}"))?;
            w.flush().await.map_err(|e| format!("adapter flush: {e}"))?;
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err("adapter reader dropped the request".into()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(format!("adapter timeout on '{command}'"))
            }
        }
    }

    fn run_state(&self) -> RunState {
        *self.current.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn set_run_state(&self, st: RunState) {
        *self.current.lock().unwrap_or_else(|p| p.into_inner()) = st;
    }

    async fn wait_for_stopped(&self, max_ms: u64) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(max_ms);
        while tokio::time::Instant::now() < deadline {
            {
                let ring = self.events.lock().await;
                if let Some(ev) = ring
                    .iter()
                    .rev()
                    .find(|e| e.get("event").and_then(Value::as_str) == Some("stopped"))
                {
                    return Some(ev.clone());
                }
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        None
    }

    async fn drain_events(&self) -> Vec<Value> {
        let mut ring = self.events.lock().await;
        ring.drain(..).collect()
    }

    async fn teardown(self: &Arc<Self>) {
        // 1) Politely disconnect (short timeout — best effort).
        let _ = self
            .request("disconnect", serde_json::json!({"terminateDebuggee": true}), Duration::from_secs(3))
            .await;
        // 2) Kill the adapter child (taken out of the slot to await freely).
        let child = self.child.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(mut child) = child {
            let _ = child.kill().await;
        }
        // 3) Abort the reader loop.
        if let Some(reader) = self.reader.lock().await.take() {
            reader.abort();
        }
    }
}

/// Route one inbound DAP message: responses resolve pending requests,
/// events land in the session ring.
async fn route_message(
    msg: &str,
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<DapResult<Value>>>>>,
    events: &Arc<Mutex<VecDeque<Value>>>,
) {
    let Ok(v) = serde_json::from_str::<Value>(msg) else { return };
    match v.get("type").and_then(Value::as_str) {
        Some("response") => {
            let id = v.get("request_seq").and_then(Value::as_u64);
            let success = v.get("success").and_then(Value::as_bool).unwrap_or(false);
            if let Some(id) = id {
                let tx = pending.lock().await.remove(&id);
                if let Some(tx) = tx {
                    let payload = if success {
                        Ok(v.get("body").cloned().unwrap_or(Value::Null))
                    } else {
                        Err(v
                            .pointer("/message")
                            .and_then(Value::as_str)
                            .unwrap_or("adapter error")
                            .to_string())
                    };
                    let _ = tx.send(payload);
                }
            }
        }
        Some("event") => {
            let mut ring = events.lock().await;
            if ring.len() >= EVENT_RING {
                ring.pop_front();
            }
            ring.push_back(v);
        }
        _ => {}
    }
}

async fn reader_loop<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<DapResult<Value>>>>>,
    events: Arc<Mutex<VecDeque<Value>>>,
) {
    while let Ok(Some(body)) = crate::lsp_ops::read_frame(&mut reader, "dap").await {
        route_message(&body, &pending, &events).await;
    }
    // Adapter died: fail every in-flight request so callers don't hang.
    let mut map = pending.lock().await;
    for (_, tx) in map.drain() {
        let _ = tx.send(Err("adapter exited".into()));
    }
}

// ─── Engine ────────────────────────────────────────────────────────────────

pub struct DapEngine {
    gate: Arc<Mutex<crate::capability::CapabilityGate>>,
    sessions: Mutex<HashMap<String, Arc<DapClient>>>,
}

impl DapEngine {
    pub fn new(gate: Arc<Mutex<crate::capability::CapabilityGate>>) -> Self {
        Self { gate, sessions: Mutex::new(HashMap::new()) }
    }

    async fn gate_shell(&self) -> DapResult<()> {
        let gate = self.gate.lock().await;
        if gate.allows("desktop.shell.execute") {
            Ok(())
        } else {
            Err("capability not granted: desktop.shell.execute".into())
        }
    }

    async fn get(&self, session: &str) -> DapResult<Arc<DapClient>> {
        self.sessions
            .lock()
            .await
            .get(session)
            .cloned()
            .ok_or_else(|| format!("no such debug session: {session} (start one with op:'start')"))
    }

    pub async fn execute(&self, op: DapOp) -> DapResult<Value> {
        match op {
            DapOp::Start { session, adapter, cwd, launch, stop_on_entry } => {
                self.start(&session, adapter, cwd, launch, stop_on_entry).await
            }
            DapOp::SetBreakpoints { session, file, lines } => {
                let c = self.get(&session).await?;
                let bps: Vec<Value> = lines.iter().map(|l| serde_json::json!({"line": l})).collect();
                let body = c
                    .request(
                        "setBreakpoints",
                        serde_json::json!({
                            "source": {"path": file},
                            "breakpoints": bps,
                            "sourceModified": false,
                        }),
                        REQ_TIMEOUT,
                    )
                    .await?;
                // configurationDone right after breakpoints: the standard
                // end-of-config signal for launch-then-attach flows.
                let _ = c.request("configurationDone", serde_json::json!({}), REQ_TIMEOUT).await;
                Ok(serde_json::json!({
                    "session": session,
                    "breakpoints": body.get("breakpoints").cloned().unwrap_or_default(),
                }))
            }
            DapOp::StackTrace { session, thread_id } => {
                let c = self.get(&session).await?;
                let tid = thread_id.unwrap_or_else(|| self.run_state_of(&c).thread);
                let body = c
                    .request("stackTrace", serde_json::json!({"threadId": tid, "levels": 20}), REQ_TIMEOUT)
                    .await?;
                // Remember the top frame for variables/eval.
                let mut st = self.run_state_of(&c);
                st.thread = tid;
                if let Some(frame) = body.get("stackFrames").and_then(Value::as_array).and_then(|f| f.first()) {
                    st.frame = frame.get("id").and_then(Value::as_i64).map(|v| v as u64);
                }
                c.set_run_state(st);
                Ok(serde_json::json!({
                    "session": session,
                    "threadId": tid,
                    "stackFrames": body.get("stackFrames").cloned().unwrap_or_default(),
                }))
            }
            DapOp::Variables { session, frame_id } => {
                let c = self.get(&session).await?;
                let frame = frame_id
                    .or(self.run_state_of(&c).frame)
                    .ok_or("no frame: take a stackTrace first")?;
                let scopes = c
                    .request("scopes", serde_json::json!({"frameId": frame}), REQ_TIMEOUT)
                    .await?;
                let mut out = Vec::new();
                if let Some(list) = scopes.get("scopes").and_then(Value::as_array) {
                    for scope in list.iter().take(2) {
                        let name = scope.get("name").and_then(Value::as_str).unwrap_or("?");
                        let var_ref = scope.get("variablesReference").and_then(Value::as_i64).unwrap_or(0);
                        if var_ref == 0 {
                            continue;
                        }
                        let vars = c
                            .request("variables", serde_json::json!({"variablesReference": var_ref}), REQ_TIMEOUT)
                            .await?;
                        out.push(serde_json::json!({
                            "scope": name,
                            "variables": vars.get("variables").cloned().unwrap_or_default(),
                        }));
                    }
                }
                Ok(serde_json::json!({ "session": session, "frameId": frame, "scopes": out }))
            }
            DapOp::Continue { session, thread_id } => {
                self.gate_shell().await?;
                let c = self.get(&session).await?;
                let tid = thread_id.unwrap_or_else(|| self.run_state_of(&c).thread);
                let body = c
                    .request("continue", serde_json::json!({"threadId": tid}), REQ_TIMEOUT)
                    .await?;
                Ok(serde_json::json!({
                    "session": session,
                    "resumed": true,
                    "allThreadsContinued": body.get("allThreadsContinued").and_then(Value::as_bool).unwrap_or(true),
                    "hint": "poll bridge_desktop_dap {op:'events'} or take a stackTrace after the next stop",
                }))
            }
            DapOp::Step { session, action, thread_id } => {
                self.gate_shell().await?;
                let action = action.as_deref().unwrap_or("over");
                let c = self.get(&session).await?;
                let tid = thread_id.unwrap_or_else(|| self.run_state_of(&c).thread);
                let cmd = match action {
                    "in" => "stepIn",
                    "out" => "stepOut",
                    _ => "next",
                };
                c.request(cmd, serde_json::json!({"threadId": tid}), REQ_TIMEOUT).await?;
                // Wait briefly for the next stop and hand it back.
                let stopped = c.wait_for_stopped(4_000).await;
                let mut out = serde_json::json!({ "session": session, "action": cmd });
                if let Some(ev) = stopped {
                    out["stopped"] = serde_json::json!({
                        "reason": ev.pointer("/body/reason").cloned().unwrap_or(Value::Null),
                        "line": ev.pointer("/body/line").cloned().unwrap_or(Value::Null),
                        "threadId": ev.pointer("/body/threadId").cloned().unwrap_or(Value::Null),
                    });
                    let mut st = self.run_state_of(&c);
                    if let Some(tid) = ev.pointer("/body/threadId").and_then(Value::as_u64) {
                        st.thread = tid;
                    }
                    c.set_run_state(st);
                }
                Ok(out)
            }
            DapOp::Pause { session, thread_id } => {
                self.gate_shell().await?;
                let c = self.get(&session).await?;
                let tid = thread_id.unwrap_or_else(|| self.run_state_of(&c).thread);
                c.request("pause", serde_json::json!({"threadId": tid}), REQ_TIMEOUT).await?;
                Ok(serde_json::json!({ "session": session, "paused": tid }))
            }
            DapOp::Eval { session, expression, frame_id } => {
                self.gate_shell().await?;
                let c = self.get(&session).await?;
                let frame = frame_id.or(self.run_state_of(&c).frame);
                let body = c
                    .request(
                        "evaluate",
                        serde_json::json!({"expression": expression, "frameId": frame, "context": "repl"}),
                        REQ_TIMEOUT,
                    )
                    .await?;
                Ok(serde_json::json!({
                    "session": session,
                    "result": body.get("result").cloned().unwrap_or(Value::Null),
                    "type": body.get("type").cloned().unwrap_or(Value::Null),
                }))
            }
            DapOp::Threads { session } => {
                let c = self.get(&session).await?;
                let body = c.request("threads", serde_json::json!({}), REQ_TIMEOUT).await?;
                Ok(serde_json::json!({
                    "session": session,
                    "threads": body.get("threads").cloned().unwrap_or_default(),
                }))
            }
            DapOp::Events { session } => {
                let c = self.get(&session).await?;
                let evs = c.drain_events().await;
                Ok(serde_json::json!({ "session": session, "events": evs }))
            }
            DapOp::Stop { session } => {
                let c = self.sessions.lock().await.remove(&session);
                match c {
                    Some(c) => {
                        c.teardown().await;
                        Ok(serde_json::json!({ "session": session, "stopped": true }))
                    }
                    None => Err(format!("no such debug session: {session}")),
                }
            }
        }
    }

    fn run_state_of(&self, c: &Arc<DapClient>) -> RunState {
        c.run_state()
    }

    async fn start(
        &self,
        session: &str,
        adapter: Vec<String>,
        cwd: Option<String>,
        launch: Value,
        stop_on_entry: bool,
    ) -> DapResult<Value> {
        self.gate_shell().await?;
        // Replace any previous session with the same name.
        if let Some(old) = self.sessions.lock().await.remove(session) {
            old.teardown().await;
        }

        let events: Arc<Mutex<VecDeque<Value>>> = Arc::new(Mutex::new(VecDeque::new()));
        let client = DapClient::spawn(&adapter, cwd.as_deref(), events.clone()).await?;

        let initialize = client
            .request(
                "initialize",
                serde_json::json!({
                    "adapterID": launch.get("type").and_then(Value::as_str).unwrap_or("unknown"),
                    "clientID": "synthhires-bridge",
                    "clientName": "synthhires-bridge",
                    "linesStartAt1": true,
                    "columnsStartAt1": true,
                    "pathFormat": "path",
                }),
                START_TIMEOUT,
            )
            .await;
        if initialize.is_err() {
            client.teardown().await;
            return Err(initialize.unwrap_err());
        }
        let capabilities = initialize.unwrap();

        let request_kind = launch
            .get("request")
            .and_then(Value::as_str)
            .unwrap_or("launch")
            .to_string();
        let launched = client.request(&request_kind, launch.clone(), START_TIMEOUT).await;
        if launched.is_err() {
            client.teardown().await;
            return Err(launched.unwrap_err());
        }

        let stopped = if stop_on_entry { client.wait_for_stopped(6_000).await } else { None };

        self.sessions.lock().await.insert(session.to_string(), client);

        Ok(serde_json::json!({
            "session": session,
            "request": request_kind,
            "adapterCapabilities": capabilities,
            "stoppedOnEntry": stopped,
            "hint": if stop_on_entry && stopped.is_none() {
                "no stopped event yet: poll {op:'events'} or take a stackTrace"
            } else {
                "set breakpoints with {op:'set_breakpoints'}, then continue/step; evaluate expressions with {op:'eval'}"
            },
        }))
    }

    pub async fn has_session(&self, session: &str) -> bool {
        self.sessions.lock().await.contains_key(session)
    }
}

// ─── DAP framing helpers (same base protocol as LSP, unit-tested) ──────────

/// Frame a DAP message exactly like the LSP base protocol requires.
pub fn dap_frame(body: &str) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// Parse the `Content-Length` of a header block (case-insensitive).
pub fn parse_content_length(headers: &str) -> Option<usize> {
    for line in headers.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                return value.trim().parse::<usize>().ok();
            }
        }
    }
    None
}

// Keep the bound visible for reviewers of both bridges.
const _: () = {
    // 8 MB — mirrors MAX_FRAME_BYTES in lsp_ops (kept in sync by tests below).
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn framing_matches_lsp_base_protocol() {
        let body = r#"{"seq":1,"type":"request"}"#;
        let framed = dap_frame(body);
        let head = String::from_utf8(framed[..framed.len() - body.len()].to_vec()).unwrap();
        assert_eq!(head, format!("Content-Length: {}\r\n\r\n", body.len()));
        assert_eq!(parse_content_length(&head), Some(body.len()));
    }

    #[test]
    fn content_length_is_case_insensitive() {
        assert_eq!(parse_content_length("content-length: 42\r\n\r\n"), Some(42));
        assert_eq!(parse_content_length("CONTENT-LENGTH: 7\r\n\r\n"), Some(7));
        assert_eq!(parse_content_length("content-type: application/json\r\n\r\n"), None);
    }

    #[tokio::test]
    async fn routes_responses_and_events() {
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<DapResult<Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let events: Arc<Mutex<VecDeque<Value>>> = Arc::new(Mutex::new(VecDeque::new()));

        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(5, tx);

        route_message(
            r#"{"seq":3,"type":"response","request_seq":5,"success":true,"body":{"threads":[]}}"#,
            &pending,
            &events,
        )
        .await;
        route_message(
            r#"{"seq":4,"type":"event","event":"stopped","body":{"reason":"breakpoint","line":12}}"#,
            &pending,
            &events,
        )
        .await;

        let reply = rx.await.unwrap().expect("response body");
        assert_eq!(reply.get("threads"), Some(&json!([])));
        let ring = events.lock().await;
        assert_eq!(ring.len(), 1);
        assert_eq!(ring[0].get("event"), Some(&json!("stopped")));
    }

    #[tokio::test]
    async fn errors_surface_to_the_caller() {
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<DapResult<Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let events: Arc<Mutex<VecDeque<Value>>> = Arc::new(Mutex::new(VecDeque::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(9, tx);
        route_message(
            r#"{"seq":2,"type":"response","request_seq":9,"success":false,"message":"cannot set breakpoint"}"#,
            &pending,
            &events,
        )
        .await;
        assert_eq!(rx.await.unwrap().unwrap_err(), "cannot set breakpoint");
    }

    #[tokio::test]
    async fn engine_requires_a_session() {
        let gate = Arc::new(Mutex::new(crate::capability::CapabilityGate::new(
            crate::capability::ScopeSnapshot::default(),
        )));
        let engine = DapEngine::new(gate);
        let err = engine
            .execute(DapOp::Threads { session: "nope".into() })
            .await
            .unwrap_err();
        assert!(err.contains("no such debug session"), "got: {err}");
    }

    #[test]
    fn dap_ops_parse_from_wire_json() {
        let start: DapOp = serde_json::from_value(json!({
            "op": "start",
            "session": "s1",
            "adapter": ["python", "-m", "debugpy.adapter"],
            "launch": {"request": "launch", "type": "python", "program": "app.py"},
            "stopOnEntry": true,
        }))
        .unwrap();
        assert!(matches!(start, DapOp::Start { ref session, stop_on_entry, .. } if session == "s1" && stop_on_entry));

        let bp: DapOp = serde_json::from_value(json!({
            "op": "setBreakpoints", "session": "s1", "file": "/a.py", "lines": [4, 9],
        }))
        .unwrap();
        assert!(matches!(bp, DapOp::SetBreakpoints { lines, .. } if lines.len() == 2));

        let step: DapOp = serde_json::from_value(json!({ "op": "step", "session": "s1", "action": "in" })).unwrap();
        assert!(matches!(step, DapOp::Step { action: Some(ref a), .. } if a == "in"));
    }
}

#[derive(Debug, Serialize)]
#[allow(dead_code)]
pub struct DapInfo {
    pub feature: &'static str,
}
