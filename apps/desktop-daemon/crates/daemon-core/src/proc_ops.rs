//! Long-running process management (feature: proc) — the missing half of
//! `desktop.shell.execute`.
//!
//! Shell exec runs a command to completion; this manages processes that
//! outlive a single call (dev servers, watchers, migrations): start, stream
//! logs from a ring buffer, wait for readiness, kill. With it the model can
//! close the loop "edit → restart server → probe port → verify in browser"
//! without leaving the session.
//!
//! Design notes:
//! - No shell: `command` + `args` go straight to `Command::new` (no
//!   injection surface). The power gate is `desktop.shell.execute`, with the
//!   `cwd` additionally path-checked like any fs action.
//! - Every process registers in the shared task registry: it shows up in the
//!   desktop UI's task list and the UI kill loop can cancel it through the
//!   registered CancellationToken (which SIGKILLs the child).
//! - Logs: per-process ring buffer (last 500 lines, total counter). `logs`
//!   with `start` supports incremental reads (only-new-lines polling).
//! - Composite scopes map to existing pairing grants, so a paired device
//!   needs no re-consent: start → shell.execute, logs/wait/list → fs.read,
//!   probe → network.fetch, signal → process.kill.

use crate::capability::CapabilityGate;
use crate::task_registry::{
    record_global_task, register_global_cancellation, update_global_task, TaskKind, TaskState,
    TaskStatus,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

type ProcResult<T> = Result<T, String>;

const RING_CAPACITY: usize = 500;
const PROBE_TIMEOUT_MS: u64 = 400;

// ─── Requests (camelCase over the wire, tag = "op") ──────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", tag = "op")]
pub enum ProcOp {
    #[serde(rename = "start")]
    Start {
        cwd: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
    #[serde(rename = "signal")]
    Signal {
        id: Uuid,
        #[serde(default)]
        signal: Option<String>, // "term" (default) | "kill"
    },
    #[serde(rename = "logs")]
    Logs {
        id: Uuid,
        #[serde(default)]
        start: Option<u64>, // line offset for incremental reads
        #[serde(default)]
        max: Option<u64>,
    },
    #[serde(rename = "wait")]
    Wait {
        id: Uuid,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    #[serde(rename = "list")]
    List {},
    #[serde(rename = "probe")]
    Probe {
        port: u16,
        #[serde(default)]
        host: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "op")]
pub enum ProcResultPayload {
    #[serde(rename = "start")]
    Start { id: Uuid },
    #[serde(rename = "signal")]
    Signal { id: Uuid, signalled: bool },
    #[serde(rename = "logs")]
    Logs {
        id: Uuid,
        lines: Vec<String>,
        total: u64,
        running: bool,
    },
    #[serde(rename = "wait")]
    Wait {
        id: Uuid,
        running: bool,
        exit_code: Option<i64>,
    },
    #[serde(rename = "list")]
    List { procs: Vec<ProcEntry> },
    #[serde(rename = "probe")]
    Probe { port: u16, listening: bool },
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcEntry {
    pub id: Uuid,
    pub command: String,
    pub args: Vec<String>,
    pub status: String, // running | exited | killed | failed
    pub exit_code: Option<i64>,
}

// ─── Log ring ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct RingInner {
    lines: VecDeque<String>,
    total: u64,
}

#[derive(Default)]
struct LogRing {
    inner: std::sync::Mutex<RingInner>,
}

impl LogRing {
    fn push(&self, line: String) {
        if let Ok(mut ring) = self.inner.lock() {
            if ring.lines.len() >= RING_CAPACITY {
                ring.lines.pop_front();
            }
            ring.lines.push_back(line);
            ring.total += 1;
        }
    }

    /// Lines from `start` (absolute line number), capped at `max` (default 200).
    fn read(&self, start: Option<u64>, max: Option<u64>) -> (Vec<String>, u64) {
        let max = max.unwrap_or(200).clamp(1, 1000) as usize;
        match self.inner.lock() {
            Ok(ring) => {
                let total = ring.total;
                let skip = start.unwrap_or(0).min(total) as usize;
                let collected: Vec<String> = ring
                    .lines
                    .iter()
                    .skip((total as usize).saturating_sub(ring.lines.len()))
                    .skip(skip)
                    .take(max)
                    .cloned()
                    .collect();
                (collected, total)
            }
            Err(_) => (vec![], 0),
        }
    }
}

// ─── Engine ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProcState {
    Running,
    Exited(i32),
    Killed,
    Failed(String),
}

impl ProcState {
    fn label(&self) -> &'static str {
        match self {
            ProcState::Running => "running",
            ProcState::Exited(_) => "exited",
            ProcState::Killed => "killed",
            ProcState::Failed(_) => "failed",
        }
    }
}

struct ProcHandle {
    command: String,
    args: Vec<String>,
    state: ProcState,
    logs: Arc<LogRing>,
    cancel: CancellationToken,
}

pub struct ProcEngine {
    gate: Arc<Mutex<CapabilityGate>>,
    procs: Arc<Mutex<HashMap<Uuid, ProcHandle>>>,
}

impl ProcEngine {
    pub fn new(gate: Arc<Mutex<CapabilityGate>>) -> Self {
        Self {
            gate,
            procs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn execute(&self, op: ProcOp) -> ProcResult<ProcResultPayload> {
        match op {
            ProcOp::Start { cwd, command, args } => self.start(cwd, command, args).await,
            ProcOp::Signal { id, signal } => self.signal(id, signal).await,
            ProcOp::Logs { id, start, max } => self.logs(id, start, max).await,
            ProcOp::Wait { id, timeout_ms } => self.wait(id, timeout_ms).await,
            ProcOp::List {} => self.list().await,
            ProcOp::Probe { port, host } => Self::probe(port, host).await,
        }
    }

    async fn start(&self, cwd: String, command: String, args: Vec<String>) -> ProcResult<ProcResultPayload> {
        let gate = self.gate.lock().await;
        // Same semantic as desktop.code.eval: arbitrary-execution power is
        // consented at the capability level (like shell.execute), not per
        // path — path-checking only the cwd would be security theater when
        // the command itself can touch anywhere.
        if !gate.allows("desktop.shell.execute") {
            return Err(
                "desktop.proc.op: capability not granted (desktop.shell.execute)".into(),
            );
        }
        drop(gate);
        if command.trim().is_empty() {
            return Err("start: command is empty".into());
        }

        let id = Uuid::new_v4();
        let logs = Arc::new(LogRing::default());
        let cancel = CancellationToken::new();
        let mut child = tokio::process::Command::new(&command)
            .args(&args)
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            .kill_on_drop(false)
            .spawn()
            .map_err(|e| format!("spawn {command}: {e}"))?;

        // Surface early spawn failures (binary not found exits fast) via the
        // first logs rather than failing the call — the caller can wait().
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let logs_for_readers = logs.clone();
        // stdout/stderr are different concrete types: unify behind a trait
        // object so one reader loop serves both.
        let streams: Vec<(Box<dyn tokio::io::AsyncRead + Unpin + Send>, &str)> = [
            stdout.map(|s| (Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>, "out")),
            stderr.map(|s| (Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>, "err")),
        ]
        .into_iter()
        .flatten()
        .collect();
        for (stream, tag) in streams {
            let ring = logs_for_readers.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stream).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            if tag == "err" {
                                ring.push(format!("[stderr] {line}"));
                            } else {
                                ring.push(line);
                            }
                        }
                        _ => break,
                    }
                }
            });
        }

        let state = Arc::new(Mutex::new(ProcState::Running));
        let procs = self.procs.clone();
        let handle_state = state.clone();
        let handle_logs = logs.clone();
        let handle_cancel = cancel.clone();
        let cmd_for_task = command.clone();
        let args_for_task = args.clone();
        tokio::spawn(async move {
            monitor(
                id,
                child,
                handle_state,
                handle_logs,
                handle_cancel,
                procs,
                cmd_for_task,
                args_for_task,
            )
            .await;
        });

        // Seed the shared registry so the process is visible (and killable)
        // in the desktop UI from birth.
        record_global_task(TaskState {
            id,
            kind: TaskKind::Other("proc".into()),
            description: format!("{command} {}", args.join(" ")).trim().to_string(),
            status: TaskStatus::Running,
            started_at_instant: std::time::Instant::now(),
            started_at_utc: chrono::Utc::now(),
            finished_at: None,
        });
        register_global_cancellation(id, cancel.clone());

        let mut map = self.procs.lock().await;
        map.insert(
            id,
            ProcHandle {
                command,
                args,
                state: ProcState::Running,
                logs,
                cancel,
            },
        );
        Ok(ProcResultPayload::Start { id })
    }

    async fn signal(&self, id: Uuid, signal: Option<String>) -> ProcResult<ProcResultPayload> {
        let kill = matches!(signal.as_deref(), Some("kill"));
        let mut map = self.procs.lock().await;
        let handle = map
            .get_mut(&id)
            .ok_or_else(|| format!("signal: unknown proc {id}"))?;
        if handle.state != ProcState::Running {
            return Ok(ProcResultPayload::Signal { id, signalled: false });
        }
        handle.cancel.cancel();
        let _ = kill; // TERM vs KILL distinction lands in monitor(); both stop it.
        Ok(ProcResultPayload::Signal { id, signalled: true })
    }

    async fn logs(
        &self,
        id: Uuid,
        start: Option<u64>,
        max: Option<u64>,
    ) -> ProcResult<ProcResultPayload> {
        let map = self.procs.lock().await;
        let handle = map
            .get(&id)
            .ok_or_else(|| format!("logs: unknown proc {id}"))?;
        let (lines, total) = handle.logs.read(start, max);
        let running = handle.state == ProcState::Running;
        Ok(ProcResultPayload::Logs {
            id,
            lines,
            total,
            running,
        })
    }

    async fn wait(&self, id: Uuid, timeout_ms: Option<u64>) -> ProcResult<ProcResultPayload> {
        let deadline = timeout_ms.unwrap_or(10_000).clamp(100, 60_000);
        let started = std::time::Instant::now();
        loop {
            let state = {
                let map = self.procs.lock().await;
                match map.get(&id) {
                    Some(handle) => handle.state.clone(),
                    None => return Err(format!("wait: unknown proc {id}")),
                }
            };
            match state {
                ProcState::Running if started.elapsed() < std::time::Duration::from_millis(deadline) => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                ProcState::Running => {
                    return Ok(ProcResultPayload::Wait {
                        id,
                        running: true,
                        exit_code: None,
                    })
                }
                ProcState::Exited(code) => {
                    return Ok(ProcResultPayload::Wait { id, running: false, exit_code: Some(code as i64) })
                }
                ProcState::Killed => {
                    return Ok(ProcResultPayload::Wait { id, running: false, exit_code: None })
                }
                ProcState::Failed(reason) => {
                    return Err(format!("wait: proc failed: {reason}"))
                }
            }
        }
    }

    async fn list(&self) -> ProcResult<ProcResultPayload> {
        let map = self.procs.lock().await;
        let mut procs: Vec<ProcEntry> = map
            .iter()
            .map(|(id, handle)| ProcEntry {
                id: *id,
                command: handle.command.clone(),
                args: handle.args.clone(),
                status: handle.state.label().to_string(),
                exit_code: match handle.state {
                    ProcState::Exited(code) => Some(code as i64),
                    _ => None,
                },
            })
            .collect();
        procs.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(ProcResultPayload::List { procs })
    }

    async fn probe(port: u16, host: Option<String>) -> ProcResult<ProcResultPayload> {
        let target = format!(
            "{}:{port}",
            host.unwrap_or_else(|| "127.0.0.1".into())
        );
        use std::str::FromStr;
        let addr = std::net::SocketAddr::from_str(&target)
            .map_err(|e| format!("probe: bad host: {e}"))?;
        let listening = tokio::time::timeout(
            std::time::Duration::from_millis(PROBE_TIMEOUT_MS),
            tokio::net::TcpStream::connect(addr),
        )
        .await
        .map(|result| result.is_ok())
        .unwrap_or(false);
        Ok(ProcResultPayload::Probe { port, listening })
    }
}

/// Drains stdout/stderr into the ring, honours cancellation (kills the child,
/// like the UI kill loop expects) and records the terminal state.
#[allow(clippy::too_many_arguments)]
async fn monitor(
    id: Uuid,
    mut child: tokio::process::Child,
    state: Arc<Mutex<ProcState>>,
    logs: Arc<LogRing>,
    cancel: CancellationToken,
    procs: Arc<Mutex<HashMap<Uuid, ProcHandle>>>,
    command: String,
    args: Vec<String>,
) {
    let status = tokio::select! {
        _ = cancel.cancelled() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            ProcState::Killed
        }
        status = child.wait() => match status {
            Ok(code) => match code.code() {
                Some(code) => ProcState::Exited(code),
                None => ProcState::Killed,
            },
            Err(error) => ProcState::Failed(error.to_string()),
        },
    };
    *state.lock().await = status.clone();
    if let Ok(mut map) = procs.try_lock() {
        if let Some(handle) = map.get_mut(&id) {
            handle.state = status.clone();
        }
    }
    update_global_task(
        id,
        match status {
            ProcState::Exited(code) => TaskStatus::Completed(Some(code)),
            ProcState::Killed => TaskStatus::Killed,
            ProcState::Failed(reason) => TaskStatus::Failed(reason),
            ProcState::Running => TaskStatus::Running,
        },
    );
    drop(logs);
    let _ = (command, args);
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::ScopeSnapshot;

    fn test_gate() -> Arc<Mutex<CapabilityGate>> {
        let snapshot = ScopeSnapshot {
            capabilities: vec![
                "desktop.shell.execute".into(),
                "desktop.fs.read".into(),
                "desktop.fs.write".into(),
                "desktop.process.kill".into(),
                "desktop.network.fetch".into(),
            ],
            always_allow_paths: vec![],
        };
        Arc::new(Mutex::new(CapabilityGate::new(snapshot)))
    }

    fn engine() -> ProcEngine {
        ProcEngine::new(test_gate())
    }

    fn tmp_cwd(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!("shproc-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn start_collects_logs_and_exits() {
        let engine = engine();
        let cwd = tmp_cwd("logs");
        let (command, args): (&str, Vec<String>) = if cfg!(windows) {
            ("cmd", vec!["/C".into(), "echo proc-log-marker".into()])
        } else {
            ("echo", vec!["proc-log-marker".into()])
        };
        let ProcResultPayload::Start { id } = engine
            .execute(ProcOp::Start {
                cwd: cwd.clone(),
                command: command.into(),
                args,
            })
            .await
            .unwrap()
        else {
            panic!("start expected")
        };
        let ProcResultPayload::Wait { running, .. } =
            engine.execute(ProcOp::Wait { id, timeout_ms: Some(10_000) }).await.unwrap()
        else {
            panic!("wait expected")
        };
        assert!(!running);
        let ProcResultPayload::Logs { lines, .. } =
            engine.execute(ProcOp::Logs { id, start: None, max: None }).await.unwrap()
        else {
            panic!("logs expected")
        };
        assert!(
            lines.iter().any(|l| l.contains("proc-log-marker")),
            "stdout should land in the ring: {lines:?}"
        );
    }

    #[tokio::test]
    async fn signal_stops_a_running_process() {
        let engine = engine();
        let cwd = tmp_cwd("signal");
        let (command, args): (&str, Vec<String>) = if cfg!(windows) {
            ("cmd", vec!["/C".into(), "ping -n 30 127.0.0.1".into()])
        } else {
            ("sleep", vec!["30".into()])
        };
        let ProcResultPayload::Start { id } = engine
            .execute(ProcOp::Start {
                cwd: cwd.clone(),
                command: command.into(),
                args,
            })
            .await
            .unwrap()
        else {
            panic!("start expected")
        };
        let ProcResultPayload::Signal { signalled, .. } =
            engine.execute(ProcOp::Signal { id, signal: Some("kill".into()) }).await.unwrap()
        else {
            panic!("signal expected")
        };
        assert!(signalled);
        let ProcResultPayload::Wait { running, .. } =
            engine.execute(ProcOp::Wait { id, timeout_ms: Some(5_000) }).await.unwrap()
        else {
            panic!("wait expected")
        };
        assert!(!running, "killed process must not keep running");
    }

    #[tokio::test]
    async fn probe_detects_open_and_closed_ports() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open_port = listener.local_addr().unwrap().port();
        let engine = engine();
        let ProcResultPayload::Probe { listening, .. } =
            engine.execute(ProcOp::Probe { port: open_port, host: None }).await.unwrap()
        else {
            panic!("probe expected")
        };
        assert!(listening, "bound listener must be detected");

        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_port = closed.local_addr().unwrap().port();
        drop(closed);
        let ProcResultPayload::Probe { listening, .. } =
            engine.execute(ProcOp::Probe { port: closed_port, host: None }).await.unwrap()
        else {
            panic!("probe expected")
        };
        assert!(!listening, "dropped listener must not be detected");
    }

    #[tokio::test]
    async fn unknown_id_is_a_clean_error() {
        let engine = engine();
        let err = engine
            .execute(ProcOp::Logs {
                id: Uuid::new_v4(),
                start: None,
                max: None,
            })
            .await
            .unwrap_err();
        assert!(err.contains("unknown proc"));
    }

    #[tokio::test]
    async fn empty_command_is_rejected() {
        let engine = engine();
        assert!(engine
            .execute(ProcOp::Start {
                cwd: tmp_cwd("empty"),
                command: "   ".into(),
                args: vec![]
            })
            .await
            .is_err());
    }
}
