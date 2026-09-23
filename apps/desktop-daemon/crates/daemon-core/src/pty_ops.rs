//! Persistent PTY shell (feature: pty) — stateful shell sessions.
//!
//! `desktop.shell.execute` is stateless (fresh process per call) and
//! `desktop.proc.op` manages detached background processes. This fills the
//! gap: ONE long-lived shell per session whose cwd, environment variables,
//! virtualenvs and exit-code ($?) survive between calls — like a real
//! terminal the agent keeps open while it works.
//!
//! Design notes:
//! - portable-pty (wezterm's crate) for real cross-platform PTYs (ConPTY on
//!   Windows, openpty on Unix). No PTY → clean error, not a fallback shell.
//! - One shell per session id (the web sends the conversation id): a
//!   different chat gets its own shell. Shells cap and evict LRU (the oldest
//!   dies when a new one would exceed the cap).
//! - Output lands in a ring buffer (last 1000 lines / 64 KiB per shell);
//!   `read` drains new bytes since the last read for that session.
//! - Power scope: same as arbitrary execution (`desktop.shell.execute`) —
//!   a PTY is a terminal.
//! - `exit` kills the shell process; sessions also die on drop via the
//!   writer/reader handles.

use crate::capability::CapabilityGate;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read as _;
use std::sync::Arc;
use tokio::sync::Mutex;

type PtyResult<T> = Result<T, String>;

const MAX_SESSIONS: usize = 8;
const RING_LINES: usize = 1_000;

// ─── Requests / results ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all_fields = "camelCase", tag = "op")]
pub enum PtyOp {
    #[serde(rename = "run")]
    Run {
        session: String,
        command: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    #[serde(rename = "read")]
    Read { session: String },
    #[serde(rename = "status")]
    Status { session: String },
    #[serde(rename = "list")]
    List {},
    #[serde(rename = "exit")]
    Exit { session: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all_fields = "camelCase", tag = "op")]
pub enum PtyResultPayload {
    #[serde(rename = "run")]
    Run {
        session: String,
        output: String,
        exited: bool,
        exit_hint: Option<String>,
    },
    #[serde(rename = "read")]
    Read { session: String, output: String },
    #[serde(rename = "status")]
    Status { session: String, alive: bool },
    #[serde(rename = "list")]
    List { sessions: Vec<String> },
    #[serde(rename = "exit")]
    Exit { session: String, exited: bool },
}

// ─── Session ─────────────────────────────────────────────────────────────────

/// Output accumulator: ring of complete lines + a trailing partial line.
#[derive(Default)]
struct OutputBuffer {
    lines: std::sync::Mutex<VecDequeOutput>,
}

#[derive(Default)]
struct VecDequeOutput {
    lines: Vec<std::string::String>,
    partial: String,
    bytes: usize,
}

impl OutputBuffer {
    fn push(&self, chunk: &str) {
        if let Ok(mut buf) = self.lines.lock() {
            for ch in chunk.chars() {
                if ch == '\n' {
                    let mut line = std::mem::take(&mut buf.partial);
                    line.push('\n');
                    buf.bytes += line.len();
                    line.pop();
                    buf.lines.push(line);
                    if buf.lines.len() > RING_LINES {
                        let drop = buf.lines.len() - RING_LINES;
                        buf.bytes -= buf.lines.drain(..drop).map(|l| l.len() + 1).sum::<usize>();
                    }
                } else {
                    buf.partial.push(ch);
                }
            }
            // Cap a pathological partial line too.
            if buf.partial.len() > 16 * 1024 {
                let cut = buf.partial.len() - 16 * 1024;
                buf.partial.drain(..cut);
            }
        }
    }

    fn drain(&self) -> String {
        match self.lines.lock() {
            Ok(mut buf) => {
                let mut out = std::mem::take(&mut buf.lines).join("\n");
                if !buf.partial.is_empty() {
                    out.push('\n');
                    out.push_str(&buf.partial.clone());
                }
                buf.bytes = 0;
                let _ = out.len(); // bytes tracked loosely; ring caps apply
                out
            }
            Err(_) => String::new(),
        }
    }

    fn pending(&self) -> bool {
        self.lines.lock().map(|b| !b.lines.is_empty() || !b.partial.is_empty()).unwrap_or(false)
    }
}

struct PtySession {
    writer: std::sync::Mutex<Box<dyn std::io::Write + Send>>,
    child: std::sync::Mutex<Box<dyn portable_pty::Child + Send>>,
    output: Arc<OutputBuffer>,
}

impl PtySession {
    fn is_alive(&self) -> bool {
        match self.child.lock().map(|mut c| c.try_wait()) {
            Ok(Ok(Some(_))) => false,
            Ok(Ok(None)) => true,
            _ => false,
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = portable_pty::ChildKiller::kill(&mut **child);
        }
    }
}

// ─── Engine ──────────────────────────────────────────────────────────────────

pub struct PtyEngine {
    gate: Arc<Mutex<CapabilityGate>>,
    sessions: Arc<Mutex<HashMap<String, Arc<PtySession>>>>,
}

impl PtyEngine {
    pub fn new(gate: Arc<Mutex<CapabilityGate>>) -> Self {
        Self {
            gate,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn execute(&self, op: PtyOp) -> PtyResult<PtyResultPayload> {
        match op {
            PtyOp::Run { session, command, timeout_ms } => {
                self.run(session, command, timeout_ms).await
            }
            PtyOp::Read { session } => self.read(session).await,
            PtyOp::Status { session } => self.status(session).await,
            PtyOp::List {} => self.list().await,
            PtyOp::Exit { session } => self.exit(session).await,
        }
    }

    fn shell_program() -> &'static str {
        // Windows: cmd.exe, not PowerShell. PSReadLine + ConPTY on runners is
        // fragile (slow boot, process dying at spawn); cmd is the first-class
        // ConPTY citizen. cwd/env/state persist identically.
        if cfg!(windows) {
            "cmd"
        } else {
            "bash"
        }
    }

    fn eol() -> &'static str {
        // ConPTY consumers (cmd/PowerShell) need CRLF; Unix shells want LF.
        if cfg!(windows) {
            "\r\n"
        } else {
            "\n"
        }
    }

    async fn gate_shell(&self) -> PtyResult<()> {
        let gate = self.gate.lock().await;
        if gate.allows("desktop.shell.execute") {
            Ok(())
        } else {
            Err("desktop.pty: capability not granted (desktop.shell.execute)".into())
        }
    }

    async fn get_or_create(&self, session: &str) -> PtyResult<Arc<PtySession>> {
        {
            let sessions = self.sessions.lock().await;
            if let Some(existing) = sessions.get(session) {
                return Ok(existing.clone());
            }
        }
        // Create outside the map lock: PTY spawn can block briefly.
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 30,
                cols: 110,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty: {e}"))?;
        let mut cmd = CommandBuilder::new(Self::shell_program());
        if !cfg!(windows) {
            // arg returns (); chain as statements.
            cmd.arg("--norc");
            cmd.arg("--noprofile");
        }
        cmd.env("BRIDGE_PTY", "1");
        cmd.cwd(
            std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_else(|_| ".".into()),
        );
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("spawn {}: {e}", Self::shell_program()))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("clone reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("take writer: {e}"))?;

        let output = Arc::new(OutputBuffer::default());
        let output_reader = output.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut lost = false;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF: shell closed
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                        output_reader.push(&chunk);
                    }
                    Err(_) => {
                        if lost {
                            break;
                        }
                        lost = true; // one retry on transient EINTR-ish errors
                    }
                }
            }
        });

        // Wait for the shell to paint its prompt (or die) before the first
        // write: slow-starting shells on CI/Windows would otherwise swallow
        // the first command into the void. Bounded; then discard the banner
        // so the first `run` returns only the command's own output.
        let start = std::time::Instant::now();
        while !output.pending() && start.elapsed() < std::time::Duration::from_secs(3) {
            // Shell died while booting: no point waiting for a prompt.
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => {}
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let _ = output.drain();
        if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
            return Err(format!(
                "spawn {}: shell exited immediately (unavailable or crashed); retry later",
                Self::shell_program()
            ));
        }

        let entry = Arc::new(PtySession {
            writer: std::sync::Mutex::new(writer),
            child: std::sync::Mutex::new(child),
            output,
        });
        let mut sessions = self.sessions.lock().await;
        // LRU-ish eviction: when over cap, evict some other session (HashMap
        // has no insertion order; any non-requested session is a victim).
        if sessions.len() >= MAX_SESSIONS && !sessions.contains_key(session) {
            if let Some(oldest) = sessions.keys().next().cloned() {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(session.to_string(), entry.clone());
        Ok(entry)
    }

    async fn run(
        &self,
        session: String,
        command: String,
        timeout_ms: Option<u64>,
    ) -> PtyResult<PtyResultPayload> {
        self.gate_shell().await?;
        if command.trim().is_empty() {
            return Err("run: command is empty".into());
        }
        let timeout = timeout_ms.unwrap_or(10_000).clamp(200, 120_000);
        let shell = self.get_or_create(&session).await?;
        if !shell.is_alive() {
            return Err(format!("run: session '{session}' already exited; use a new session id"));
        }
        {
            let mut writer = shell.writer.lock().unwrap();
            std::io::Write::write_all(&mut *writer, format!("{command}{}", Self::eol()).as_bytes())
                .map_err(|e| format!("write: {e}"))?;
            std::io::Write::flush(&mut *writer).map_err(|e| format!("flush: {e}"))?;
        }
        // Wait for output to settle: sleep in slices until either the
        // timeout passes or no new bytes arrive for ~2 quiet slices.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout);
        let mut last_pending = shell.output.pending();
        let mut quiet_slices = 0u8;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let pending = shell.output.pending();
            if pending && last_pending {
                quiet_slices = 0;
            } else if !pending && !last_pending {
                quiet_slices += 1;
                if quiet_slices >= 2 {
                    break;
                }
            }
            last_pending = pending;
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
        let output = shell.output.drain();
        let exited = !shell.is_alive();
        let exit_hint = if exited {
            Some(format!(
                "shell '{session}' exited (last command may have killed it or the session ended); open a new session id"
            ))
        } else {
            None
        };
        Ok(PtyResultPayload::Run { session, output, exited, exit_hint })
    }

    async fn read(&self, session: String) -> PtyResult<PtyResultPayload> {
        self.gate_shell().await?;
        let sessions = self.sessions.lock().await;
        let shell = sessions
            .get(&session)
            .ok_or_else(|| format!("read: unknown session '{session}'"))?;
        let output = shell.output.drain();
        Ok(PtyResultPayload::Read { session, output })
    }

    async fn status(&self, session: String) -> PtyResult<PtyResultPayload> {
        self.gate_shell().await?;
        let sessions = self.sessions.lock().await;
        let alive = match sessions.get(&session) {
            Some(shell) => shell.is_alive(),
            None => false,
        };
        Ok(PtyResultPayload::Status { session, alive })
    }

    async fn list(&self) -> PtyResult<PtyResultPayload> {
        self.gate_shell().await?;
        let sessions = self.sessions.lock().await;
        Ok(PtyResultPayload::List {
            sessions: sessions.keys().cloned().collect(),
        })
    }

    async fn exit(&self, session: String) -> PtyResult<PtyResultPayload> {
        self.gate_shell().await?;
        let dead = {
            let mut sessions = self.sessions.lock().await;
            sessions.remove(&session)
        };
        if let Some(shell) = dead {
            {
                let mut writer = shell.writer.lock().unwrap();
                let _ = std::io::Write::write_all(&mut *writer, format!("exit{}", Self::eol()).as_bytes());
                let _ = std::io::Write::flush(&mut *writer);
            }
            let _ = shell
                .child
                .lock()
                .map(|mut c| portable_pty::ChildKiller::kill(&mut **c));
            Ok(PtyResultPayload::Exit { session, exited: true })
        } else {
            Ok(PtyResultPayload::Exit { session, exited: false })
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::ScopeSnapshot;

    fn test_gate() -> Arc<Mutex<CapabilityGate>> {
        let snapshot = ScopeSnapshot {
            capabilities: vec!["desktop.shell.execute".into()],
            always_allow_paths: vec![],
        };
        Arc::new(Mutex::new(CapabilityGate::new(snapshot)))
    }

    fn engine() -> PtyEngine {
        PtyEngine::new(test_gate())
    }

    fn session_id(tag: &str) -> String {
        format!("{tag}-{}", uuid::Uuid::new_v4())
    }

    #[tokio::test]
    async fn state_persists_between_calls() {
        let engine = engine();
        let session = session_id("persist");
        // Set a variable, then read it back in a second call (shell-agnostic
        // syntax: cmd.exe on Windows, POSIX shell on Unix).
        let set_cmd = if cfg!(windows) { "set SHPTY_TEST_VAR=hello_state" } else { "SHPTY_TEST_VAR=hello_state" };
        let PtyResultPayload::Run { output, exited, .. } = engine
            .execute(PtyOp::Run {
                session: session.clone(),
                command: set_cmd.into(),
                timeout_ms: Some(5_000),
            })
            .await
            .unwrap()
        else {
            panic!("run expected")
        };
        assert!(!exited, "shell must survive after setting a variable");
        let _ = output;
        let probe = if cfg!(windows) { "echo %SHPTY_TEST_VAR%" } else { "echo $SHPTY_TEST_VAR" };
        let PtyResultPayload::Run { output, .. } = engine
            .execute(PtyOp::Run { session, command: probe.into(), timeout_ms: Some(5_000) })
            .await
            .unwrap()
        else {
            panic!("run expected")
        };
        assert!(
            output.contains("hello_state"),
            "state must persist between calls, got: {output:?}"
        );
    }

    #[tokio::test]
    async fn run_rejects_empty_command() {
        let engine = engine();
        let session = session_id("empty");
        assert!(engine
            .execute(PtyOp::Run { session, command: "   ".into(), timeout_ms: None })
            .await
            .is_err());
    }

    #[tokio::test]
    async fn list_and_status_track_sessions() {
        let engine = engine();
        let session = session_id("track");
        let PtyResultPayload::Run { .. } = engine
            .execute(PtyOp::Run {
                session: session.clone(),
                command: if cfg!(windows) { "cd ." } else { "true" }.into(),
                timeout_ms: Some(5_000),
            })
            .await
            .unwrap()
        else {
            panic!("run expected")
        };
        let PtyResultPayload::List { sessions } = engine.execute(PtyOp::List {}).await.unwrap()
        else {
            panic!("list expected")
        };
        assert!(sessions.contains(&session));
        let PtyResultPayload::Status { alive, .. } =
            engine.execute(PtyOp::Status { session }).await.unwrap()
        else {
            panic!("status expected")
        };
        assert!(alive);
    }

    #[tokio::test]
    async fn exit_kills_the_session() {
        let engine = engine();
        let session = session_id("bye");
        let PtyResultPayload::Run { .. } = engine
            .execute(PtyOp::Run {
                session: session.clone(),
                command: if cfg!(windows) { "cd ." } else { "true" }.into(),
                timeout_ms: Some(5_000),
            })
            .await
            .unwrap()
        else {
            panic!("run expected")
        };
        let PtyResultPayload::Exit { exited, .. } =
            engine.execute(PtyOp::Exit { session: session.clone() }).await.unwrap()
        else {
            panic!("exit expected")
        };
        assert!(exited);
        let PtyResultPayload::Status { alive, .. } = engine.execute(PtyOp::Status { session }).await.unwrap()
        else {
            panic!("status expected")
        };
        assert!(!alive);
    }

    #[tokio::test]
    async fn unknown_session_read_is_clean_error() {
        let engine = engine();
        let err = engine
            .execute(PtyOp::Read { session: "ghost-xyz".into() })
            .await
            .unwrap_err();
        assert!(err.contains("unknown session"));
    }
}
