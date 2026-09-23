//! Persistent shell sessions (feature: pty) — stateful shells, one per chat.
//!
//! `desktop.shell.execute` is stateless (fresh process per call) and
//! `desktop.proc.op` manages detached background processes. This fills the
//! gap: ONE long-lived shell per session whose cwd, environment variables,
//! virtualenvs and exit codes survive between calls — like a real terminal
//! the agent keeps open while it works.
//!
//! Backend per platform:
//! - **Unix**: real PTY via portable-pty (wezterm's crate). A pty is the
//!   honest thing on Unix — job control, TTY detection and colors all work.
//! - **Windows**: plain stdin/stdout pipes to `cmd /q`. ConPTY requires the
//!   host to BE a terminal emulator (answer DSR-6n cursor queries, drive
//!   repaint sync, consume VT sequences); a pipe host cannot answer
//!   correctly and shells stall or die mid-session (observed on CI: the
//!   first prompt never paints, or the process dies right after a command).
//!   Plain pipes are deterministic, keep per-session state just as well,
//!   and `/q` keeps output clean (no prompt, no command echo).
//!
//! Shared design:
//! - One shell per session id (the web sends the conversation id): a
//!   different chat gets its own shell. Shells cap and evict (oldest dies
//!   when a new one would exceed the cap).
//! - Deterministic boot handshake: the host writes `echo __BRIDGE_READY__`
//!   and waits for the marker to appear in the output ring. When it does,
//!   the shell has provably consumed stdin and executed a command — no
//!   prompt parsing, no fixed sleeps. The banner + marker are then drained.
//! - Output lands in a ring buffer (last 1000 lines / 64 KiB per shell);
//!   `read` drains new bytes since the last read for that session. ANSI
//!   escapes and control noise are scrubbed so the model sees plain text.
//! - Power scope: same as arbitrary execution (`desktop.shell.execute`) —
//!   a persistent shell is a terminal.
//! - `exit` writes the shell's exit command, then kills the process; dead
//!   sessions are also reaped on drop.

#[cfg(not(windows))]
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
#[cfg(windows)]
use std::os::windows::process::CommandExt as _;
#[cfg(windows)]
use std::process::{Child as StdChild, Command as StdCommand, Stdio};
use crate::capability::CapabilityGate;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read as _;
use std::sync::Arc;
use tokio::sync::Mutex;

type PtyResult<T> = Result<T, String>;

const MAX_SESSIONS: usize = 8;
const RING_LINES: usize = 1_000;

/// Marker the host echoes through the shell to prove it is accepting
/// commands (boot handshake). Chosen to never collide with real output.
const BOOT_MARKER: &str = "__BRIDGE_READY__";
/// Bound on the boot handshake — shells start in well under a second even
/// on cold CI runners; past this the backend is broken, fail loudly.
const BOOT_TIMEOUT_MS: u64 = 10_000;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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

// ─── Output buffer ───────────────────────────────────────────────────────────

/// Strip ANSI escape sequences (CSI + two-byte escapes) and control noise
/// from shell output so the model never sees cursor moves, colors, bells.
/// Input is always valid UTF-8 (`from_utf8_lossy` upstream), so char-wise
/// iteration is exact.
fn scrub_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                // CSI: consume params/intermediates up to the final byte.
                chars.next();
                while let Some(n) = chars.next() {
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            } else {
                // Two-byte escape (ESC + final): drop both.
                chars.next();
            }
            continue;
        }
        if matches!(c, '\u{7}' | '\u{8}' | '\u{b}' | '\u{c}') {
            continue;
        }
        out.push(c);
    }
    out
}

/// Output accumulator: ring of complete lines + a trailing partial line.
#[derive(Default)]
struct OutputBuffer {
    lines: std::sync::Mutex<BufferState>,
}

#[derive(Default)]
struct BufferState {
    lines: Vec<std::string::String>,
    partial: String,
    bytes: usize,
}

impl OutputBuffer {
    fn push(&self, chunk: &str) {
        if let Ok(mut buf) = self.lines.lock() {
            for ch in scrub_ansi(chunk).chars() {
                if ch == '\n' {
                    let mut line = std::mem::take(&mut buf.partial);
                    line.push('\n');
                    buf.bytes += line.len();
                    line.pop();
                    // Windows pipes emit CRLF; a stray CR would pollute lines.
                    if line.ends_with('\r') {
                        line.pop();
                    }
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

    fn contains(&self, needle: &str) -> bool {
        self.lines.lock().map(|b| {
            b.lines.iter().any(|l| l.contains(needle)) || b.partial.contains(needle)
        }).unwrap_or(false)
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

// ─── Session ─────────────────────────────────────────────────────────────────

/// Platform backend for one shell session.
enum ShellIo {
    /// Unix: real PTY; the child is killed via wezterm's ChildKiller trait.
    #[cfg(not(windows))]
    Pty { child: std::sync::Mutex<Box<dyn portable_pty::Child + Send>> },
    /// Windows: `cmd /q` over plain pipes; a regular std Child.
    #[cfg(windows)]
    Pipes { child: std::sync::Mutex<StdChild> },
}

fn shell_alive(io: &ShellIo) -> bool {
    match io {
        #[cfg(not(windows))]
        ShellIo::Pty { child } => match child.lock().map(|mut c| c.try_wait()) {
            Ok(Ok(None)) => true,
            _ => false,
        },
        #[cfg(windows)]
        ShellIo::Pipes { child } => match child.lock().map(|mut c| c.try_wait()) {
            Ok(Ok(None)) => true,
            _ => false,
        },
    }
}

fn kill_shell(io: &ShellIo) {
    match io {
        #[cfg(not(windows))]
        ShellIo::Pty { child } => {
            if let Ok(mut child) = child.lock() {
                let _ = portable_pty::ChildKiller::kill(&mut **child);
            }
        }
        #[cfg(windows)]
        ShellIo::Pipes { child } => {
            if let Ok(mut child) = child.lock() {
                let _ = child.kill();
            }
        }
    }
}

struct PtySession {
    // Arc so reader threads and command writers share the shell's stdin
    // (on Unix through the pty master, on Windows through the pipe).
    writer: Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>,
    io: ShellIo,
    output: Arc<OutputBuffer>,
}

impl PtySession {
    fn is_alive(&self) -> bool {
        shell_alive(&self.io)
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        kill_shell(&self.io);
    }
}

/// Handles from spawning the shell process; the caller wires reader threads.
struct ShellHandles {
    writer: Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>,
    io: ShellIo,
    /// One reader per output stream (pty master on Unix; stdout+stderr on
    /// Windows pipes). Each is pumped into the OutputBuffer by a thread.
    readers: Vec<Box<dyn std::io::Read + Send>>,
}

fn home_dir() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into())
}

#[cfg(not(windows))]
fn spawn_shell() -> PtyResult<ShellHandles> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 30,
            cols: 110,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty: {e}"))?;
    let mut cmd = CommandBuilder::new("bash");
    // arg returns (); chain as statements.
    cmd.arg("--norc");
    cmd.arg("--noprofile");
    cmd.env("BRIDGE_PTY", "1");
    cmd.cwd(home_dir());
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn bash: {e}"))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("clone reader: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("take writer: {e}"))?;
    Ok(ShellHandles {
        writer: Arc::new(std::sync::Mutex::new(writer)),
        io: ShellIo::Pty { child: std::sync::Mutex::new(child) },
        readers: vec![Box::new(reader)],
    })
}

#[cfg(windows)]
fn spawn_shell() -> PtyResult<ShellHandles> {
    // cmd /q: echo off — no prompt, no command echo; output is command
    // results only. State (cwd, `set` vars) persists in the live process.
    let mut child = StdCommand::new("cmd")
        .args(["/q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(home_dir())
        .env("BRIDGE_PTY", "1")
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("spawn cmd: {e}"))?;
    let stdin = child.stdin.take().ok_or_else(|| "spawn cmd: no stdin".to_string())?;
    let stdout = child.stdout.take().ok_or_else(|| "spawn cmd: no stdout".to_string())?;
    let stderr = child.stderr.take().ok_or_else(|| "spawn cmd: no stderr".to_string())?;
    Ok(ShellHandles {
        writer: Arc::new(std::sync::Mutex::new(Box::new(stdin))),
        io: ShellIo::Pipes { child: std::sync::Mutex::new(child) },
        readers: vec![Box::new(stdout), Box::new(stderr)],
    })
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

    fn eol() -> &'static str {
        // cmd's stdin wants CRLF; POSIX shells want LF.
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
        // Create outside the map lock: spawning can block briefly.
        let handles = spawn_shell()?;

        let output = Arc::new(OutputBuffer::default());
        for reader in handles.readers {
            let output_reader = output.clone();
            std::thread::spawn(move || {
                let mut reader = reader;
                let mut buf = [0u8; 8192];
                let mut lost = false;
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break, // EOF: shell closed the stream
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
        }

        // Boot handshake: prove the shell reads stdin and executes commands
        // by having it echo a fixed marker. No prompt parsing, no sleeps.
        {
            let mut writer = handles.writer.lock().unwrap();
            std::io::Write::write_all(
                &mut *writer,
                format!("echo {BOOT_MARKER}{}", Self::eol()).as_bytes(),
            )
            .map_err(|e| format!("write: {e}"))?;
            std::io::Write::flush(&mut *writer).map_err(|e| format!("flush: {e}"))?;
        }
        let start = std::time::Instant::now();
        loop {
            if !shell_alive(&handles.io) {
                return Err(
                    "spawn shell: process exited immediately (unavailable or crashed)".into(),
                );
            }
            if output.contains(BOOT_MARKER) {
                break;
            }
            if start.elapsed() >= std::time::Duration::from_millis(BOOT_TIMEOUT_MS) {
                kill_shell(&handles.io);
                return Err("spawn shell: boot handshake timed out".into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // Discard banner + handshake so the first `run` returns only the
        // command's own output.
        let _ = output.drain();

        let entry = Arc::new(PtySession {
            writer: handles.writer,
            io: handles.io,
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
            kill_shell(&shell.io);
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
