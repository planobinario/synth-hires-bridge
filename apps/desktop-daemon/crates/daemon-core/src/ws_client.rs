use crate::{
    capability::{CapabilityGate, ScopeSnapshot},
    chat_store::ChatStore,
    consent::ConsentBroker,
    health::WsHealth,
    system_ops::{
        fetch_network, kill_process, list_processes, watch_filesystem, FsWatchRequest,
        NetworkFetchRequest, ProcessKillRequest, ProcessListRequest,
    },
    task_registry::{
        finish_global_task, record_global_task, register_global_cancellation, TaskKind, TaskState,
        TaskStatus,
    },
    DaemonError, Result,
};
use daemon_protocol::{parse_chat_push_params, BridgeFrame, HelloFrame, PROTOCOL_VERSION};
use futures_util::{SinkExt, StreamExt};
use sha2::Digest;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Canonical dispatch arm: parse params → run op → serialize → respond.
///
/// `$value`/`$req` are caller-chosen binding names (macro hygiene: bindings
/// created inside a macro are invisible to the passed block). `$run` is the
/// async op body; it must evaluate to `Result<impl Serialize, E: Display>`.
/// The macro owns the boilerplate: param-parse errors, `send_action_result`
/// with the canonical ok=true/None pairing, and `send_error` on op failure.
/// Arms with non-standard contracts (streaming shell, eval's JS-error
/// transport, verified-write flags, memory's blocking worker) stay hand-written.
macro_rules! dispatch_op {
    ($self:ident, $ws:ident, $request:ident, $started:ident, $value:ident, $req:ident, $param_ty:ty, $run:block) => {{
        let parsed = serde_json::from_value::<$param_ty>($request.params.clone());
        match parsed {
            Ok($value) => {
                // Most arms don't need the request — the lint is silenced per
                // binding, not per arm, so every arm stays symmetric.
                #[allow(unused_variables)]
                let $req = &$request;
                let result = async move {
                    $run
                }
                .await;
                match result {
                    Ok(ok) => {
                        $self
                            .send_action_result(
                                $ws,
                                $request.id.clone(),
                                true,
                                Some(serde_json::to_value(&ok)?),
                                None,
                                elapsed_ms($started),
                            )
                            .await
                    }
                    Err(error) => $self.send_error($ws, $request.id.clone(), error.to_string()).await,
                }
            }
            Err(error) => {
                $self
                    .send_error($ws, $request.id.clone(), format!("bad params: {error}"))
                    .await
            }
        }
    }};
}

pub struct WsClient {
    backend_url: String,
    token: String,
    _device_id: String,
    fingerprint: String,
    device_kind: &'static str,
    device_name: String,
    gate: Arc<Mutex<CapabilityGate>>,
    eval: Arc<crate::eval_session::EvalEngine>,
    lsp: Arc<crate::lsp_ops::LspEngine>,
    memory: Arc<crate::memory_store::MemoryStore>,
    proc: Arc<crate::proc_ops::ProcEngine>,
    pty: Arc<crate::pty_ops::PtyEngine>,
    browser: Arc<crate::browser_ops::BrowserStore>,
    dap: Arc<crate::dap_ops::DapEngine>,
    chat_store: Arc<ChatStore>,
    _consent: Arc<ConsentBroker>,
    health: Arc<WsHealth>,
}

impl WsClient {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backend_url: impl Into<String>,
        token: impl Into<String>,
        device_id: impl Into<String>,
        fingerprint: impl Into<String>,
        device_kind: &'static str,
        device_name: impl Into<String>,
        gate: CapabilityGate,
        chat_store: Arc<ChatStore>,
        consent: Arc<ConsentBroker>,
        health: Arc<WsHealth>,
    ) -> Self {
        let gate = Arc::new(Mutex::new(gate));
        Self {
            backend_url: backend_url.into(),
            token: token.into(),
            _device_id: device_id.into(),
            fingerprint: fingerprint.into(),
            device_kind,
            device_name: device_name.into(),
            gate: gate.clone(),
            // The eval engine shares the SAME gate Arc: a scope update or
            // revoke on the connection instantly constrains eval too.
            eval: Arc::new(crate::eval_session::EvalEngine::new(gate.clone())),
            lsp: Arc::new(crate::lsp_ops::LspEngine::new(gate.clone())),
            proc: Arc::new(crate::proc_ops::ProcEngine::new(gate.clone())),
            pty: Arc::new(crate::pty_ops::PtyEngine::new(gate.clone())),
            browser: Arc::new(crate::browser_ops::BrowserStore::new()),
            dap: Arc::new(crate::dap_ops::DapEngine::new(gate.clone())),
            memory: Arc::new(
                crate::memory_store::MemoryStore::open(crate::memory_store::MemoryStore::default_path())
                    .unwrap_or_else(|e| {
                        tracing::warn!("memory.db unavailable ({e}); using in-memory fallback");
                        crate::memory_store::MemoryStore::open_in_memory()
                            .expect("in-memory sqlite cannot fail to open")
                    }),
            ),
            chat_store,
            _consent: consent,
            health,
        }
    }

    pub fn health(&self) -> Arc<WsHealth> {
        self.health.clone()
    }

    pub async fn run(&self) -> Result<()> {
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.connect_once().await {
                Ok(()) => {
                    self.health.mark_disconnected();
                }
                Err(DaemonError::Protocol(message))
                    if message.contains("auth_failed") || message.contains("revoked") =>
                {
                    self.health.set_error(&message);
                    self.health.mark_disconnected();
                    return Err(DaemonError::Protocol(message));
                }
                Err(error) => {
                    self.health.set_error(&error.to_string());
                    self.health.mark_disconnected();
                    tracing::warn!("WS error: {error}; reconnecting in {:?}", backoff);
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
            tokio::time::sleep(Duration::from_millis(rand::random::<u64>() % 1000)).await;
        }
    }

    async fn connect_once(&self) -> Result<()> {
        let mut request = self
            .backend_url
            .clone()
            .into_client_request()
            .map_err(|e| DaemonError::Ws(format!("into_client_request: {e}")))?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            format!("bearer.{}", self.token).parse().map_err(
                |e: http::header::InvalidHeaderValue| {
                    DaemonError::Ws(format!("invalid header: {e}"))
                },
            )?,
        );
        let token_hash = hex::encode(sha2::Sha256::digest(self.token.as_bytes()));
        request.headers_mut().insert(
            "x-bridge-token-hash",
            token_hash
                .parse()
                .map_err(|e: http::header::InvalidHeaderValue| {
                    DaemonError::Ws(format!("invalid header: {e}"))
                })?,
        );
        let (mut ws, _) = connect_async(request)
            .await
            .map_err(|e| DaemonError::Ws(format!("connect: {e}")))?;
        let hello = BridgeFrame::Hello(HelloFrame {
            v: PROTOCOL_VERSION,
            token_hash,
            fingerprint: self.fingerprint.clone(),
            device_kind: if self.device_kind == "desktop" {
                daemon_protocol::DeviceKind::Desktop
            } else {
                daemon_protocol::DeviceKind::Mobile
            },
            device_name: self.device_name.clone(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            // Protocol v1.1 capability negotiation. The web only dispatches
            // new-feature actions to daemons that advertise them.
            features: vec![
                "fs.patch".to_string(),
                "ast".to_string(),
                "eval".to_string(),
                "lsp".to_string(),
                "memory".to_string(),
                "git".to_string(),
                "proc".to_string(),
                "pty".to_string(),
                "browser".to_string(),
                "dap".to_string(),
            ],
        });
        ws.send(Message::Text(serde_json::to_string(&hello)?))
            .await
            .map_err(|e| DaemonError::Ws(format!("send hello: {e}")))?;
        let first = ws
            .next()
            .await
            .ok_or_else(|| DaemonError::Protocol("ws closed before hello_ack".into()))?
            .map_err(|e| DaemonError::Ws(format!("recv hello_ack: {e}")))?;
        let frame: BridgeFrame = match first {
            Message::Text(text) => serde_json::from_str(&text)?,
            _ => return Err(DaemonError::Protocol("hello_ack not text".into())),
        };
        let ack = match frame {
            BridgeFrame::HelloAck(value) => value,
            BridgeFrame::Error(error) => {
                return Err(DaemonError::Protocol(format!(
                    "{}: {}",
                    error.code, error.message
                )))
            }
            _ => return Err(DaemonError::Protocol("expected hello_ack".into())),
        };
        self.health.mark_connected();
        *self.gate.lock().await = CapabilityGate::new(ScopeSnapshot::from(&ack.scopes));
        let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                Some(message) = ws.next() => {
                    match message.map_err(|e| DaemonError::Ws(format!("recv: {e}")))? {
                        Message::Text(text) => {
                            let frame: BridgeFrame = serde_json::from_str(&text)?;
                            if let BridgeFrame::HeartbeatAck(ref ack) = frame { self.health.mark_heartbeat_ack(ack.t); }
                            self.handle_frame(&mut ws, frame).await?;
                        }
                        Message::Close(_) => return Ok(()),
                        _ => {}
                    }
                }
                _ = heartbeat.tick() => {
                    let frame = BridgeFrame::Heartbeat(daemon_protocol::HeartbeatFrame { v: PROTOCOL_VERSION, t: now_ms() });
                    ws.send(Message::Text(serde_json::to_string(&frame)?)).await
                        .map_err(|e| DaemonError::Ws(format!("send heartbeat: {e}")))?;
                }
            }
        }
    }

    async fn handle_frame(&self, ws: &mut WsStream, frame: BridgeFrame) -> Result<()> {
        match frame {
            BridgeFrame::ActionRequest(request) if request.capability == "sync.chat.push" => {
                self.handle_chat_push(ws, request).await
            }
            BridgeFrame::ActionRequest(request) => self.handle_action(ws, request).await,
            BridgeFrame::ScopeUpdate(update) => {
                *self.gate.lock().await = CapabilityGate::new(ScopeSnapshot::from(&update.scopes));
                Ok(())
            }
            BridgeFrame::Revoke(_) => Err(DaemonError::Protocol("revoked".into())),
            BridgeFrame::Error(error) => {
                tracing::error!("server error: {}: {}", error.code, error.message);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn handle_action(
        &self,
        ws: &mut WsStream,
        request: daemon_protocol::ActionRequestFrame,
    ) -> Result<()> {
        self.register_task(&request);
        self.audit(&request);
        // Composite actions are authorized by their parent scope (patch/edit
        // are writes, ast_grep is a read): pairing scopes stay untouched
        // across daemon updates.
        let required_scope = match request.capability.as_str() {
            "desktop.fs.patch" | "desktop.code.ast_edit" => "desktop.fs.write",
            "desktop.code.ast_grep" => "desktop.fs.read",
            // Arbitrary JS with require() is shell-equivalent power: gate it
            // by the terminal-command scope (consent dialog already answered).
            "desktop.code.eval" | "desktop.lsp.rename" => "desktop.shell.execute",
            "desktop.lsp.diagnostics" | "desktop.lsp.hover" | "desktop.lsp.definition" => {
                "desktop.fs.read"
            }
            "desktop.memory.recall" | "desktop.memory.stats" => "desktop.fs.read",
            "desktop.git.overview" | "desktop.git.diff" => "desktop.fs.read",
            "desktop.memory.remember" | "desktop.memory.forget" | "desktop.memory.reflect" => {
                "desktop.fs.write"
            }
            // One capability, scope derived from the actual op in params —
            // an op can never ride in under a weaker sibling's scope.
            "desktop.proc.op" => match request.params.get("op").and_then(|v| v.as_str()) {
                Some("signal") => "desktop.process.kill",
                Some("probe") => "desktop.network.fetch",
                Some("start") => "desktop.shell.execute",
                _ => "desktop.fs.read", // logs / wait / list
            }
            "desktop.pty.op" => "desktop.shell.execute", // a PTY is a terminal
            // Browser: navigate ≈ network read; act (click/type in real pages)
            // rides the same consent tier as shell; snapshots/screenshots are reads.
            "desktop.browser.launch" | "desktop.browser.nav" => "desktop.network.fetch",
            "desktop.browser.act" => "desktop.shell.execute",
            "desktop.browser.snapshot" | "desktop.browser.shot" | "desktop.browser.wait"
            | "desktop.browser.close" => "desktop.fs.read",
            // Debugger: start/eval/flow-control are as powerful as a shell;
            // inspection ops (breakpoints, stack, variables, threads) are reads.
            "desktop.debug.op" => match request.params.get("op").and_then(|v| v.as_str()) {
                Some("start") | Some("eval") | Some("continue") | Some("step") | Some("pause") => {
                    "desktop.shell.execute"
                }
                _ => "desktop.fs.read",
            }
            other => other,
        };
        if !self.gate.lock().await.allows(required_scope) {
            return self
                .send_error(
                    ws,
                    request.id,
                    format!("capability not granted: {}", request.capability),
                )
                .await;
        }
        if request.capability == "desktop.shell.execute" {
            let command = request
                .params
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if contains_dangerous_shell(command) {
                return self
                    .send_error(
                        ws,
                        request.id,
                        "hard_stop_blocked: dangerous command pattern".into(),
                    )
                    .await;
            }
        }
        if request.capability == "desktop.process.kill" {
            if !self.gate.lock().await.allows("desktop.process.kill") {
                return self
                    .send_error(ws, request.id, "capability_denied: desktop.process.kill".into())
                    .await;
            }
        }

        let started = std::time::Instant::now();
        match request.capability.as_str() {
            "desktop.fs.read" => {
                dispatch_op!(self, ws, request, started, value, req, crate::fs_ops::FsReadRequest, {
                    let annotate = value.annotate.unwrap_or(false);
                    let gate = self.path_gate(req, "desktop.fs.read", &value.path).await?;
                    crate::fs_ops::FsOps::new(&gate)
                        .read_annotated(value, annotate)
                        .await
                        .map(|r| serde_json::json!({"content_base64": r.content_base64, "size": r.size}))
                })
            }
            "desktop.fs.patch" => {
                let parsed =
                    serde_json::from_value::<crate::fs_ops::FsPatchRequest>(request.params.clone());
                match parsed {
                    Ok(value) => match self.path_gate(&request, "desktop.fs.write", &value.path).await {
                        Ok(gate) => match crate::fs_ops::FsOps::new(&gate).patch(value).await {
                            Ok(result) => {
                                self.send_action_result(
                                    ws,
                                    request.id,
                                    result.verified,
                                    Some(serde_json::to_value(&result)?),
                                    (!result.verified).then_some("patch read-back verification failed".into()),
                                    elapsed_ms(started),
                                )
                                .await
                            }
                            Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                        },
                        Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                    },
                    Err(error) => self.send_error(ws, request.id, format!("bad params: {error}")).await,
                }
            }
            "desktop.fs.write" => {
                let parsed =
                    serde_json::from_value::<crate::fs_ops::FsWriteRequest>(request.params.clone());
                match parsed {
                    Ok(value) => match self.path_gate(&request, "desktop.fs.write", &value.path).await {
                        Ok(gate) => match crate::fs_ops::FsOps::new(&gate).write(value).await {
                            Ok(result) => self.send_action_result(ws, request.id, result.verified, Some(serde_json::json!({"bytes_written": result.bytes_written, "verified": result.verified})), (!result.verified).then_some("write read-back verification failed".into()), elapsed_ms(started)).await,
                            Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                        },
                        Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                    },
                    Err(error) => self.send_error(ws, request.id, format!("bad params: {error}")).await,
                }
            }
            "desktop.code.ast_grep" => {
                dispatch_op!(self, ws, request, started, value, req, crate::ast_ops::AstGrepRequest, {
                    let gate = self.gate.lock().await.clone();
                    crate::ast_ops::AstOps::new(&gate).grep(value).await
                })
            }
            "desktop.code.ast_edit" => {
                let parsed = serde_json::from_value::<
                    crate::ast_ops::AstEditRequest,
                >(request.params.clone());
                match parsed {
                    Ok(value) => {
                        let gate = self.gate.lock().await.clone();
                        match crate::ast_ops::AstOps::new(&gate).edit(value).await {
                            Ok(result) => {
                                self.send_action_result(
                                    ws,
                                    request.id,
                                    result.verified,
                                    Some(serde_json::to_value(&result)?),
                                    (!result.verified)
                                        .then_some("ast_edit read-back verification failed".into()),
                                    elapsed_ms(started),
                                )
                                .await
                            }
                            Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                        }
                    }
                    Err(error) => self.send_error(ws, request.id, format!("bad params: {error}")).await,
                }
            }
            "desktop.code.eval" => {
                let parsed = serde_json::from_value::<crate::eval_session::EvalRequest>(
                    request.params.clone(),
                );
                match parsed {
                    Ok(value) => match self.eval.exec(value).await {
                        Ok(result) => {
                            self.send_action_result(
                                ws,
                                request.id,
                                true,
                                Some(serde_json::to_value(&result)?),
                                None,
                                elapsed_ms(started),
                            )
                            .await
                        }
                        Err(DaemonError::EvalFailed {
                            session_id,
                            created_session,
                            message,
                            stdout,
                            stderr,
                        }) => {
                            // A JS-level failure is a SUCCESSFUL action
                            // transport-wise: the model gets stdout/stderr
                            // and the session survives for the next turn.
                            self.send_action_result(
                                ws,
                                request.id,
                                false,
                                Some(serde_json::json!({
                                    "sessionId": session_id,
                                    "createdSession": created_session,
                                    "error": message,
                                    "stdout": stdout,
                                    "stderr": stderr,
                                })),
                                None,
                                elapsed_ms(started),
                            )
                            .await
                        }
                        Err(other) => self.send_error(ws, request.id, other.to_string()).await,
                    },
                    Err(error) => self.send_error(ws, request.id, format!("bad params: {error}")).await,
                }
            }
            "desktop.lsp.diagnostics" => {
                dispatch_op!(self, ws, request, started, value, req, crate::lsp_ops::LspFileRequest, {
                    self.lsp.diagnostics(value).await
                })
            }
            "desktop.lsp.hover" => {
                dispatch_op!(self, ws, request, started, value, req, crate::lsp_ops::LspPositionRequest, {
                    self.lsp.hover(value).await
                })
            }
            "desktop.lsp.definition" => {
                dispatch_op!(self, ws, request, started, value, req, crate::lsp_ops::LspPositionRequest, {
                    self.lsp.definition(value).await
                })
            }
            "desktop.lsp.rename" => {
                let parsed = serde_json::from_value::<crate::lsp_ops::LspRenameRequest>(
                    request.params.clone(),
                );
                match parsed {
                    Ok(value) => match self.lsp.rename(value).await {
                        Ok(result) => {
                            self.send_action_result(
                                ws,
                                request.id,
                                result.edits_applied > 0,
                                Some(serde_json::to_value(&result)?),
                                None,
                                elapsed_ms(started),
                            )
                            .await
                        }
                        Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                    },
                    Err(error) => self.send_error(ws, request.id, format!("bad params: {error}")).await,
                }
            }
            "desktop.memory.remember" | "desktop.memory.recall" | "desktop.memory.forget"
            | "desktop.memory.reflect" | "desktop.memory.stats" => {
                let memory = self.memory.clone();
                let params = request.params.clone();
                let id = request.id;
                let started_at = started;
                // rusqlite is blocking: run the op off the async runtime, but
                // answer on this connection (respond_to needs ws).
                let (tx, rx) = tokio::sync::oneshot::channel::<
                    std::result::Result<serde_json::Value, String>,
                >();
                tokio::task::spawn_blocking(move || {
                    let outcome = match serde_json::from_value::<
                        crate::memory_store::MemoryOp,
                    >(params)
                    {
                        Ok(op) => match memory.execute(op) {
                            Ok(result) => serde_json::to_value(&result)
                                .map_err(|e| format!("serialize: {e}")),
                            Err(e) => Err(e),
                        },
                        Err(e) => Err(format!("bad params: {e}")),
                    };
                    let _ = tx.send(outcome);
                });
                let elapsed_here = elapsed_ms(started_at);
                let _ = elapsed_here;
                match rx.await {
                    Ok(Ok(value)) => {
                        self.send_action_result(ws, id, true, Some(value), None, elapsed_ms(started))
                            .await
                    }
                    Ok(Err(message)) => self.send_error(ws, id, message).await,
                    Err(_) => {
                        self.send_error(ws, id, "memory: worker dropped".into()).await
                    }
                }
            }
            "desktop.proc.op" => {
                dispatch_op!(self, ws, request, started, value, req, crate::proc_ops::ProcOp, {
                    self.proc.execute(value).await
                })
            }
            "desktop.pty.op" => {
                dispatch_op!(self, ws, request, started, value, req, crate::pty_ops::PtyOp, {
                    self.pty.execute(value).await
                })
            }
            "desktop.browser.launch" => {
                dispatch_op!(self, ws, request, started, value, req, crate::browser_ops::BrowserLaunchParams, {
                    self.browser.launch(value).await
                })
            }
            "desktop.browser.nav" => {
                dispatch_op!(self, ws, request, started, value, req, crate::browser_ops::BrowserNavigateParams, {
                    self.browser.navigate(value).await
                })
            }
            "desktop.browser.snapshot" => {
                dispatch_op!(self, ws, request, started, value, req, crate::browser_ops::SnapshotParams, {
                    self.browser.snapshot(value).await
                })
            }
            "desktop.browser.act" => {
                dispatch_op!(self, ws, request, started, value, req, crate::browser_ops::BrowserActParams, {
                    self.browser.act(value).await
                })
            }
            "desktop.browser.shot" => {
                dispatch_op!(self, ws, request, started, value, req, crate::browser_ops::ScreenshotParams, {
                    self.browser.screenshot(value).await
                })
            }
            "desktop.browser.wait" => {
                dispatch_op!(self, ws, request, started, value, req, crate::browser_ops::WaitParams, {
                    self.browser.wait(value).await
                })
            }
            "desktop.debug.op" => {
                dispatch_op!(self, ws, request, started, value, req, crate::dap_ops::DapOp, {
                    self.dap.execute(value).await
                })
            }
            "desktop.browser.close" => {
                dispatch_op!(self, ws, request, started, value, req, crate::browser_ops::BrowserCloseParams, {
                    self.browser.close(value.kill).await
                })
            }
            "desktop.git.overview" => {
                dispatch_op!(self, ws, request, started, value, req, crate::git_ops::GitOverviewRequest, {
                    crate::git_ops::GitOps::new(&*self.gate.lock().await)
                        .overview(value)
                        .await
                })
            }
            "desktop.git.diff" => {
                dispatch_op!(self, ws, request, started, value, req, crate::git_ops::GitDiffRequest, {
                    crate::git_ops::GitOps::new(&*self.gate.lock().await).diff(value).await
                })
            }
            "desktop.fs.delete" => {
                let parsed = serde_json::from_value::<crate::fs_ops::FsDeleteRequest>(
                    request.params.clone(),
                );
                match parsed {
                    Ok(value) => match self
                        .path_gate(&request, "desktop.fs.delete", &value.path)
                        .await
                    {
                        Ok(gate) => match crate::fs_ops::FsOps::new(&gate).delete(value).await {
                            Ok(()) => {
                                self.send_action_result(
                                    ws,
                                    request.id,
                                    true,
                                    Some(serde_json::json!({"deleted": true})),
                                    None,
                                    elapsed_ms(started),
                                )
                                .await
                            }
                            Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                        },
                        Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                    },
                    Err(error) => {
                        self.send_error(ws, request.id, format!("bad params: {error}"))
                            .await
                    }
                }
            }
            "desktop.fs.verify" => {
                let parsed = serde_json::from_value::<crate::fs_ops::FsVerifyRequest>(
                    request.params.clone(),
                );
                match parsed {
                    Ok(value) => {
                        let gate = self.gate.lock().await.clone();
                        let result = crate::fs_ops::FsOps::new(&gate).verify(value).await;
                        self.send_action_result(ws, request.id, result.exists && result.readable && result.writable, Some(serde_json::json!({"exists": result.exists, "is_dir": result.is_dir, "readable": result.readable, "writable": result.writable})), result.error, elapsed_ms(started)).await
                    }
                    Err(error) => {
                        self.send_error(ws, request.id, format!("bad params: {error}"))
                            .await
                    }
                }
            }
            "desktop.fs.list" => {
                dispatch_op!(self, ws, request, started, value, req, crate::fs_ops::FsListRequest, {
                    let gate = self.gate.lock().await.clone();
                    crate::fs_ops::FsOps::new(&gate).list(value).await
                })
            }
            "desktop.fs.watch" => {
                let parsed = serde_json::from_value::<FsWatchRequest>(request.params.clone());
                match parsed {
                    Ok(value) => match self
                        .path_gate(&request, "desktop.fs.watch", &value.path)
                        .await
                    {
                        Ok(_) => match watch_filesystem(value).await {
                            Ok(result) => {
                                self.send_action_result(
                                    ws,
                                    request.id,
                                    true,
                                    Some(serde_json::to_value(result)?),
                                    None,
                                    elapsed_ms(started),
                                )
                                .await
                            }
                            Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                        },
                        Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                    },
                    Err(error) => {
                        self.send_error(ws, request.id, format!("bad params: {error}"))
                            .await
                    }
                }
            }
            "desktop.process.list" => {
                let parsed = serde_json::from_value::<ProcessListRequest>(request.params.clone());
                match parsed {
                    Ok(value) => match list_processes(value).await {
                        Ok(result) => {
                            self.send_action_result(
                                ws,
                                request.id,
                                true,
                                Some(serde_json::to_value(result)?),
                                None,
                                elapsed_ms(started),
                            )
                            .await
                        }
                        Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                    },
                    Err(error) => {
                        self.send_error(ws, request.id, format!("bad params: {error}"))
                            .await
                    }
                }
            }
            "desktop.process.kill" => {
                let parsed = serde_json::from_value::<ProcessKillRequest>(request.params.clone());
                match parsed {
                    Ok(value) => match kill_process(value).await {
                        Ok(result) => {
                            self.send_action_result(
                                ws,
                                request.id,
                                true,
                                Some(serde_json::to_value(result)?),
                                None,
                                elapsed_ms(started),
                            )
                            .await
                        }
                        Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                    },
                    Err(error) => {
                        self.send_error(ws, request.id, format!("bad params: {error}"))
                            .await
                    }
                }
            }
            "desktop.network.fetch" => {
                let parsed = serde_json::from_value::<NetworkFetchRequest>(request.params.clone());
                match parsed {
                    Ok(value) => match fetch_network(value).await {
                        Ok(result) => {
                            self.send_action_result(
                                ws,
                                request.id,
                                true,
                                Some(serde_json::to_value(result)?),
                                None,
                                elapsed_ms(started),
                            )
                            .await
                        }
                        Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                    },
                    Err(error) => {
                        self.send_error(ws, request.id, format!("bad params: {error}"))
                            .await
                    }
                }
            }
            "desktop.shell.execute" => {
                let parsed =
                    serde_json::from_value::<crate::shell::ShellRequest>(request.params.clone());
                match parsed {
                    Ok(value) => {
                        let cancellation = CancellationToken::new();
                        if let Ok(action_id) = Uuid::parse_str(&request.id) {
                            register_global_cancellation(action_id, cancellation.clone());
                        }
                        let run = {
                            let gate = self.gate.lock().await;
                            crate::shell::ShellRunner::new(&gate)
                                .run(value, cancellation)
                                .await
                        };
                        match run {
                            Ok((mut receiver, future)) => {
                                let handle =
                                    tokio::spawn(async move { future.await_result().await });
                                let mut seq = 0;
                                while let Some(chunk) = receiver.recv().await {
                                    let stream = daemon_protocol::ActionStreamFrame {
                                        v: PROTOCOL_VERSION,
                                        id: request.id.clone(),
                                        seq,
                                        channel: if chunk.channel == "stdout" {
                                            daemon_protocol::StreamChannel::Stdout
                                        } else {
                                            daemon_protocol::StreamChannel::Stderr
                                        },
                                        data: chunk.data,
                                        eof: false,
                                    };
                                    ws.send(Message::Text(serde_json::to_string(
                                        &BridgeFrame::ActionStream(stream),
                                    )?))
                                    .await?;
                                    seq += 1;
                                }
                                match handle.await {
                                    Ok(Ok(result)) => {
                                        let ok = result.exit_code == Some(0);
                                        self.send_action_result(ws, request.id, ok, Some(serde_json::json!({"exit_code": result.exit_code, "stdout": result.stdout, "stderr": result.stderr})), (!ok).then_some(format!("exit code {:?}", result.exit_code)), result.duration_ms).await
                                    }
                                    Ok(Err(DaemonError::Cancelled)) => {
                                        self.send_action_result(
                                            ws,
                                            request.id,
                                            false,
                                            None,
                                            Some("action_cancelled".into()),
                                            elapsed_ms(started),
                                        )
                                        .await
                                    }
                                    Ok(Err(error)) => {
                                        self.send_error(ws, request.id, error.to_string()).await
                                    }
                                    Err(error) => {
                                        self.send_error(
                                            ws,
                                            request.id,
                                            format!("shell task failed: {error}"),
                                        )
                                        .await
                                    }
                                }
                            }
                            Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                        }
                    }
                    Err(error) => {
                        self.send_error(ws, request.id, format!("bad params: {error}"))
                            .await
                    }
                }
            }
            other => {
                self.send_error(
                    ws,
                    request.id,
                    format!("capability not implemented in daemon: {other}"),
                )
                .await
            }
        }
    }

    fn register_task(&self, request: &daemon_protocol::ActionRequestFrame) {
        let Ok(id) = Uuid::parse_str(&request.id) else {
            return;
        };
        record_global_task(TaskState {
            id,
            kind: task_kind(&request.capability),
            description: format!("{} · {}", request.capability, request.id),
            status: TaskStatus::Running,
            started_at_instant: std::time::Instant::now(),
            started_at_utc: chrono::Utc::now(),
            finished_at: None,
        });
    }

    async fn path_gate(
        &self,
        _request: &daemon_protocol::ActionRequestFrame,
        capability: &str,
        path: &std::path::Path,
    ) -> Result<CapabilityGate> {
        if !self.gate.lock().await.allows(capability) {
            return Err(DaemonError::CapabilityDenied(capability.into()));
        }
        Ok(self
            .gate
            .lock()
            .await
            .with_additional_path(path.to_path_buf()))
    }

    async fn send_error(&self, ws: &mut WsStream, action_id: String, error: String) -> Result<()> {
        self.send_action_result(ws, action_id, false, None, Some(error), 0)
            .await
    }

    async fn send_action_result(
        &self,
        ws: &mut WsStream,
        action_id: String,
        ok: bool,
        output: Option<serde_json::Value>,
        error: Option<String>,
        duration_ms: u64,
    ) -> Result<()> {
        if let Ok(id) = Uuid::parse_str(&action_id) {
            let status = if ok {
                TaskStatus::Completed(None)
            } else if error.as_deref() == Some("action_cancelled") {
                TaskStatus::Killed
            } else {
                TaskStatus::Failed(error.clone().unwrap_or_else(|| "action_failed".into()))
            };
            finish_global_task(id, status);
        }
        let frame = daemon_protocol::ActionResultFrame {
            v: PROTOCOL_VERSION,
            id: action_id,
            ok,
            output,
            error: error.map(|message| daemon_protocol::ActionError {
                code: if message == "action_cancelled" {
                    "action_cancelled".into()
                } else {
                    "action_failed".into()
                },
                message,
            }),
            duration_ms,
        };
        ws.send(Message::Text(serde_json::to_string(
            &BridgeFrame::ActionResult(frame),
        )?))
        .await?;
        Ok(())
    }

    fn audit(&self, request: &daemon_protocol::ActionRequestFrame) {
        let config_dir = directories::ProjectDirs::from("com", "synthhires", "bridge")
            .map(|d| d.config_dir().to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from(".").join("synthhires-bridge"));
        let _ = std::fs::create_dir_all(&config_dir);
        let params = serde_json::to_string(&request.params).unwrap_or_default();
        let line = format!(
            "[{}] CAPABILITY: {} PARAMS: {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            request.capability,
            params
        );
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(config_dir.join("audit.log"))
        {
            use std::io::Write;
            let _ = file.write_all(line.as_bytes());
        }
    }

    async fn handle_chat_push(
        &self,
        ws: &mut WsStream,
        request: daemon_protocol::ActionRequestFrame,
    ) -> Result<()> {
        self.register_task(&request);
        if !self.gate.lock().await.allows("sync.chat.push") {
            return self
                .send_error(
                    ws,
                    request.id,
                    "capability not granted: sync.chat.push".into(),
                )
                .await;
        }
        let started = std::time::Instant::now();
        let (ok, error) = match parse_chat_push_params(&request.params) {
            Ok(conversations) => {
                let mut saved = 0usize;
                for conversation in &conversations {
                    saved += self
                        .chat_store
                        .upsert_conversation(conversation)
                        .map_err(|e| DaemonError::Protocol(format!("store_error: {e}")))?;
                }
                tracing::info!(
                    "[chat-sync] pushed {} conversations, {} messages saved",
                    conversations.len(),
                    saved
                );
                (true, None)
            }
            Err(error) => (false, Some(format!("bad_params: {error}"))),
        };
        self.send_action_result(ws, request.id, ok, None, error, elapsed_ms(started))
            .await
    }
}

fn task_kind(capability: &str) -> TaskKind {
    match capability {
        "desktop.shell.execute" => TaskKind::ShellExec,
        "desktop.fs.read" => TaskKind::FileRead,
        "desktop.fs.write" | "desktop.fs.delete" | "desktop.fs.patch" | "desktop.code.ast_edit" => {
            TaskKind::FileWrite
        }
        "desktop.code.ast_grep" => TaskKind::FileRead,
        "desktop.code.eval" | "desktop.lsp.rename" => TaskKind::ShellExec,
        "desktop.lsp.diagnostics" | "desktop.lsp.hover" | "desktop.lsp.definition" => {
            TaskKind::FileRead
        }
        "desktop.memory.remember" | "desktop.memory.recall" | "desktop.memory.forget"
        | "desktop.memory.reflect" | "desktop.memory.stats" => TaskKind::DbProxy,
        "desktop.git.overview" | "desktop.git.diff" => TaskKind::FileRead,
        "sync.chat.push" => TaskKind::DbProxy,
        other => TaskKind::Other(other.to_string()),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn elapsed_ms(started: std::time::Instant) -> u64 {
    started.elapsed().as_millis() as u64
}
fn contains_dangerous_shell(command: &str) -> bool {
    [
        "sudo ",
        "rm -rf",
        "del /f /s /q",
        "mkfs",
        "chmod -R 777",
        "chown -R",
    ]
    .iter()
    .any(|pattern| command.contains(pattern))
}
