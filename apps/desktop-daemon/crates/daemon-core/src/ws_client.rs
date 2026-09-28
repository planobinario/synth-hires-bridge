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
/// Distinguishes the park marker from real errors inside dispatch_op!:
/// arms return heterogeneous error types (DaemonError, String,
/// BrowserError…), so a direct `Err(DaemonError::AwaitingConsent)`
/// pattern cannot compile against all of them.
trait ParkMarker {
    fn is_awaiting_consent(&self) -> bool {
        false
    }
}
impl ParkMarker for DaemonError {
    fn is_awaiting_consent(&self) -> bool {
        matches!(self, DaemonError::AwaitingConsent)
    }
}
impl ParkMarker for String {}
impl ParkMarker for crate::browser_ops::BrowserError {}

macro_rules! dispatch_op {
    ($self:ident, $ws:ident, $request:ident, $started:ident, $value:ident, $req:ident, $param_ty:ty, $run:block) => {{
        let parsed = serde_json::from_value::<$param_ty>($request.params.clone());
        match parsed {
            Ok($value) => {
                // Most arms don't need the request — the lint is silenced per
                // binding, not per arm, so every arm stays symmetric.
                #[allow(unused_variables)]
                let $req = &$request;
                // Reborrow the socket so arms that relay frames (web-first
                // consent) can use it without moving it out of the caller.
                let result = async {
                    let ws: &mut WsStream = &mut *$ws;
                    let _ = &ws; // silencia unused en brazos sin relay
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
                    // Parked for consent: NOT an error — no result frame is
                    // sent (the action stays pending server-side) and the
                    // AwaitingConsent marker bubbles up so handle_action
                    // parks the request. Non-DaemonError arms can never park.
                    Err(error) => {
                        if ParkMarker::is_awaiting_consent(&error) {
                            Err(DaemonError::AwaitingConsent)
                        } else {
                            $self.send_error($ws, $request.id.clone(), error.to_string()).await
                        }
                    }
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
    mcp: Arc<crate::mcp_ops::McpStore>,
    chat_store: Arc<ChatStore>,
    consent: Arc<ConsentBroker>,
    health: Arc<WsHealth>,
    /// Pre-image store for undo. None in tests/android; Some in production.
    checkpoints: Option<Arc<crate::checkpoint::CheckpointStore>>,
    /// Actions blocked on owner consent, parked OUTSIDE the read loop.
    /// The loop must never block on a human (it would stop reading
    /// heartbeats and consent_responses — a self-inflicted zombie).
    /// Keyed by action id; resumed when the owner answers (web-first
    /// relay frame handled by this same loop, or the local egui dialog).
    parked: Arc<
        tokio::sync::Mutex<
            std::collections::HashMap<String, (daemon_protocol::ActionRequestFrame, u64)>,
        >,
    >,
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
        // Undo/checkpoint store (docs/undo-checkpoint-spec.md): every fs
        // mutation by the agent gets a recoverable pre-image. Tests and the
        // Android JNI entry opt out via with_checkpoint_dir(None).
        let checkpoints = Self::checkpoint_store_from_env();
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
            mcp: Arc::new(crate::mcp_ops::McpStore::new()),
            memory: Arc::new(
                crate::memory_store::MemoryStore::open(crate::memory_store::MemoryStore::default_path())
                    .unwrap_or_else(|e| {
                        tracing::warn!("memory.db unavailable ({e}); using in-memory fallback");
                        crate::memory_store::MemoryStore::open_in_memory()
                            .expect("in-memory sqlite cannot fail to open")
                    }),
            ),
            chat_store,
            consent,
            health,
            checkpoints,
            parked: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Drop parked actions whose owner never answered within 2 minutes
    /// (mirrors the cockpit's 3-minute prompt age guard). Called from the
    /// heartbeat tick — the one code path that runs periodically.
    async fn sweep_expired_parked(&self) {
        let now = now_ms();
        let mut parked = self.parked.lock().await;
        let expired: Vec<String> = parked
            .iter()
            .filter(|(_, (_, at))| now.saturating_sub(*at) > 120_000)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            parked.remove(&id);
            self.consent.answer(
                &id,
                crate::consent::ConsentAnswer {
                    approved: false,
                    remember: false,
                },
            );
        }
    }

    /// Production wiring: point the undo store at the daemon's config dir.
    /// Returns Self for chaining right after `WsClient::new(...)`.
    pub fn with_checkpoint_dir(self, dir: Option<std::path::PathBuf>) -> Self {
        let checkpoints = dir.and_then(|d| {
            match crate::checkpoint::CheckpointStore::open(&d) {
                Ok(s) => Some(Arc::new(s)),
                Err(e) => {
                    tracing::warn!("checkpoint store unavailable ({e}); undo disabled");
                    None
                }
            }
        });
        Self { checkpoints, ..self }
    }

    /// Env-aware default store location: SYNTHHIRES_CHECKPOINTS=<dir> pins
    /// it, `off` disables, unset derives ProjectDirs(com/synthhires/bridge).
    fn checkpoint_store_from_env() -> Option<Arc<crate::checkpoint::CheckpointStore>> {
        match std::env::var("SYNTHHIRES_CHECKPOINTS") {
            Ok(v) if v == "off" => None,
            Ok(v) => Some(Self::open_store_or_fallback(std::path::PathBuf::from(v))),
            Err(_) => directories::ProjectDirs::from("com", "synthhires", "bridge")
                .map(|d| Self::open_store_or_fallback(d.config_dir().join("checkpoints"))),
        }
    }

    fn open_store_or_fallback(dir: std::path::PathBuf) -> Arc<crate::checkpoint::CheckpointStore> {
        match crate::checkpoint::CheckpointStore::open(&dir) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::warn!("checkpoint store unavailable at {} ({e}); temp-dir fallback", dir.display());
                let fallback = std::env::temp_dir().join("synthhires-checkpoints");
                Arc::new(
                    crate::checkpoint::CheckpointStore::open(&fallback)
                        .expect("writable temp dir is a platform invariant"),
                )
            }
        }
    }

    pub fn health(&self) -> Arc<WsHealth> {
        self.health.clone()
    }

    pub async fn run(&self) -> Result<()> {
        // Consent decisions (web relay handled in the loop below, or the
        // local egui dialog) land here; the connection loop resumes the
        // parked action.
        let (consent_tx, mut consent_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, crate::consent::ConsentAnswer)>();
        self.consent.set_notifier(consent_tx);
        let mut backoff = Duration::from_secs(1);
        loop {
            let connect_result = self
                .connect_once(Self::FALLBACK_HEARTBEAT_INTERVAL_MS, &mut consent_rx)
                .await;
            match connect_result {
                Ok(()) => {
                    self.health.mark_disconnected();
                    // connect_once returning Ok means the connection ended
                    // WITHOUT an error path (e.g. a clean Close frame). That
                    // is the network working as designed, not a failure to
                    // retreat from: keep the backoff at the floor for the
                    // next attempt. A zombie-detected disconnect arrives as
                    // Err and keeps the escalation.
                    backoff = Duration::from_secs(1);
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

    /// Server-dictated heartbeat cadence (hello_ack.heartbeatIntervalMs).
    /// Authority: bridge-do.ts publishes HEARTBEAT_INTERVAL_MS = 30s and
    /// acks every heartbeat. The watchdog needs a real answer to detect
    /// silence, so the fallback here is the same value the server uses
    /// today — one source of truth, no config drift.
    const FALLBACK_HEARTBEAT_INTERVAL_MS: u64 = 30_000;

    async fn connect_once(
        &self,
        heartbeat_interval_ms: u64,
        consent_rx: &mut tokio::sync::mpsc::UnboundedReceiver<
            (String, crate::consent::ConsentAnswer),
        >,
    ) -> Result<()> {
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
                "mcp".to_string(),
                "tools".to_string(),
            ],
            // One-shot manifest of tools already on the machine (CLI + LSP
            // servers + debug adapters). v1.1: serde-defaulted in the
            // protocol, so older webs ignore it and nothing breaks.
            tools: crate::tools_probe::detect_tools().await,
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
        // v1.2 (backward compatible): the server's own heartbeat cadence.
        // Older servers omit it -> serde default -> our fallback, which is
        // the value those servers use anyway.
        let heartbeat_interval_ms = if ack.heartbeat_interval_ms > 0 {
            ack.heartbeat_interval_ms
        } else {
            heartbeat_interval_ms
        };
        self.health.mark_connected();
        *self.gate.lock().await = CapabilityGate::new(ScopeSnapshot::from(&ack.scopes));
        let mut heartbeat = tokio::time::interval(Duration::from_millis(heartbeat_interval_ms));
        // Zombie watchdog (protocol v1.2): the old loop sent heartbeats and
        // never checked for acks, so a TCP connection whose server side was
        // gone (laptop suspend, NAT/DO eviction) sat "connected" for hours
        // while every dispatch returned pending. Now every beat must be
        // acked by the NEXT tick: at interval+grace the DO acks within RTT,
        // so a beat still unacked one full interval later means the path is
        // suspect. ONE miss is tolerated (late ack can straddle a GC/slow
        // link); TWO consecutive misses force the reconnect — ~60-70s from
        // failure at the 30s cadence, versus HOURS with the old loop.
        let mut expected_heartbeat: Option<u64> = None;
        let mut consecutive_misses: u32 = 0;
        loop {
            tokio::select! {
                Some(message) = ws.next() => {
                    match message.map_err(|e| DaemonError::Ws(format!("recv: {e}")))? {
                        Message::Text(text) => {
                            let frame: BridgeFrame = serde_json::from_str(&text)?;
                            if let BridgeFrame::HeartbeatAck(ref ack) = frame {
                                self.health.mark_heartbeat_ack(ack.t);
                                // Any ack proves the server runtime is alive:
                                // clear the pending deadline and the miss
                                // streak. (The DO acks from the same runtime
                                // that dispatches actions, so ack => alive.)
                                expected_heartbeat = None;
                                consecutive_misses = 0;
                            }
                            self.handle_frame(&mut ws, frame).await?;
                        }
                        Message::Close(_) => return Ok(()),
                        _ => {}
                    }
                }
                Some(decision) = consent_rx.recv() => {
                    // A parked action's owner decision arrived — web cockpit
                    // relay (a consent_response frame handled by this same
                    // loop) or the local egui dialog. Whoever answered went
                    // through ConsentBroker::answer(), which already drained
                    // the internal oneshot; resume WITHOUT the frame wait.
                    let (action_id, answer) = decision;
                    if let Some((parked, _at)) = self.parked.lock().await.remove(&action_id) {
                        if let Err(error) =
                            self.handle_action(&mut ws, parked, Some(answer)).await
                        {
                            if !matches!(error, DaemonError::AwaitingConsent) {
                                tracing::warn!("parked action {action_id} failed on resume: {error}");
                            }
                        }
                    }
                    continue;
                }
                _ = heartbeat.tick() => {
                    self.sweep_expired_parked().await;
                    // Only the OLDEST unacked beat can be judged: a tick
                    // burst (two ticks in the same millisecond happens when
                    // the runtime is descheduled under load and intervals
                    // fire catch-up) must not double-count as two misses —
                    // the acks may simply still be queued on the socket.
                    let burst = expected_heartbeat
                        .map(|t| now_ms().saturating_sub(t) < heartbeat_interval_ms)
                        .unwrap_or(false);
                    if let Some(t) = expected_heartbeat.take() {
                        if burst {
                            // Catch-up tick for the SAME beat (runtime was
                            // descheduled): re-arm it and skip this tick —
                            // sending another beat now would stack beats on
                            // the wire and miscount the next misses.
                            expected_heartbeat = Some(t);
                            continue;
                        } else {
                            // The beat is a full interval old and still
                            // unacked. One miss is not conclusive; the second
                            // CONSECUTIVE aged miss is a zombie verdict — no
                            // TCP error will ever surface on a half-dead
                            // socket, so we must surface it ourselves.
                            consecutive_misses = consecutive_misses.saturating_add(1);
                            self.health.mark_heartbeat_stale();
                            let decision = zombie_decision(true, consecutive_misses);
                            if decision.kill {
                                tracing::warn!("zombie watchdog: {decision:?}; forcing reconnect");
                                return Err(DaemonError::Ws(
                                    "heartbeat watchdog: no ack from server".into(),
                                ));
                            }
                            tracing::debug!("heartbeat t={t} unacked (miss {consecutive_misses}/2)");
                        }
                    }
                    let frame = BridgeFrame::Heartbeat(daemon_protocol::HeartbeatFrame { v: PROTOCOL_VERSION, t: now_ms() });
                    ws.send(Message::Text(serde_json::to_string(&frame)?))
                        .await
                        .map_err(|e| DaemonError::Ws(format!("send heartbeat: {e}")))?;
                    expected_heartbeat = Some(frame_heartbeat_t(&frame));
                }
            }
        }
    }

    async fn handle_frame(&self, ws: &mut WsStream, frame: BridgeFrame) -> Result<()> {
        match frame {
            BridgeFrame::ActionRequest(request) if request.capability == "sync.chat.push" => {
                self.handle_chat_push(ws, request).await
            }
            BridgeFrame::ActionRequest(request) => self.handle_action(ws, request, None).await,
            BridgeFrame::ScopeUpdate(update) => {
                *self.gate.lock().await = CapabilityGate::new(ScopeSnapshot::from(&update.scopes));
                Ok(())
            }
            BridgeFrame::ConsentResponse(response) => {
                // Web-first consent: the owner answered from the cockpit.
                // The DO relays it here; resolve the broker so whichever
                // surface answers first (web or local egui) decides.
                let answered = self.consent.answer(
                    &response.id,
                    crate::consent::ConsentAnswer {
                        approved: response.approved,
                        remember: response.remember,
                    },
                );
                if answered {
                    // answer() pushed the decision to the notifier; the
                    // select! in connect_once resumes the parked action.
                    tracing::debug!("consent {} resuelto desde la web; reanudando", response.id);
                } else {
                    tracing::debug!("consent {} ya resuelto localmente; respuesta web descartada", response.id);
                }
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

    /// `decision` is Some when a parked action is being RESUMED after its
    /// owner answered (web-first relay or local egui). None on first dispatch.
    async fn handle_action(
        &self,
        ws: &mut WsStream,
        request: daemon_protocol::ActionRequestFrame,
        decision: Option<crate::consent::ConsentAnswer>,
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
            // Tool manifest: versions of user-installed CLIs/LSP/DAP tools.
            // A read of the environment, nothing executable is run by THIS op
            // (the one-shot --version probes run once per process at hello).
            "desktop.git.overview" | "desktop.git.diff" => "desktop.fs.read",
            "desktop.tools.manifest" => "desktop.fs.read",
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
            | "desktop.browser.close" | "desktop.browser.eval" => "desktop.fs.read",
            // Multi-tab lifecycle rides nav's network tier; listing tabs is a
            // read. Route mocks intercept network traffic -> network tier.
            "desktop.browser.tab_open" | "desktop.browser.tab_select" | "desktop.browser.tab_close"
            | "desktop.browser.mock_set" => "desktop.network.fetch",
            "desktop.browser.tab_list" => "desktop.fs.read",
            // Debugger: start/eval/flow-control are as powerful as a shell;
            // inspection ops (breakpoints, stack, variables, threads) are reads.
            // MCP: status is a read; spawning/listing/calling a server is as
            // powerful as a shell; stopping one is the kill tier.
            "desktop.mcp.op" => match request.params.get("op").and_then(|v| v.as_str()) {
                Some("status") => "desktop.fs.read",
                Some("stop") => "desktop.process.kill",
                _ => "desktop.shell.execute",
            }
            "desktop.debug.op" => match request.params.get("op").and_then(|v| v.as_str()) {
                Some("start") | Some("eval") | Some("continue") | Some("step") | Some("pause") => {
                    "desktop.shell.execute"
                }
                _ => "desktop.fs.read",
            }
            "desktop.checkpoint.list" => "desktop.fs.read",
            // The restore arm performs its OWN two-path authorization
            // (explicit grant OR one-shot interactive consent — a server-side
            // skip_consent_prompt flag must never widen what the user can
            // undo on their own disk), so the generic pre-check is skipped.
            "desktop.fs.restore" => "self",
            other => other,
        };
        if required_scope != "self" && !self.gate.lock().await.allows(required_scope) {
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
        // Captured BEFORE the match: several arms move request.id (partial
        // move), and the park block below needs both afterwards.
        let action_id = request.id.clone();
        let parked_frame = request.clone();
        let outcome: Result<()> = match request.capability.as_str() {
            "desktop.fs.read" => {
                dispatch_op!(self, ws, request, started, value, req, crate::fs_ops::FsReadRequest, {
                    let annotate = value.annotate.unwrap_or(false);
                    let gate = self
                        .path_gate(ws, req, "desktop.fs.read", &value.path, decision)
                        .await?;
                    crate::fs_ops::FsOps::new(&gate)
                        .read_annotated(value, annotate)
                        .await
                        .map(|r| {
                            serde_json::json!({
                                "content_base64": r.content_base64,
                                "size": r.size,
                                // Honest signal: the file is bigger than the
                                // bounded read returned (dropped before).
                                "truncated": r.truncated,
                            })
                        })
                })
            }
            "desktop.fs.patch" => {
                let parsed =
                    serde_json::from_value::<crate::fs_ops::FsPatchRequest>(request.params.clone());
                match parsed {
                    Ok(value) => match self
                        .path_gate(ws, &request, "desktop.fs.write", &value.path, decision)
                        .await
                    {
                        Ok(gate) => match self.checkpoint_before(&request, &value.path).await {
                            Ok(()) => match crate::fs_ops::FsOps::new(&gate).patch(value).await {
                                Ok(result) => {
                                    self.send_action_result(
                                        ws,
                                        request.id,
                                        result.verified,
                                        Some(serde_json::to_value(&result)?),
                                        (!result.verified)
                                            .then_some("patch read-back verification failed".into()),
                                        elapsed_ms(started),
                                    )
                                    .await
                                }
                                Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                            },
                            Err(error) => self.send_error(ws, request.id, error).await,
                        },
                        // Park marker passes through: the request gets parked
                        // (no result frame), the select loop resumes it when the
                        // owner answers (web cockpit or local egui).
                        Err(DaemonError::AwaitingConsent) => Err(DaemonError::AwaitingConsent),
                        Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                    },
                    Err(error) => self.send_error(ws, request.id, format!("bad params: {error}")).await,
                }
            }
            "desktop.fs.write" => {
                let parsed =
                    serde_json::from_value::<crate::fs_ops::FsWriteRequest>(request.params.clone());
                match parsed {
                    Ok(value) => match self
                        .path_gate(ws, &request, "desktop.fs.write", &value.path, decision)
                        .await
                    {
                        Ok(gate) => match self.checkpoint_before(&request, &value.path).await {
                            Ok(()) => match crate::fs_ops::FsOps::new(&gate).write(value).await {
                                Ok(result) => self.send_action_result(ws, request.id, result.verified, Some(serde_json::json!({"bytes_written": result.bytes_written, "verified": result.verified})), (!result.verified).then_some("write read-back verification failed".into()), elapsed_ms(started)).await,
                                Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                            },
                            Err(error) => self.send_error(ws, request.id, error).await,
                        },
                        // Park marker passes through: the request gets parked
                        // (no result frame), the select loop resumes it when the
                        // owner answers (web cockpit or local egui).
                        Err(DaemonError::AwaitingConsent) => Err(DaemonError::AwaitingConsent),
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
                        match self.checkpoint_before(&request, &value.path).await {
                            Ok(()) => match crate::ast_ops::AstOps::new(&gate).edit(value).await {
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
                            },
                            Err(error) => self.send_error(ws, request.id, error).await,
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
                        // Park marker passes through: the request gets parked
                        // (no result frame), the select loop resumes it when the
                        // owner answers (web cockpit or local egui).
                        Err(DaemonError::AwaitingConsent) => Err(DaemonError::AwaitingConsent),
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
            "desktop.browser.eval" => {
                dispatch_op!(self, ws, request, started, value, req, crate::browser_ops::BrowserEvalParams, {
                    self.browser.eval(value).await
                })
 }
            "desktop.debug.op" => {
                dispatch_op!(self, ws, request, started, value, req, crate::dap_ops::DapOp, {
                    self.dap.execute(value).await
                })
            }
            "desktop.mcp.op" => {
                dispatch_op!(self, ws, request, started, value, req, crate::mcp_ops::McpOp, {
                    value.execute(&self.mcp).await
                })
            }
            "desktop.browser.close" => {
                dispatch_op!(self, ws, request, started, value, req, crate::browser_ops::BrowserCloseParams, {
                    self.browser.close(value.kill).await
                })
            }
            "desktop.browser.tab_open" => {
                dispatch_op!(self, ws, request, started, value, req, crate::browser_ops::TabOpenParams, {
                    self.browser.tab_open(value).await
                })
            }
            "desktop.browser.tab_select" => {
                dispatch_op!(self, ws, request, started, value, req, crate::browser_ops::TabSelectParams, {
                    self.browser.tab_select(value).await
                })
            }
            "desktop.browser.tab_close" => {
                dispatch_op!(self, ws, request, started, value, req, crate::browser_ops::TabCloseParams, {
                    self.browser.tab_close(value).await
                })
            }
            "desktop.browser.tab_list" => {
                dispatch_op!(self, ws, request, started, value, req, serde_json::Value, {
                    let _ = value;
                    self.browser.tab_list().await
                })
            }
            "desktop.browser.mock_set" => {
                dispatch_op!(self, ws, request, started, value, req, crate::browser_ops::MockSetParams, {
                    self.browser.mock_set(value).await
                })
            }
            "desktop.git.overview" => {
                dispatch_op!(self, ws, request, started, value, req, crate::git_ops::GitOverviewRequest, {
                    crate::git_ops::GitOps::new(&*self.gate.lock().await)
                        .overview(value)
                        .await
                })
            }
            "desktop.tools.manifest" => {
                dispatch_op!(self, ws, request, started, value, req, serde_json::Value, {
                    let _ = value;
                    Ok::<serde_json::Value, DaemonError>(serde_json::json!({
                        "tools": crate::tools_probe::detect_tools().await,
                    }))
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
                        .path_gate(ws, &request, "desktop.fs.delete", &value.path, decision)
                        .await
                    {
                        Ok(gate) => match self.checkpoint_before(&request, &value.path).await {
                            Ok(()) => match crate::fs_ops::FsOps::new(&gate).delete(value).await {
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
                            Err(error) => self.send_error(ws, request.id, error).await,
                        },
                        Err(error) => self.send_error(ws, request.id, error.to_string()).await,
                    },
                    Err(error) => {
                        self.send_error(ws, request.id, format!("bad params: {error}"))
                            .await
                    }
                }
            }
            "desktop.fs.restore" => {
                // Undo: NO entry in any pairing preset. Two legitimate paths:
                // (a) the owner granted desktop.fs.restore explicitly, or
                // (b) a one-shot interactive consent — which skip_consent_prompt
                // must NEVER substitute (a server-side flag cannot widen
                // what the user can undo on their own disk).
                let parsed = serde_json::from_value::<serde_json::Value>(request.params.clone());
                match parsed {
                    Ok(value) => {
                        let ts = value.get("ts").and_then(|v| v.as_u64()).unwrap_or(0);
                        if ts == 0 {
                            return self
                                .send_error(ws, request.id, "bad params: ts (ms) requerido".into())
                                .await;
                        } else if self.checkpoints.is_none() {
                            return self
                                .send_error(ws, request.id, "checkpoint store unavailable".into())
                                .await;
                        } else {
                            let store = self.checkpoints.as_ref().unwrap();
                            let entry = store.list().into_iter().find(|e| e.ts == ts);
                            let Some(entry) = entry else {
                                return self
                                    .send_error(
                                        ws,
                                        request.id,
                                        format!("checkpoint {ts} no existe (¿evicted?)"),
                                    )
                                    .await;
                            };
                            // Resolve the authorization decision:
                            //   granted → auto-approve (explicit owner grant);
                            //   resumed → the owner already answered (parked action);
                            //   otherwise → one-shot consent: relay to the web
                            //   cockpit (FIRST) + local egui fallback, then PARK.
                            //   skip_consent_prompt must NEVER auto-run a restore
                            //   (a server-side flag cannot widen what the user
                            //   can undo on their own disk).
                            let granted = self.gate.lock().await.allows("desktop.fs.restore");
                            let resolved = if granted {
                                Some(crate::consent::ConsentAnswer {
                                    approved: true,
                                    remember: false,
                                })
                            } else {
                                decision
                            };
                            let resolved = match resolved {
                                Some(answer) => Some(answer),
                                None => {
                                    let relay = daemon_protocol::ConsentPromptFrame {
                                        v: PROTOCOL_VERSION,
                                        id: request.id.clone(),
                                        capability: "desktop.fs.restore".into(),
                                        summary: format!("Restaurar {} desde checkpoint", entry.path),
                                        params_hash: String::new(),
                                        path: Some(entry.path.clone()),
                                    };
                                    if let Ok(frame) = serde_json::to_string(
                                        &daemon_protocol::BridgeFrame::ConsentPrompt(relay),
                                    ) {
                                        let _ = ws.send(Message::Text(frame)).await;
                                    }
                                    self.consent.ask(crate::consent::ConsentPrompt {
                                        action_id: request.id.clone(),
                                        capability: "desktop.fs.restore".into(),
                                        summary: format!("Restaurar {} desde checkpoint", entry.path),
                                        path: Some(entry.path.clone()),
                                    });
                                    None // park: AwaitingConsent bubbles below
                                }
                            };
                            match resolved {
                                None => Err(DaemonError::AwaitingConsent),
                                Some(answer) if !answer.approved => {
                                    self.send_error(
                                        ws,
                                        request.id,
                                        "capability_denied: desktop.fs.restore (denegado por el owner)".into(),
                                    )
                                    .await
                                }
                                Some(_) => match store.restore(ts) {
                                    Ok(rollback) => {
                                        self.send_action_result(
                                            ws,
                                            request.id,
                                            true,
                                            Some(serde_json::json!({
                                                "restored": true,
                                                "ts": ts,
                                                "path": entry.path,
                                                "rollback_ts": rollback.ts,
                                            })),
                                            None,
                                            elapsed_ms(started),
                                        )
                                        .await
                                    }
                                    Err(e) => {
                                        self.send_error(ws, request.id, format!("restore failed: {e}"))
                                            .await
                                    }
                                },
                            }
                        }
                    }
                    Err(error) => self.send_error(ws, request.id, format!("bad params: {error}")).await,
                }
            }
            "desktop.checkpoint.list" => {
                // Undo cockpit (web-first): read-only index for the owner's
                // control surface. Scope: desktop.fs.read — same tier as the
                // data it exposes (paths + sizes, never contents).
                dispatch_op!(self, ws, request, started, value, req, serde_json::Value, {
                    let _ = value;
                    let gate = self
                        .path_gate(ws, req, "desktop.fs.read", std::path::Path::new(""), decision)
                        .await?;
                    let _ = gate;
                    match self.checkpoints.as_ref() {
                        Some(store) => {
                            let entries: Vec<serde_json::Value> = store
                                .list()
                                .into_iter()
                                .take(200)
                                .map(|e| serde_json::to_value(&e).unwrap_or(serde_json::Value::Null))
                                .collect();
                            Ok::<_, DaemonError>(serde_json::json!({ "entries": entries }))
                        }
                        None => Ok::<_, DaemonError>(serde_json::json!({ "entries": [] })),
                    }
                })
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
                        .path_gate(ws, &request, "desktop.fs.watch", &value.path, decision)
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
                        // Park marker passes through (same contract as the
                        // other manual fs arms above).
                        Err(DaemonError::AwaitingConsent) => Err(DaemonError::AwaitingConsent),
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
                                        // `output_truncated` MUST reach the web: it is
                                        // the honest signal that stdout/stderr is
                                        // INCOMPLETE (hard-capped at 4 MiB/stream).
                                        let error = if result.output_truncated {
                                            (!ok)
                                                .then_some(format!(
                                                    "exit code {:?} (output truncated)",
                                                    result.exit_code
                                                ))
                                                .or_else(|| Some("output truncated".into()))
                                        } else {
                                            (!ok).then_some(format!("exit code {:?}", result.exit_code))
                                        };
                                        self.send_action_result(ws, request.id, ok, Some(serde_json::json!({"exit_code": result.exit_code, "stdout": result.stdout, "stderr": result.stderr, "output_truncated": result.output_truncated})), error, result.duration_ms).await
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
        };
        // The action needed the owner's decision: park it here (NOT in the
        // read loop) and leave it pending server-side. AwaitingConsent is
        // the marker — every other error path already sent its result frame.
        if matches!(outcome, Err(DaemonError::AwaitingConsent)) {
            self.parked
                .lock()
                .await
                .insert(action_id.clone(), (parked_frame, now_ms()));
            tracing::debug!("action {action_id} aparcada esperando consentimiento del owner");
        }
        Ok(())
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

    /// Capture the recoverable pre-image for a mutating fs action. Called
    /// AFTER the gate authorizes and BEFORE the mutation runs — the pre-image
    /// is the only copy of the user's data if the write goes wrong.
    /// Contract (spec): a capture failure REFUSES the mutation — proceeding
    /// without a safety net silently recreates the status quo. Truncated
    /// captures (>32 MiB files, dirs) still record intent metadata.
    async fn checkpoint_before(
        &self,
        request: &daemon_protocol::ActionRequestFrame,
        path: &std::path::Path,
    ) -> std::result::Result<(), String> {
        if let Some(store) = self.checkpoints.as_ref() {
            store
                .capture(&request.id, &request.capability, path)
                .map(|entry| {
                    tracing::debug!(
                        "checkpoint: {} {} existed={} size={} truncated={}",
                        entry.capability,
                        entry.path,
                        entry.existed,
                        entry.size,
                        entry.truncated
                    );
                })
                .map_err(|e| format!("checkpoint failed (mutation refused): {e}"))
        } else {
            Ok(())
        }
    }

    /// Resolve the gate for a path-scoped action.
    ///
    /// Capability deny is final (scope-level, no dialog can widen it).
    /// Path-level RequireConsent previously died as a hard error even when
    /// the egui UI was open — the ConsentBroker existed but nobody called
    /// `ask()`, so real users saw every out-of-workspace action fail. Now:
    /// the broker raises the prompt, the UI shows it, and the action waits
    /// (bounded) for the user's answer. Server pre-approved actions
    /// (`skip_consent_prompt`, validated against the owner's DB grants)
    /// skip the dialog by design — that flag IS the consent.
    async fn path_gate(
        &self,
        ws: &mut WsStream,
        request: &daemon_protocol::ActionRequestFrame,
        capability: &str,
        path: &std::path::Path,
        decision: Option<crate::consent::ConsentAnswer>,
    ) -> Result<CapabilityGate> {
        if !self.gate.lock().await.allows(capability) {
            return Err(DaemonError::CapabilityDenied(capability.into()));
        }
        // Server-side consent: the action route already validated the target
        // against the device's alwaysAllowPaths in the owner's DB. Trust it.
        if request.skip_consent_prompt {
            return Ok(self
                .gate
                .lock()
                .await
                .with_additional_path(path.to_path_buf()));
        }
        // A previous "remember" may have persisted this path into the local
        // gate (or the owner granted it live via scope_update). Without this
        // check, a remembered path would prompt AGAIN on every action —
        // remember=true would be a lie.
        if matches!(
            self.gate.lock().await.check_path_real(capability, path),
            Ok(crate::capability::GateDecision::Allow)
        ) {
            return Ok(self.gate.lock().await.clone());
        }
        // Pre-approved resume (the parked action's owner decision already
        // arrived — web cockpit or local egui; whoever answered first won).
        if let Some(answer) = decision {
            if answer.approved {
                if answer.remember {
                    // Persist for future actions: the base gate now trusts
                    // this path without asking again.
                    let mut gate = self.gate.lock().await;
                    *gate = gate.with_additional_path(path.to_path_buf());
                }
                // One-shot authorization ALWAYS includes the path (FsOps
                // re-checks the gate): the owner approved THIS exact path,
                // so the action must be executable now. remember=false only
                // means it is NOT persisted for future actions.
                return Ok(self
                    .gate
                    .lock()
                    .await
                    .clone()
                    .with_one_shot_path(path.to_path_buf()));
            }
            return Err(DaemonError::CapabilityDenied(format!(
                "consentimiento denegado por el usuario para {capability} sobre {}",
                path.display()
            )));
        }
        // Out-of-workspace: ask the human. WEB-FIRST: the prompt is relayed
        // to the owner's cockpit (DO stores it, lists it at /consents, and
        // relays the answer back as consent_response → broker → notifier →
        // this connection's select loop, which resumes the parked action).
        // The local egui dialog remains a fallback surface; FIRST answer
        // wins, the loser is discarded by the broker.
        let relay = daemon_protocol::ConsentPromptFrame {
            v: PROTOCOL_VERSION,
            id: request.id.clone(),
            capability: capability.to_string(),
            summary: format!("{capability} sobre {}", path.display()),
            params_hash: String::new(),
            path: Some(path.display().to_string()),
        };
        let frame = serde_json::to_string(&daemon_protocol::BridgeFrame::ConsentPrompt(relay))
            .unwrap_or_default();
        if !frame.is_empty() {
            // Best-effort: a dead socket means the DO also lost the action
            // polling; the local dialog still decides. The prompt is also
            // registered in the broker so the egui UI can answer it.
            let _ = ws.send(Message::Text(frame)).await;
        }
        self.consent.ask(crate::consent::ConsentPrompt {
            action_id: request.id.clone(),
            capability: capability.to_string(),
            summary: format!("{capability} sobre {}", path.display()),
            path: Some(path.display().to_string()),
        });
        // CRITICAL: never block this loop on a human. Park the request and
        // bubble AwaitingConsent; the caller stores it in self.parked and
        // the select! in connect_once resumes it when the decision lands
        // (answer() fires the notifier either way — web or local UI).
        Err(DaemonError::AwaitingConsent)
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
        "desktop.fs.write" | "desktop.fs.delete" | "desktop.fs.patch" | "desktop.code.ast_edit"
        | "desktop.fs.restore" => {
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

/// Pure zombie-watchdog verdict, extracted so the kill policy is unit-test
/// time constant even though it fires inside a tokio select loop.
///
/// Contract (2 consecutive unacked heartbeats => zombie): with a 30s server
/// interval, detection lands ~60-70s after the path dies — under one old
/// heartbeat period of added latency, versus HOURS with the old loop that
/// never checked acks at all.
///
/// `connected` is already known true (we are inside the event loop); it is
/// threaded through so tests cover the full predicate, not a fragment.
fn zombie_decision(connected: bool, consecutive_misses: u32) -> ZombieDecision {
    ZombieDecision {
        connected,
        consecutive_misses,
        kill: connected && consecutive_misses >= 2,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ZombieDecision {
    connected: bool,
    consecutive_misses: u32,
    kill: bool,
}

/// t of a Heartbeat frame we just built (kept in one place so the select
/// loop never desyncs from the frame construction).
fn frame_heartbeat_t(frame: &BridgeFrame) -> u64 {
    match frame {
        BridgeFrame::Heartbeat(h) => h.t,
        _ => 0,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zombie_requires_two_consecutive_misses() {
        // Healthy: acks keep coming.
        assert_eq!(
            zombie_decision(true, 0),
            ZombieDecision { connected: true, consecutive_misses: 0, kill: false }
        );
        // ONE miss: late ack / GC pause / slow link — never kill.
        assert_eq!(
            zombie_decision(true, 1),
            ZombieDecision { connected: true, consecutive_misses: 1, kill: false }
        );
        // TWO consecutive misses: the path is dead (zombie verdict).
        assert_eq!(
            zombie_decision(true, 2),
            ZombieDecision { connected: true, consecutive_misses: 2, kill: true }
        );
        // More misses keep the verdict (no un-kill).
        assert_eq!(
            zombie_decision(true, 5),
            ZombieDecision { connected: true, consecutive_misses: 5, kill: true }
        );
        // Never kill when not connected (defensive).
        assert_eq!(
            zombie_decision(false, 9),
            ZombieDecision { connected: false, consecutive_misses: 9, kill: false }
        );
    }

    #[test]
    fn zombie_uses_server_interval_not_hardcoded() {
        // The watchdog timing is derived from hello_ack.heartbeatIntervalMs;
        // this pins the contract that the interval flows from the server.
        // FALLBACK_HEARTBEAT_INTERVAL_MS must match the web's published
        // HEARTBEAT_INTERVAL_MS (30s) so a legacy server and a v1.2 server
        // produce identical cadences.
        assert_eq!(WsClient::FALLBACK_HEARTBEAT_INTERVAL_MS, 30_000);
    }

    #[test]
    fn frame_heartbeat_t_extracts_t() {
        let frame = BridgeFrame::Heartbeat(daemon_protocol::HeartbeatFrame { v: PROTOCOL_VERSION, t: 1727400000123 });
        assert_eq!(frame_heartbeat_t(&frame), 1727400000123);
    }
}
