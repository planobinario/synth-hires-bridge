//! Shell execution.
//!
//! HARD RULE: the daemon never invokes `sh -c "<user args>"` with
//! user-provided args concatenated as a string. That is the classic
//! shell-injection vector. We use `std::process::Command` with the
//! command and args as discrete argv entries, so the kernel's exec
//! boundary is the only interpretation point.
//!
//! The only string-concat path is the explicit "shell" mode where
//! the user enters a single command string (e.g. "ls -la"). On Unix
//! that goes through `Command::new("bash").arg("-lc").arg(cmd)`; on
//! Windows there is no bash — we use `cmd.exe /C` with the command
//! string passed verbatim via `raw_arg` (not `Command::arg`, whose
//! C-runtime re-quoting mangles quoted paths), so the shell still
//! parses it as one command line.

use crate::{capability::CapabilityGate, DaemonError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Deserialize)]
pub struct ShellRequest {
    pub command: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShellOutputChunk {
    pub channel: &'static str,
    pub data: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShellResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

pub struct ShellRunner<'a> {
    gate: &'a CapabilityGate,
}

impl<'a> ShellRunner<'a> {
    pub fn new(gate: &'a CapabilityGate) -> Self {
        Self { gate }
    }

    /// Execute the request. Returns a stream handle + final result.
    /// The caller pipes chunks to the WS as `action_stream` frames and
    /// sends the final `action_result` with the exit code.
    pub async fn run(
        &self,
        req: ShellRequest,
        cancellation: CancellationToken,
    ) -> Result<(mpsc::Receiver<ShellOutputChunk>, ShellResultFuture)> {
        if !self.gate.allows("desktop.shell.execute") {
            return Err(DaemonError::CapabilityDenied(
                "desktop.shell.execute".into(),
            ));
        }
        let timeout = req.timeout_ms.unwrap_or(30_000);
        // NOTE: `cfg!` (runtime) compiles BOTH branches on every target, and
        // `tokio::process::Command::raw_arg` only exists on Windows — so this
        // must be a compile-time `#[cfg]`, or the Unix build fails.
        #[cfg(windows)]
        let mut cmd = {
            // /C runs the command and exits. `raw_arg` appends the command
            // line VERBATIM, without `Command::arg`'s C-runtime re-quoting.
            // That re-quoting is exactly what breaks quoted Windows paths
            // (`dir "C:\Users\me\Documents"`) — cmd.exe receives
            // backslash-escaped quotes it does not understand and errors
            // with "El nombre de archivo ... no son correctos".
            //
            // No shell-injection risk beyond what the user already typed:
            // the string is still parsed only by the user's own cmd.exe
            // semantics, never concatenated into a privileged argv.
            let mut c = Command::new("cmd.exe");
            c.raw_arg("/C").raw_arg(&req.command);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = Command::new("bash");
            c.arg("-lc").arg(&req.command);
            c
        };
        if let Some(cwd) = &req.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        // Kill on drop is the last-resort safety net; explicit cancellation
        // below also terminates the process tree and waits for the child.
        cmd.kill_on_drop(true);

        let start = std::time::Instant::now();
        let mut child = cmd.spawn().map_err(DaemonError::Io)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DaemonError::Io(std::io::Error::other("no stdout")))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| DaemonError::Io(std::io::Error::other("no stderr")))?;

        let (tx, rx) = mpsc::channel::<ShellOutputChunk>(64);

        // Accumulated buffers so the final result carries the REAL
        // output (previous code returned empty strings).
        let stdout_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf = Arc::new(Mutex::new(String::new()));

        // Stdout pump: broadcast to the stream AND accumulate.
        let txo = tx.clone();
        let acco = stdout_buf.clone();
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let data = format!("{}\n", line);
                acco.lock().await.push_str(&data);
                if txo
                    .send(ShellOutputChunk {
                        channel: "stdout",
                        data,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        // Stderr pump: broadcast to the stream AND accumulate.
        let txe = tx.clone();
        let acce = stderr_buf.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let data = format!("{}\n", line);
                acce.lock().await.push_str(&data);
                if txe
                    .send(ShellOutputChunk {
                        channel: "stderr",
                        data,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        let future = ShellResultFuture {
            child,
            timeout_ms: timeout,
            started_at: start,
            cancellation,
            tx,
            stdout_buf,
            stderr_buf,
            stdout_task,
            stderr_task,
        };
        Ok((rx, future))
    }
}

/// Awaits the child with the configured timeout or an explicit cancellation.
/// The receiver of the stream must drain it before this completes; otherwise
/// the pumps deadlock on a full channel.
pub struct ShellResultFuture {
    child: tokio::process::Child,
    timeout_ms: u64,
    started_at: std::time::Instant,
    cancellation: CancellationToken,
    tx: mpsc::Sender<ShellOutputChunk>,
    stdout_buf: Arc<Mutex<String>>,
    stderr_buf: Arc<Mutex<String>>,
    stdout_task: tokio::task::JoinHandle<()>,
    stderr_task: tokio::task::JoinHandle<()>,
}

impl ShellResultFuture {
    pub async fn await_result(mut self) -> Result<ShellResult> {
        // Cancellation is real only after the child has been terminated and
        // reaped. The UI therefore cannot display a terminal state early.
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(self.timeout_ms), async {
                tokio::select! {
                    result = self.child.wait() => result.map(Some),
                    _ = self.cancellation.cancelled() => {
                        terminate_child(&mut self.child).await;
                        Ok(None)
                    }
                }
            })
            .await;

        // Drain tx so the pumps don't deadlock, then wait for both output
        // readers before returning the terminal state.
        drop(self.tx);
        let _ = self.stdout_task.await;
        let _ = self.stderr_task.await;

        match result {
            Ok(Ok(Some(status))) => {
                let stdout_buf = self.stdout_buf.lock().await.clone();
                let stderr_buf = self.stderr_buf.lock().await.clone();
                Ok(ShellResult {
                    exit_code: status.code(),
                    stdout: stdout_buf,
                    stderr: stderr_buf,
                    duration_ms: self.started_at.elapsed().as_millis() as u64,
                })
            }
            Ok(Ok(None)) => Err(DaemonError::Cancelled),
            Ok(Err(e)) => Err(DaemonError::Io(e)),
            Err(_) => {
                terminate_child(&mut self.child).await;
                Err(DaemonError::Timeout(self.timeout_ms))
            }
        }
    }
}

async fn terminate_child(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        #[cfg(target_os = "windows")]
        {
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .status()
                .await;
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status()
                .await;
        }
    }
    // Fallback if the tree command is unavailable or the child exited after
    // the PID lookup and before the platform signal was delivered.
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::ScopeSnapshot;

    #[tokio::test]
    async fn cancellation_terminates_child_and_returns_cancelled() {
        let gate = CapabilityGate::new(ScopeSnapshot {
            capabilities: vec!["desktop.shell.execute".into()],
            always_allow_paths: Vec::new(),
            one_shot_paths: vec![],
        });
        let cancellation = CancellationToken::new();
        let command = if cfg!(target_os = "windows") {
            "ping -n 30 127.0.0.1 >NUL"
        } else {
            "sleep 30"
        };
        let (mut output, future) = ShellRunner::new(&gate)
            .run(
                ShellRequest {
                    command: command.into(),
                    cwd: None,
                    timeout_ms: Some(10_000),
                },
                cancellation.clone(),
            )
            .await
            .expect("shell child should spawn");
        let drain = tokio::spawn(async move { while output.recv().await.is_some() {} });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancellation.cancel();
        let result = future.await_result().await;
        assert!(matches!(result, Err(DaemonError::Cancelled)));
        drain.await.expect("output drain should finish");
    }
}
