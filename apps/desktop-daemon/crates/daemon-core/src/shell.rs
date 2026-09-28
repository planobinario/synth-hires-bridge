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

/// Hard resource limits for shell execution (sandboxing that does not
/// depend on OS containers, works on every platform):
///
/// - MAX_TIMEOUT_MS caps a requested timeout: a misbehaving agent must not
///   be able to park a shell for hours (`timeout_ms: 86_400_000`).
/// - MAX_OUTPUT_BYTES per stream (stdout, stderr): a command like
///   `yes` or `cat /dev/urandom` previously grew the accumulator buffers
///   without bound — a daemon OOM. Past the cap the stream is truncated
///   (with a marker) and the loop DROPS further data instead of buffering:
///   the process keeps running but can no longer grow the daemon's memory.
///   The final result reports `output_truncated: true` honestly.
/// - Processes still get the full process-tree kill on timeout/cancel
///   (terminate_child), which is the real containment for runaway work.
pub struct ShellLimits;

impl ShellLimits {
    pub const MAX_TIMEOUT_MS: u64 = 10 * 60 * 1000; // 10 min
    pub const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024; // 4 MiB per stream
    pub const TRUNCATION_MARKER: &str = "\n…[synthhires: output truncated at 4 MiB]\n";
    /// Live-stream flush threshold: chunks sent over the WS bridge are
    /// BATCHED up to this size instead of one frame per line. The DO consumes
    /// every frame with a storage get+put, so a 1M-line flood as 1M tiny
    /// frames would drown the Durable Object (observed live: a 576 KB
    /// socket backlog and the worker wedged). 8 KiB batches cap a 4 MiB
    /// stream at ~512 frames — bounded work for the consumer.
    pub const STREAM_FLUSH_BYTES: usize = 8 * 1024;

    pub fn clamp_timeout(requested: Option<u64>) -> u64 {
        let t = requested.unwrap_or(30_000);
        t.min(Self::MAX_TIMEOUT_MS).max(1_000)
    }
}

fn push_capped(buf: &mut String, data: &str, dropped: &mut bool) {
    if buf.len() >= ShellLimits::MAX_OUTPUT_BYTES {
        *dropped = true;
        return; // hard drop: no memory growth past the cap
    }
    if buf.len() + data.len() > ShellLimits::MAX_OUTPUT_BYTES {
        let space = ShellLimits::MAX_OUTPUT_BYTES - buf.len();
        let cut = match data.char_indices().nth(space) {
            Some((i, _)) => i,
            None => data.len(),
        };
        buf.push_str(&data[..cut]);
        buf.push_str(ShellLimits::TRUNCATION_MARKER);
        *dropped = true;
        return;
    }
    buf.push_str(data);
}

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
    /// Honest signal: a stream hit the hard cap and data was dropped.
    /// The consumer (agent) knows the output is INCOMPLETE.
    #[serde(default)]
    pub output_truncated: bool,
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
        let timeout = ShellLimits::clamp_timeout(req.timeout_ms);
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
        // output (previous code returned empty strings). Hard-capped:
        // see ShellLimits — past the cap data is DROPPED, not buffered.
        let stdout_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let stdout_dropped = Arc::new(Mutex::new(false));
        let stderr_dropped = Arc::new(Mutex::new(false));

        // Stdout pump: batch to the stream AND accumulate (capped).
        // Past the cap the live stream STOPS broadcasting (one final marker
        // chunk) while the reader keeps draining the pipe so the child is
        // never blocked by a full stdout buffer.
        let txo = tx.clone();
        let acco = stdout_buf.clone();
        let dropo = stdout_dropped.clone();
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut pending = String::new();
            let mut stream_stopped = false;
            while let Ok(Some(line)) = lines.next_line().await {
                let data = format!("{}\n", line);
                push_capped(&mut *acco.lock().await, &data, &mut *dropo.lock().await);
                if *dropo.lock().await {
                    if !stream_stopped {
                        // Flush what was batched so far, then the marker.
                        if !pending.is_empty() {
                            let _ = txo
                                .send(ShellOutputChunk {
                                    channel: "stdout",
                                    data: std::mem::take(&mut pending),
                                })
                                .await;
                        }
                        let _ = txo
                            .send(ShellOutputChunk {
                                channel: "stdout",
                                data: ShellLimits::TRUNCATION_MARKER.to_string(),
                            })
                            .await;
                        stream_stopped = true;
                    }
                    continue; // keep DRAINING the pipe, but don't broadcast
                }
                pending.push_str(&data);
                if pending.len() >= ShellLimits::STREAM_FLUSH_BYTES {
                    if txo
                        .send(ShellOutputChunk {
                            channel: "stdout",
                            data: std::mem::take(&mut pending),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            if !pending.is_empty() {
                let _ = txo
                    .send(ShellOutputChunk {
                        channel: "stdout",
                        data: pending,
                    })
                    .await;
            }
        });
        // Stderr pump: same batching + stop-at-cap policy.
        let txe = tx.clone();
        let acce = stderr_buf.clone();
        let drope = stderr_dropped.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let mut pending = String::new();
            let mut stream_stopped = false;
            while let Ok(Some(line)) = lines.next_line().await {
                let data = format!("{}\n", line);
                push_capped(&mut *acce.lock().await, &data, &mut *drope.lock().await);
                if *drope.lock().await {
                    if !stream_stopped {
                        if !pending.is_empty() {
                            let _ = txe
                                .send(ShellOutputChunk {
                                    channel: "stderr",
                                    data: std::mem::take(&mut pending),
                                })
                                .await;
                        }
                        let _ = txe
                            .send(ShellOutputChunk {
                                channel: "stderr",
                                data: ShellLimits::TRUNCATION_MARKER.to_string(),
                            })
                            .await;
                        stream_stopped = true;
                    }
                    continue;
                }
                pending.push_str(&data);
                if pending.len() >= ShellLimits::STREAM_FLUSH_BYTES {
                    if txe
                        .send(ShellOutputChunk {
                            channel: "stderr",
                            data: std::mem::take(&mut pending),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            if !pending.is_empty() {
                let _ = txe
                    .send(ShellOutputChunk {
                        channel: "stderr",
                        data: pending,
                    })
                    .await;
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
            stdout_dropped,
            stderr_dropped,
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
    stdout_dropped: Arc<Mutex<bool>>,
    stderr_dropped: Arc<Mutex<bool>>,
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
                let output_truncated =
                    *self.stdout_dropped.lock().await || *self.stderr_dropped.lock().await;
                Ok(ShellResult {
                    exit_code: status.code(),
                    stdout: stdout_buf,
                    stderr: stderr_buf,
                    duration_ms: self.started_at.elapsed().as_millis() as u64,
                    output_truncated,
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

    #[test]
    fn timeout_is_clamped_to_the_hard_cap() {
        assert_eq!(ShellLimits::clamp_timeout(None), 30_000);
        assert_eq!(ShellLimits::clamp_timeout(Some(5_000)), 5_000);
        // A day-long timeout collapses to the cap.
        assert_eq!(
            ShellLimits::clamp_timeout(Some(86_400_000)),
            ShellLimits::MAX_TIMEOUT_MS
        );
        // Degenerate values floor at 1s (never 0 — an instant timeout is a bug).
        assert_eq!(ShellLimits::clamp_timeout(Some(0)), 1_000);
    }

    #[tokio::test]
    async fn output_past_the_cap_is_dropped_and_honestly_reported() {
        let gate = CapabilityGate::new(ScopeSnapshot {
            capabilities: vec!["desktop.shell.execute".into()],
            always_allow_paths: Vec::new(),
            one_shot_paths: vec![],
        });
        // Far more than 4 MiB on stdout (~6.9 MB from seq 1 1_000_000).
        // The daemon must survive with a bounded buffer and report the
        // truncation honestly.
        let command = if cfg!(target_os = "windows") {
            "powershell -Command \"for($i=0;$i -lt 900000;$i++){ Write-Output 0123456789 }\""
        } else {
            "seq 1 1000000"
        };
        let (mut output, future) = ShellRunner::new(&gate)
            .run(
                ShellRequest {
                    command: command.into(),
                    cwd: None,
                    timeout_ms: Some(60_000),
                },
                CancellationToken::new(),
            )
            .await
            .expect("shell child should spawn");
        // Drain the stream while running (the required contract).
        let drain = tokio::spawn(async move { while output.recv().await.is_some() {} });
        let result = future.await_result().await.expect("shell result");
        drain.await.expect("output drain should finish");
        assert!(
            result.output_truncated,
            "1M lines (~6.9MB) must trip the 4MiB cap and be reported"
        );
        assert!(
            result.stdout.len() <= ShellLimits::MAX_OUTPUT_BYTES + 200,
            "buffer must stay bounded: {} bytes",
            result.stdout.len()
        );
        assert!(result.stdout.contains("[synthhires: output truncated"));
    }

    #[tokio::test]
    async fn normal_output_is_not_marked_truncated() {
        let gate = CapabilityGate::new(ScopeSnapshot {
            capabilities: vec!["desktop.shell.execute".into()],
            always_allow_paths: Vec::new(),
            one_shot_paths: vec![],
        });
        let command = if cfg!(target_os = "windows") { "echo hi" } else { "echo hi" };
        let (mut output, future) = ShellRunner::new(&gate)
            .run(
                ShellRequest {
                    command: command.into(),
                    cwd: None,
                    timeout_ms: Some(10_000),
                },
                CancellationToken::new(),
            )
            .await
            .expect("shell child should spawn");
        let drain = tokio::spawn(async move { while output.recv().await.is_some() {} });
        let result = future.await_result().await.expect("shell result");
        drain.await.expect("output drain should finish");
        assert!(!result.output_truncated);
        assert!(result.stdout.contains("hi"));
    }
}
