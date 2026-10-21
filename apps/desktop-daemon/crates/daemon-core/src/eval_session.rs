//! Persistent JavaScript evaluation (feature: eval) — OMP-style `eval`.
//!
//! A long-lived JS runtime process (Bun if present, otherwise Node — both are
//! universal on dev machines) speaks a one-JSON-object-per-line protocol over
//! stdio. Each call runs inside a `vm.createContext` session whose globals
//! (`console`, `require`, user-defined vars) SURVIVE between calls, so the
//! model can build state incrementally instead of re-bootstrapping a script
//! every turn.
//!
//! Console frames are tagged with the SESSION that produced them, not only
//! with the request that happened to be in flight: a session's console
//! closure outlives any single request, so request-id routing silently
//! discarded every log line after the call that created the session. Frames
//! for a session that is not the one executing (fire-and-forget async work
//! landing during another session's round-trip, or output written while
//! idle) are queued per session and delivered on that session's next call.
//!
//! Threat model (honest): arbitrary JS with `require` is shell-equivalent
//! power. The action is therefore gated by the `desktop.shell.execute` scope
//! (the consent dialog the user already answers for terminal commands) and
//! additionally negotiated behind the `eval` protocol feature. `vm` timeouts
//! catch runaway synchronous code; the Rust-side deadline KILLS the runtime
//! process for async hangs — destroying every session with it. That is a
//! feature: a wedged context must never survive silently.
//!
//! Engine lifecycle: one engine per daemon WS connection (lazy-init). A
//! reconnect = fresh runtime = clean sessions. Loopback tool re-entry
//! (JS calling agent tools) is a follow-up slice (F1c.2).

use crate::{capability::CapabilityGate, DaemonError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

/// Hard deadline per exec call. Must exceed the runtime's own async-result
/// cap (15s) and the vm sync timeout (20s) so those report a precise error
/// first; a Rust-side hit means the runtime is wedged and gets killed.
const EXEC_TIMEOUT: Duration = Duration::from_secs(30);
/// Sessions idle longer than this are reaped (context dropped in runtime).
const SESSION_TTL: Duration = Duration::from_secs(600);
const MAX_CODE_BYTES: usize = 1_000_000;
const MAX_CAPTURED_CHARS: usize = 200_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalRequest {
    pub code: String,
    /// Opaque session key. Absent → a new session is created and its id is
    /// returned; the web reuses it on follow-up calls. Unknown ids (after a
    /// timeout kill) simply start fresh.
    #[serde(default)]
    pub session: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalResult {
    pub session_id: String,
    pub created_session: bool,
    /// Captured console output for this session. Includes lines this session
    /// produced earlier while idle (queued); never another session's —
    /// console frames route by session, not by request id.
    pub stdout: String,
    pub stderr: String,
    /// The completion value of the last expression, JSON-serialized
    /// (promises are awaited by the runtime, up to 15s).
    pub result: Option<serde_json::Value>,
}
/// A JS runtime child process with its stdio pipes.
struct Runtime {
    proc: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
}

/// Console output that arrived for a session other than the one executing
/// (or while no exec was in flight). Held until that session's next call so
/// its console stream never silently loses lines.
#[derive(Debug, Default)]
struct PendingOut {
    stdout: String,
    stderr: String,
}

struct EngineInner {
    gate: Arc<Mutex<CapabilityGate>>,
    runtime: Option<Runtime>,
    sessions: HashMap<String, Instant>,
    pending: HashMap<String, PendingOut>,
}

/// Shared eval engine: spawns the runtime lazily and owns all sessions.
pub struct EvalEngine {
    inner: Arc<Mutex<EngineInner>>,
}

impl EvalEngine {
    pub fn new(gate: Arc<Mutex<CapabilityGate>>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(EngineInner {
                gate,
                runtime: None,
                sessions: HashMap::new(),
                pending: HashMap::new(),
            })),
        }
    }

    /// Execute `code` in the (possibly new) session. Holds the engine lock
    /// for the whole round-trip: eval calls are serialized by design.
    pub async fn exec(&self, req: EvalRequest) -> Result<EvalResult> {
        if req.code.len() > MAX_CODE_BYTES {
            return Err(DaemonError::Protocol(format!(
                "eval: code too large ({} > {} bytes)",
                req.code.len(),
                MAX_CODE_BYTES
            )));
        }
        let mut inner = self.inner.lock().await;
        if !inner.gate.lock().await.allows("desktop.shell.execute") {
            return Err(DaemonError::CapabilityDenied(
                "desktop.code.eval requires the shell.execute scope".into(),
            ));
        }
        // Reaper: drop idle sessions (and tell the runtime to free them).
        let expired: Vec<String> = inner
            .sessions
            .iter()
            .filter(|(_, t)| t.elapsed() > SESSION_TTL)
            .map(|(k, _)| k.clone())
            .collect();
        for id in expired {
            inner.sessions.remove(&id);
            inner.pending.remove(&id);
            let _ = Self::send_request(&mut inner, &serde_json::json!({
                "id": uuid::Uuid::new_v4().to_string(),
                "op": "destroy",
                "session": id,
            }))
            .await;
        }

        let session_id = req
            .session
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let request_id = uuid::Uuid::new_v4().to_string();
        Self::send_request(
            &mut inner,
            &serde_json::json!({
                "id": request_id,
                "op": "exec",
                "session": session_id,
                "code": req.code,
            }),
        )
        .await?;

        let deadline = Instant::now() + EXEC_TIMEOUT;
        // Output queued while this session was idle (logs from fire-and-forget
        // async work, or lines that hit the pipe between calls) belongs to THIS
        // session's stream: seed the buffers before reading the new frames.
        let mut queued = inner.pending.remove(&session_id).unwrap_or_default();
        let mut stdout = std::mem::take(&mut queued.stdout);
        let mut stderr = std::mem::take(&mut queued.stderr);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // Kill the whole runtime: async hangs can't be interrupted
                // any other way, and a wedged process is untrustworthy.
                Self::kill_runtime(&mut inner);
                inner.sessions.clear();
                inner.pending.clear();
                return Err(DaemonError::Protocol(
                    "eval timeout: runtime killed and all sessions destroyed".into(),
                ));
            }
            let mut line = String::new();
            let read = tokio::time::timeout(
                remaining,
                async {
                    let rt = inner.runtime.as_mut().expect("runtime alive");
                    rt.reader.read_line(&mut line).await
                },
            )
            .await;
            let line = match read {
                Ok(Ok(0)) | Err(_) => {
                    // EOF before a line: runtime died (or timeout raced the
                    // read after the deadline branch — treated the same).
                    Self::kill_runtime(&mut inner);
                    inner.sessions.clear();
                    inner.pending.clear();
                    return Err(DaemonError::Protocol(
                        "eval: runtime exited unexpectedly; sessions destroyed".into(),
                    ));
                }
                Ok(Err(e)) => {
                    Self::kill_runtime(&mut inner);
                    inner.sessions.clear();
                    inner.pending.clear();
                    return Err(DaemonError::Protocol(format!(
                        "eval: runtime read error ({e}); sessions destroyed"
                    )));
                }
                Ok(Ok(_)) => line,
            };
            let msg: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let msg_id = msg.get("id").and_then(|v| v.as_str());
            let msg_session = msg.get("session").and_then(|v| v.as_str());
            let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match msg_type {
                // Console frames route by SESSION: the console closure of a
                // session outlives the request that created it, so request-id
                // filtering discarded every log line from call 2 onward (id
                // mismatch → skip). The request id is only a fallback for
                // frames from an older runtime that lacks the session tag.
                "stdout" | "stderr" => {
                    let data = msg.get("data");
                    let mine = msg_session == Some(session_id.as_str())
                        || (msg_session.is_none() && msg_id == Some(request_id.as_str()));
                    if mine {
                        if msg_type == "stdout" {
                            append_capped(&mut stdout, data);
                        } else {
                            append_capped(&mut stderr, data);
                        }
                    } else if let Some(other) = msg_session {
                        // Another session's straggler: queue it for that
                        // session's next call. Never delivered to us, never
                        // dropped while the session is alive, never leaked
                        // into a session that has since been reaped.
                        if inner.sessions.contains_key(other) {
                            let slot = inner.pending.entry(other.to_string()).or_default();
                            if msg_type == "stdout" {
                                append_capped(&mut slot.stdout, data);
                            } else {
                                append_capped(&mut slot.stderr, data);
                            }
                        }
                    }
                }
                "done" => {
                    if msg_id != Some(request_id.as_str()) {
                        // e.g. the reaper's fire-and-forget `destroy` reply.
                        continue;
                    }
                    let created =
                        msg.get("createdSession").and_then(|v| v.as_bool()).unwrap_or(false);
                    inner.sessions.insert(session_id.clone(), Instant::now());
                    return Ok(EvalResult {
                        session_id,
                        created_session: created,
                        stdout,
                        stderr,
                        result: msg.get("result").cloned().filter(|v| !v.is_null()),
                    });
                }
                "error" => {
                    if msg_id != Some(request_id.as_str()) {
                        continue;
                    }
                    let created =
                        msg.get("createdSession").and_then(|v| v.as_bool()).unwrap_or(false);
                    inner.sessions.insert(session_id.clone(), Instant::now());
                    let err = msg
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown eval error")
                        .to_string();
                    return Err(DaemonError::EvalFailed {
                        session_id,
                        created_session: created,
                        message: err,
                        stdout,
                        stderr,
                    });
                }
                _ => continue,
            }
        }
    }

    async fn send_request(inner: &mut EngineInner, value: &serde_json::Value) -> Result<()> {
        if inner.runtime.is_none() {
            inner.runtime = Some(spawn_runtime()?);
        }
        let runtime = inner.runtime.as_mut().expect("runtime just ensured");
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        runtime
            .stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| DaemonError::Protocol(format!("eval runtime stdin: {e}")))?;
        runtime
            .stdin
            .flush()
            .await
            .map_err(|e| DaemonError::Protocol(format!("eval runtime flush: {e}")))?;
        Ok(())
    }

    fn kill_runtime(inner: &mut EngineInner) {
        if let Some(mut rt) = inner.runtime.take() {
            let _ = rt.proc.start_kill();
        }
    }
}

fn append_capped(buf: &mut String, data: Option<&serde_json::Value>) {
    if let Some(s) = data.and_then(|v| v.as_str()) {
        if buf.len() < MAX_CAPTURED_CHARS {
            buf.push_str(s);
            buf.push('\n');
        } else if !buf.ends_with("…\n") {
            buf.push_str("…\n");
        }
    }
}

/// Spawn the first available runtime (`bun` preferred, `node` as universal
/// fallback). The runtime source is a bundled const written to the temp dir,
/// so no `-e` quoting quirks across shells/runtimes.
fn spawn_runtime() -> Result<Runtime> {
    // Unique file per spawn: two engines starting concurrently (tests, or a
    // reconnect racing a shutdown) must never rewrite the script another
    // runtime is about to load.
    let path = std::env::temp_dir().join(format!(
        "synthhires-eval-runtime-{}.cjs",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&path, RUNTIME_JS)
        .map_err(|e| DaemonError::Protocol(format!("eval runtime write: {e}")))?;
    for program in ["bun", "node"] {
        match tokio::process::Command::new(program)
            .arg(&path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(mut child) => {
                let stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| DaemonError::Protocol("eval: no stdin".into()))?;
                let stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| DaemonError::Protocol("eval: no stdout".into()))?;
                if let Some(mut err_pipe) = child.stderr.take() {
                    // Drain the runtime's own stderr so a chatty runtime can
                    // never fill the pipe and deadlock itself.
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(&mut err_pipe);
                        let mut line = String::new();
                        loop {
                            line.clear();
                            match reader.read_line(&mut line).await {
                                Ok(0) | Err(_) => break,
                                Ok(_) => tracing::debug!(target: "eval_runtime", "{}", line.trim_end()),
                            }
                        }
                    });
                }
                return Ok(Runtime {
                    proc: child,
                    stdin,
                    reader: BufReader::new(stdout),
                });
            }
            // Binary not installed → try the next runtime.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(DaemonError::Io(e)),
        }
    }
    Err(DaemonError::Protocol(
        "eval: no JS runtime found (install bun or node)".into(),
    ))
}

const RUNTIME_JS: &str = r#"// synthhires eval runtime (CommonJS) — protocol: one JSON object per line on stdio.
const readline = require("readline");
const vm = require("vm");

const sessions = new Map();
const ASYNC_CAP_MS = 15000;
const VM_TIMEOUT_MS = 20000;

function send(obj) { try { process.stdout.write(JSON.stringify(obj) + "\n"); } catch {} }
function fmt(x) { if (typeof x === "string") return x; try { return JSON.stringify(x); } catch { return String(x); } }
function serialize(v) {
  if (v === undefined) return null;
  try { return JSON.parse(JSON.stringify(v)); } catch { return String(v); }
}

// Session id → { ctx, currentId }. `currentId` is the request in flight for
// this session; console frames carry the session tag (the routing key on the
// Rust side) plus that id for debuggability. The console closure is created
// once per session and must NOT be bound to the creating request's id —
// doing so made the daemon drop every log line after the first call.
function sessionConsole(sid) {
  const emit = (type) => (...a) => {
    const s = sessions.get(sid);
    send({ id: s && s.currentId ? s.currentId : null, session: sid, type, data: a.map(fmt).join(" ") });
  };
  return { log: emit("stdout"), info: emit("stdout"), warn: emit("stderr"), error: emit("stderr") };
}

const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
rl.on("line", (line) => {
  if (!line.trim()) return;
  let req;
  try { req = JSON.parse(line); } catch { send({ id: "?", type: "error", error: "bad json line" }); return; }
  const { id, op, session, code } = req;
  if (!id) return;
  if (op === "ping") { send({ id, type: "done", result: null }); return; }
  if (op === "destroy") { sessions.delete(session); send({ id, type: "done", result: null }); return; }
  if (op !== "exec") { send({ id, type: "error", error: "unknown op: " + op }); return; }

  let entry = sessions.get(session);
  const createdSession = !entry;
  if (!entry) {
    // Timer/microtask globals are injected so session code can schedule work
    // without `require("timers")` (host globals, same trust level as the
    // already-injected `require` — the whole action is gated as shell).
    entry = {
      ctx: vm.createContext({
        console: sessionConsole(session),
        require,
        setTimeout,
        clearTimeout,
        setInterval,
        clearInterval,
        queueMicrotask,
      }),
      currentId: null,
    };
    sessions.set(session, entry);
  }
  entry.currentId = id;
  let value;
  try {
    value = vm.runInContext(code, entry.ctx, { timeout: VM_TIMEOUT_MS, displayErrors: true });
  } catch (e) {
    send({ id, session, type: "error", error: String((e && e.stack) || e), createdSession });
    return;
  }
  const cap = new Promise((_, rej) => setTimeout(() => rej(new Error("async result exceeded " + ASYNC_CAP_MS + "ms")), ASYNC_CAP_MS));
  Promise.race([Promise.resolve(value), cap]).then(
    (v) => send({ id, session, type: "done", result: serialize(v), createdSession }),
    (e) => send({ id, session, type: "error", error: String((e && e.stack) || e), createdSession }),
  );
});
rl.on("close", () => process.exit(0));
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{CapabilityGate, ScopeSnapshot};

    fn gate_with(shell: bool) -> Arc<Mutex<CapabilityGate>> {
        let snap = ScopeSnapshot {
            capabilities: if shell {
                vec!["desktop.shell.execute".into()]
            } else {
                vec![]
            },
            always_allow_paths: vec![],
            one_shot_paths: vec![],
        };
        Arc::new(Mutex::new(CapabilityGate::new(snap)))
    }

    async fn have_runtime() -> bool {
        // The test box must have bun or node; CI runners do.
        for p in ["bun", "node"] {
            if tokio::process::Command::new(p)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await
                .is_ok()
            {
                return true;
            }
        }
        false
    }

    #[tokio::test]
    async fn state_persists_across_calls() {
        if !have_runtime().await {
            eprintln!("skipping: no JS runtime");
            return;
        }
        let engine = EvalEngine::new(gate_with(true));
        let first = engine
            .exec(EvalRequest {
                code: "globalThis.counter = 41; counter + 1".into(),
                session: None,
            })
            .await
            .unwrap();
        assert!(first.created_session);
        assert_eq!(first.result, Some(serde_json::json!(42)));

        // Same session: the variable set above is still alive (and still 41 —
        // the completion value was 42, but the assignment stored 41).
        let second = engine
            .exec(EvalRequest {
                code: "counter * 2".into(),
                session: Some(first.session_id.clone()),
            })
            .await
            .unwrap();
        assert!(!second.created_session);
        assert_eq!(second.result, Some(serde_json::json!(82)));
        assert_eq!(second.session_id, first.session_id);
    }

    #[tokio::test]
    async fn console_and_errors_are_captured() {
        if !have_runtime().await {
            eprintln!("skipping: no JS runtime");
            return;
        }
        let engine = EvalEngine::new(gate_with(true));
        let res = engine
            .exec(EvalRequest {
                code: "console.log('hola'); console.error('boom'); undefined".into(),
                session: None,
            })
            .await;
        // undefined completion value → serialized as null → result None,
        // but the console capture is the payload we care about.
        match res {
            Ok(r) => {
                assert!(r.stdout.contains("hola"), "stdout={:?}", r.stdout);
                assert!(r.stderr.contains("boom"), "stderr={:?}", r.stderr);
            }
            Err(DaemonError::EvalFailed { stdout, stderr, .. }) => {
                assert!(stdout.contains("hola"));
                assert!(stderr.contains("boom"));
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn syntax_error_is_a_clean_failure() {
        if !have_runtime().await {
            eprintln!("skipping: no JS runtime");
            return;
        }
        let engine = EvalEngine::new(gate_with(true));
        let res = engine
            .exec(EvalRequest {
                code: "this is not js (((".into(),
                session: None,
            })
            .await;
        match res {
            Err(DaemonError::EvalFailed { message, .. }) => {
                assert!(!message.is_empty());
            }
            other => panic!("expected EvalFailed, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn gate_denies_without_shell_scope() {
        let engine = EvalEngine::new(gate_with(false));
        let res = engine
            .exec(EvalRequest { code: "1".into(), session: None })
            .await;
        assert!(matches!(res, Err(DaemonError::CapabilityDenied(_))));
    }

    /// Regression: the session's console closure outlives the request that
    /// created it. Frames were tagged with THAT request's id, so the reader
    /// (which routes by request id) discarded every log line from call 2 on —
    /// stdout came back empty while the feature docs claimed "console
    /// captured". Console frames now route by session.
    #[tokio::test]
    async fn console_on_later_session_calls_is_captured() {
        if !have_runtime().await {
            eprintln!("skipping: no JS runtime");
            return;
        }
        let engine = EvalEngine::new(gate_with(true));
        let first = engine
            .exec(EvalRequest {
                code: "console.log('first call'); globalThis.counter = 1; 1".into(),
                session: None,
            })
            .await
            .unwrap();
        assert!(first.created_session);
        assert!(first.stdout.contains("first call"), "stdout={:?}", first.stdout);

        let second = engine
            .exec(EvalRequest {
                code: "console.log('second call'); counter + 1".into(),
                session: Some(first.session_id.clone()),
            })
            .await
            .unwrap();
        assert_eq!(second.result, Some(serde_json::json!(2)));
        assert!(
            second.stdout.contains("second call"),
            "log from the second call of a live session was dropped: stdout={:?}",
            second.stdout
        );

        // stderr rides the same session-tagged routing.
        let third = engine
            .exec(EvalRequest {
                code: "console.error('late stderr'); undefined".into(),
                session: Some(first.session_id.clone()),
            })
            .await;
        match third {
            Ok(r) => assert!(r.stderr.contains("late stderr"), "stderr={:?}", r.stderr),
            Err(DaemonError::EvalFailed { stderr, .. }) => {
                assert!(stderr.contains("late stderr"), "stderr={:?}", stderr)
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    /// Fire-and-forget async output must reach the session that produced it,
    /// even when it lands while ANOTHER session is executing (queued) or
    /// while no exec is in flight (sits in the pipe, read on next call).
    #[tokio::test]
    async fn idle_or_cross_session_output_is_queued_not_stolen_or_lost() {
        if !have_runtime().await {
            eprintln!("skipping: no JS runtime");
            return;
        }
        let engine = EvalEngine::new(gate_with(true));
        let a = engine
            .exec(EvalRequest {
                code: "(async () => { await new Promise(r => setTimeout(r, 50)); console.log('late from A'); })(); 'scheduled'"
                    .into(),
                session: None,
            })
            .await
            .unwrap();
        assert_eq!(a.result, Some(serde_json::json!("scheduled")));

        // B executes while A's timer is (or has been) firing: the frame is
        // tagged session=A and must not pollute B's stream.
        let b = engine
            .exec(EvalRequest { code: "'bee'".into(), session: None })
            .await
            .unwrap();
        assert_eq!(b.result, Some(serde_json::json!("bee")));
        assert!(
            !b.stdout.contains("late from A"),
            "another session's output leaked into this one: stdout={:?}",
            b.stdout
        );

        // Let the timer fire for sure (it may already have, during B or idle).
        tokio::time::sleep(Duration::from_millis(300)).await;

        let a2 = engine
            .exec(EvalRequest {
                code: "'again'".into(),
                session: Some(a.session_id.clone()),
            })
            .await
            .unwrap();
        assert_eq!(a2.result, Some(serde_json::json!("again")));
        assert!(
            a2.stdout.contains("late from A"),
            "queued idle output never reached its session: stdout={:?}",
            a2.stdout
        );
    }

    /// Timer globals are part of the session context now (previously the
    /// context only had console+require, so plain `setTimeout` was a
    /// ReferenceError and even the runtime's own async patterns needed
    /// require("timers")).
    #[tokio::test]
    async fn timer_globals_exist_in_the_session_context() {
        if !have_runtime().await {
            eprintln!("skipping: no JS runtime");
            return;
        }
        let engine = EvalEngine::new(gate_with(true));
        let res = engine
            .exec(EvalRequest {
                code: "typeof setTimeout + '/' + typeof clearTimeout + '/' + typeof queueMicrotask"
                    .into(),
                session: None,
            })
            .await
            .unwrap();
        assert_eq!(
            res.result,
            Some(serde_json::json!("function/function/function")),
            "result={:?}",
            res.result
        );
    }
}
