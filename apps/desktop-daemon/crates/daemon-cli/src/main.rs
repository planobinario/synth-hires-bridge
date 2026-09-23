//! SynthHires Desktop Daemon — binary entry point.
//!
//! Lifecycle:
//!   1. Parse CLI args (clap) — agent-friendly subcommands
//!      (status/doctor/verify/logs/pair/unpair/stop) run standalone.
//!   2. Acquire single-instance lock (if already running, open local dashboard in browser).
//!   3. Load state from ~/.config/synthhires-bridge/state.json.
//!   4. If paired, read token from OS keyring, spawn WS client loop.
//!   5. Spawn local axum HTTP server on 127.0.0.1:7333 with CORS and HTML status dashboard.
//!   6. Open local dashboard in default browser on launch.
//!   7. Spawn system tray icon with menu (Status, Open Dashboard, Unpair, Quit).
//!   8. Block until Quit is selected from the tray menu.

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use clap::Parser;
use daemon_core::{
    capability::{CapabilityGate, ScopeSnapshot},
    fingerprint::DeviceFingerprint,
    keyring::TokenStore,
    ws_client::WsClient,
    DaemonError, Result,
};

mod console;
mod server;
mod tray;
mod ui;

#[derive(Parser, Debug)]
#[command(
    name = "synthhires-bridge",
    version,
    about = "Bridges your agents to your PC's filesystem and terminal.",
    subcommand_negates_reqs = true
)]
struct Cli {
    #[arg(long, env = "SYNTHHIRES_BACKEND_URL")]
    backend_url: Option<String>,

    #[arg(long, env = "SYNTHHIRES_LOCAL_PORT")]
    local_port: Option<u16>,

    #[arg(long)]
    config_dir: Option<PathBuf>,

    /// Run in headless mode without egui window.
    #[arg(long)]
    headless: bool,

    /// Agent-facing control subcommands. The daemon itself runs when NO
    /// subcommand is given (or via `run`).
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(clap::Subcommand, Debug)]
enum Cmd {
    /// Start the daemon in the foreground (same as no subcommand).
    Run,
    /// Full JSON status: pairing, WS health, keyring, version.
    Status {
        /// Raw JSON output (default: human-readable table).
        #[arg(long)]
        json: bool,
    },
    /// Diagnostic report: state file, keyring, HTTP endpoint, WS state.
    Doctor {
        /// Raw JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Empirically verify read+write access to a path on THIS machine.
    Verify {
        /// Absolute path to probe.
        path: PathBuf,
        /// Raw JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Tail the daemon's on-disk log.
    Logs {
        /// Number of lines (default 100).
        #[arg(long, default_value_t = 100)]
        lines: usize,
    },
    /// Pair the daemon with a backend using a pairing code.
    Pair {
        /// Backend origin (e.g. https://synthhires.com).
        backend: String,
        /// Pairing code shown by the web UI.
        code: String,
        /// Pairing id shown by the web UI (desktop mode).
        #[arg(long)]
        pairing_id: Option<String>,
    },
    /// Unpair the daemon (clears keyring + state).
    Unpair,
    /// Stop a running daemon via its local HTTP endpoint.
    Stop,
}

/// Default state directory. User builds keep per-user config (ProjectDirs);
/// system builds (`--features system-install`) use a shared, root-manageable
/// path, overridable via SYNTHHIRES_CONFIG_DIR for non-standard deployments.
fn default_config_dir() -> PathBuf {
    #[cfg(feature = "system-install")]
    {
        if let Ok(dir) = std::env::var("SYNTHHIRES_CONFIG_DIR") {
            if !dir.is_empty() {
                return PathBuf::from(dir);
            }
        }
        #[cfg(target_os = "windows")]
        return std::env::var("ProgramData")
            .map(|d| PathBuf::from(d).join("SynthHires").join("Bridge"))
            .unwrap_or_else(|_| PathBuf::from("C:\\ProgramData\\SynthHires\\Bridge"));
        #[cfg(not(target_os = "windows"))]
        return PathBuf::from("/var/lib/synthhires-bridge");
    }
    #[cfg(not(feature = "system-install"))]
    directories::ProjectDirs::from("com", "synthhires", "bridge")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| {
            dirs_next::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("synthhires-bridge")
        })
}

fn config_dir_of(cli: &Cli) -> PathBuf {
    cli.config_dir.clone().unwrap_or_else(default_config_dir)
}

const LOCAL_ORIGIN: &str = "http://localhost:4321";

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct DaemonState {
    device_id: Option<String>,
    scopes: daemon_protocol::Scopes,
    #[serde(default)]
    backend_url: Option<String>,
}

impl DaemonState {
    async fn load(config_dir: &std::path::Path) -> Result<Self> {
        let path = config_dir.join("state.json");
        if !path.exists() {
            return Ok(Self {
                device_id: None,
                scopes: daemon_protocol::Scopes::default(),
                backend_url: None,
            });
        }
        let raw = tokio::fs::read(&path).await.map_err(DaemonError::Io)?;
        Ok(serde_json::from_slice(&raw).unwrap_or(Self {
            device_id: None,
            scopes: daemon_protocol::Scopes::default(),
            backend_url: None,
        }))
    }

    async fn save(&self, config_dir: &std::path::Path) -> Result<()> {
        let path = config_dir.join("state.json");
        let raw = serde_json::to_vec_pretty(self).unwrap();
        tokio::fs::write(&path, raw)
            .await
            .map_err(DaemonError::Io)?;
        Ok(())
    }
}

pub enum UiCmd {
    Unpair,
    OpenDashboard,
}

#[derive(Clone)]
pub struct UiLogger {
    tx: std::sync::mpsc::SyncSender<String>,
}

impl std::io::Write for UiLogger {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let msg = String::from_utf8_lossy(buf).to_string();

        // Strip ANSI color codes
        let mut clean_msg = String::with_capacity(msg.len());
        let mut in_escape = false;
        for c in msg.chars() {
            if c == '\x1b' {
                in_escape = true;
            } else if in_escape {
                if c.is_ascii_alphabetic() {
                    in_escape = false;
                }
            } else {
                clean_msg.push(c);
            }
        }

        let _ = self.tx.try_send(clean_msg);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for UiLogger {
    type Writer = UiLogger;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn main() -> Result<()> {
    // Agent-friendly subcommands run BEFORE any daemon setup: they are
    // small, side-effect-free (except stop/pair/unpair) and print plain
    // JSON/table output with conventional exit codes.
    let cli = Cli::parse();
    if let Some(ref cmd) = cli.command {
        // `run` is an alias for the default daemon mode — fall through.
        if !matches!(cmd, Cmd::Run) {
            console::attach();
            return run_cli_command(cmd, &cli);
        }
    }

    let config_dir = config_dir_of(&cli);
    std::fs::create_dir_all(&config_dir).ok();

    let (log_tx, log_rx) = std::sync::mpsc::sync_channel(2000);
    let ui_logger = UiLogger { tx: log_tx };

    // Persist the log to disk so `synthhires-bridge logs` and agents
    // can debug without scraping stdout (which was a windowless void
    // before). Daily rotation, max 2 files, inside the config dir with
    // the `daemon.` prefix (files look like daemon.2026-08-16.log).
    let file_logger = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("daemon")
        .filename_suffix("log")
        .max_log_files(2)
        .build(&config_dir)
        .unwrap_or_else(|_| tracing_appender::rolling::never(&config_dir, "daemon-fallback.log"));

    use tracing_subscriber::fmt::writer::MakeWriterExt;
    let both = std::io::stdout.and(ui_logger).and(file_logger);

    // Start tracing early
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(
                    "synthhires_bridge=debug,daemon_core=debug,daemon_protocol=debug",
                )
            }),
        )
        .with_writer(both)
        .init();
    tracing::info!("Daemon starting, logs under {}", config_dir.display());

    // Local chat archive: single ChatStore instance shared by the WS
    // client (writes) and the UI's "Conversaciones" tab (reads/export).
    let chat_store = std::sync::Arc::new(
        daemon_core::ChatStore::open(&daemon_core::ChatStore::default_path()).unwrap_or_else(|e| {
            tracing::error!("[chat-store] failed to open local archive: {e}");
            // Chat sync must never take the daemon down: fall back
            // to an in-memory store so push ACKs still work.
            daemon_core::ChatStore::open(std::path::Path::new(":memory:"))
                .expect("in-memory chat store")
        }),
    );

    // Spawn Tokio in a separate background thread so it doesn't block the UI
    let (status_tx, status_rx) = tokio::sync::watch::channel("Iniciando...".to_string());
    let (tasks_tx, tasks_rx) = tokio::sync::watch::channel(Vec::new());
    let (kill_tx, kill_rx) = tokio::sync::mpsc::channel(100);
    let (ui_cmd_tx, ui_cmd_rx) = tokio::sync::mpsc::channel(10);
    let ui_cmd_tx_bg = ui_cmd_tx.clone();
    let chat_store_bg = chat_store.clone();

    let ui_ctx = Arc::new(tokio::sync::RwLock::new(None));
    let ui_ctx_clone = ui_ctx.clone();

    // Shared consent broker: the WS client raises prompts, the UI answers.
    let consent_broker = std::sync::Arc::new(daemon_core::ConsentBroker::new());
    let consent_broker_bg = consent_broker.clone();
    let consent_broker_ui = consent_broker.clone();

    // Shared WS health: updated by the client, exposed via /status + CLI.
    let ws_health = std::sync::Arc::new(daemon_core::WsHealth::new());
    let ws_health_bg = ws_health.clone();

    if cli.headless {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to build tokio runtime");

        if let Err(e) = rt.block_on(async move {
            background_daemon_task(
                status_tx,
                tasks_tx,
                kill_rx,
                ui_cmd_rx,
                ui_cmd_tx_bg,
                ui_ctx_clone,
                chat_store_bg,
                consent_broker_bg,
                ws_health_bg,
            )
            .await
        }) {
            tracing::error!("Headless daemon exited with error: {e}");
            return Err(e);
        }
        tracing::info!("Headless daemon exited cleanly");
        return Ok(());
    }

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to build tokio runtime");

        rt.block_on(async move {
            if let Err(e) = background_daemon_task(
                status_tx,
                tasks_tx,
                kill_rx,
                ui_cmd_rx,
                ui_cmd_tx_bg,
                ui_ctx_clone,
                chat_store_bg,
                consent_broker_bg,
                ws_health_bg,
            )
            .await
            {
                tracing::error!("Daemon fatal error: {e}");
            }
        });
    });

    let icon_data = {
        let img = image::load_from_memory(include_bytes!("../../../assets/icon.png"))
            .expect("Failed to load icon")
            .into_rgba8();
        let (width, height) = img.dimensions();
        let rgba = img.into_raw();
        eframe::egui::IconData {
            rgba,
            width,
            height,
        }
    };

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([700.0, 600.0])
            .with_icon(std::sync::Arc::new(icon_data))
            .with_title("SynthHires Bridge"),
        ..Default::default()
    };

    eframe::run_native(
        "synthhires-bridge",
        native_options,
        Box::new(move |cc| {
            let app = ui::BridgeApp::new(
                cc,
                status_rx,
                tasks_rx,
                kill_tx,
                ui_cmd_tx,
                log_rx,
                chat_store.clone(),
                consent_broker_ui,
                ws_health.clone(),
            );
            let mut w_ctx = ui_ctx.blocking_write();
            *w_ctx = Some(cc.egui_ctx.clone());
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| DaemonError::Io(std::io::Error::other(e.to_string())))?;

    Ok(())
}

async fn background_daemon_task(
    status_tx: tokio::sync::watch::Sender<String>,
    tasks_tx: tokio::sync::watch::Sender<Vec<daemon_core::task_registry::TaskState>>,
    mut kill_rx: tokio::sync::mpsc::Receiver<uuid::Uuid>,
    mut ui_cmd_rx: tokio::sync::mpsc::Receiver<UiCmd>,
    ui_cmd_tx: tokio::sync::mpsc::Sender<UiCmd>,
    ui_ctx: Arc<tokio::sync::RwLock<Option<eframe::egui::Context>>>,
    chat_store: std::sync::Arc<daemon_core::ChatStore>,
    consent: std::sync::Arc<daemon_core::ConsentBroker>,
    ws_health: std::sync::Arc<daemon_core::WsHealth>,
) -> Result<()> {
    let cli = Cli::parse();

    let config_dir = cli.config_dir.clone().unwrap_or_else(default_config_dir);
    std::fs::create_dir_all(&config_dir).ok();

    let local_port = cli.local_port.unwrap_or(7333);

    let web_url = std::env::var("SYNTHHIRES_WEB_URL")
        .unwrap_or_else(|_| "http://localhost:4321/space/runtimes".to_string());

    let last_poll = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let _lock = if !cli.headless {
        let l = single_instance::SingleInstance::new("synthhires-bridge");
        let deep_link = std::env::args().find(|a| a.starts_with("synthhires://"));

        if let Ok(ref instance) = l {
            if !instance.is_single() {
                tracing::info!("Bridge is already running.");
                // Send deep link via IPC to the running instance
                if let Some(link) = deep_link {
                    use interprocess::local_socket::prelude::*;
                    use std::io::Write;

                    let name = if cfg!(windows) {
                        "synthhires-bridge-ipc"
                            .to_ns_name::<interprocess::local_socket::GenericNamespaced>()
                            .unwrap()
                    } else {
                        "/tmp/synthhires-bridge-ipc.sock"
                            .to_fs_name::<interprocess::local_socket::GenericFilePath>()
                            .unwrap()
                    };

                    if let Ok(mut conn) = interprocess::local_socket::Stream::connect(name) {
                        let _ = conn.write_all(link.as_bytes());
                        // Wait for ACK
                        let mut buf = [0u8; 3];
                        use std::io::Read;
                        let _ = conn.read_exact(&mut buf);
                    }
                } else {
                    use interprocess::local_socket::prelude::*;
                    use std::io::Write;
                    let name = if cfg!(windows) {
                        "synthhires-bridge-ipc"
                            .to_ns_name::<interprocess::local_socket::GenericNamespaced>()
                            .unwrap()
                    } else {
                        "/tmp/synthhires-bridge-ipc.sock"
                            .to_fs_name::<interprocess::local_socket::GenericFilePath>()
                            .unwrap()
                    };
                    if let Ok(mut conn) = interprocess::local_socket::Stream::connect(name) {
                        let _ = conn.write_all(b"synthhires://ping-ui");
                        let mut buf = [0u8; 3];
                        use std::io::Read;
                        let _ = conn.read_exact(&mut buf);
                    } else {
                        tracing::warn!("IPC connection to running instance failed");
                    }
                }
                return Ok(());
            }
        }
        Some(l)
    } else {
        None
    };

    let state = Arc::new(RwLock::new(DaemonState::load(&config_dir).await?));

    let backend_url = cli
        .backend_url
        .unwrap_or_else(|| "wss://synthhires.com/api/devices/ws".to_string());

    // Determine pairing status
    let is_paired = {
        let s = state.read().await;
        s.device_id.is_some()
    };

    use daemon_core::task_registry::TaskRegistry;
    let task_registry = Arc::new(tokio::sync::RwLock::new(TaskRegistry::new(200)));

    let ws_handle: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    // Task Registry loop
    tokio::spawn({
        let tr = task_registry.clone();
        let t_tx = tasks_tx.clone();
        async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            loop {
                interval.tick().await;
                let mut reg = tr.write().await;
                reg.cleanup_stale_tasks(std::time::Duration::from_secs(5));
                // Extract states for UI
                let states: Vec<_> = reg.states().cloned().collect();
                let _ = t_tx.send(states);
            }
        }
    });

    // Kill Task loop
    tokio::spawn({
        let tr = task_registry.clone();
        async move {
            while let Some(id) = kill_rx.recv().await {
                let mut reg = tr.write().await;
                reg.kill_task(id).await;
            }
        }
    });

    // Ui Cmd loop
    tokio::spawn({
        let state_cmd = state.clone();
        let config_dir_cmd = config_dir.clone();
        let status_tx_cmd = status_tx.clone();
        let ws_handle_cmd = ws_handle.clone();
        let web_url_cmd = web_url.clone();
        async move {
            while let Some(cmd) = ui_cmd_rx.recv().await {
                match cmd {
                    UiCmd::Unpair => {
                        tracing::info!("Desvinculando dispositivo...");

                        // 1. Delete token from Keyring
                        let device_id = {
                            let s = state_cmd.read().await;
                            s.device_id.clone()
                        };
                        if let Some(id) = device_id {
                            let _ = daemon_core::keyring::TokenStore::delete(&id);
                        }

                        // 2. Clear state.json
                        {
                            let mut s = state_cmd.write().await;
                            s.device_id = None;
                            s.backend_url = None;
                            let _ = s.save(&config_dir_cmd).await;
                        }

                        // 3. Abort WS task
                        {
                            let mut handle = ws_handle_cmd.lock().await;
                            if let Some(h) = handle.take() {
                                h.abort();
                            }
                        }

                        // 4. Update status
                        let _ = status_tx_cmd.send("Esperando emparejamiento...".to_string());
                        tracing::info!("Dispositivo desvinculado con éxito.");
                    }
                    UiCmd::OpenDashboard => {
                        let _ = open::that(&web_url_cmd);
                    }
                }
            }
        }
    });

    // IPC Listener for deep links
    tokio::spawn({
        let config_dir_ipc = config_dir.clone();
        let backend_url_ipc = backend_url.clone();
        let state_ipc = state.clone();
        let ws_handle_ipc = ws_handle.clone();
        let chat_store_ipc = chat_store.clone();
        let consent_ipc = consent.clone();
        let health_ipc = ws_health.clone();

        async move {
            use interprocess::local_socket::prelude::*;
            use interprocess::local_socket::traits::tokio::Listener;
            use interprocess::local_socket::ListenerOptions;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let name = if cfg!(windows) {
                "synthhires-bridge-ipc"
                    .to_ns_name::<interprocess::local_socket::GenericNamespaced>()
                    .unwrap()
            } else {
                "/tmp/synthhires-bridge-ipc.sock"
                    .to_fs_name::<interprocess::local_socket::GenericFilePath>()
                    .unwrap()
            };

            #[cfg(unix)]
            {
                let _ = std::fs::remove_file("/tmp/synthhires-bridge-ipc.sock");
            }

            let mut options = ListenerOptions::new().name(name);

            #[cfg(windows)]
            {
                use interprocess::os::windows::local_socket::ListenerOptionsExt;
                use interprocess::os::windows::security_descriptor::AsSecurityDescriptorExt;
                use std::os::windows::ffi::OsStrExt;
                use std::ptr;
                use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;

                let sddl: Vec<u16> = std::ffi::OsStr::new("D:(A;;GA;;;OW)")
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect();
                let mut sd: *mut std::ffi::c_void = ptr::null_mut();

                unsafe {
                    if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        sddl.as_ptr(),
                        1, // SDDL_REVISION_1
                        &mut sd,
                        ptr::null_mut(),
                    ) != 0
                    {
                        use interprocess::os::windows::security_descriptor::BorrowedSecurityDescriptor;
                        let bsd = BorrowedSecurityDescriptor::from_ptr(sd);
                        if let Ok(owned_sd) = bsd.to_owned_sd() {
                            options = options.security_descriptor(owned_sd);
                        }
                    } else {
                        tracing::error!(
                            "Failed to set Windows ACL on Named Pipe, error: {}",
                            std::io::Error::last_os_error()
                        );
                    }
                }
            }

            #[cfg(unix)]
            {
                use interprocess::os::unix::local_socket::ListenerOptionsExt;
                options = options.mode(0o600);
            }

            if let Ok(listener) = options.create_tokio() {
                tracing::info!("IPC Listener bound successfully");
                loop {
                    if let Ok(mut stream) = listener.accept().await {
                        let state_clone = state_ipc.clone();
                        let backend_url_clone = backend_url_ipc.clone();
                        let config_dir_clone = config_dir_ipc.clone();
                        let ws_handle_clone = ws_handle_ipc.clone();
                        let chat_store_clone = chat_store_ipc.clone();
                        let consent_clone = consent_ipc.clone();
                        let health_clone = health_ipc.clone();

                        tokio::spawn(async move {
                            // Deep links carry URLs with UUIDs + encoded
                            // backends; 1024 bytes is not enough.
                            let mut buf = [0u8; 8192];
                            if let Ok(len) = stream.read(&mut buf).await {
                                let msg = String::from_utf8_lossy(&buf[..len]).to_string();
                                tracing::info!("IPC Received data length: {}", len);

                                if let Some(uri) = msg.strip_prefix("synthhires://") {
                                    let clean_uri = uri.trim_end_matches('/').trim();

                                    if clean_uri == "ping-ui" {
                                        // Auto-open disabled intentionally by user request
                                        let _ = stream.write_all(b"ACK").await;
                                        return;
                                    }

                                    // Parse `pair?code=...&pairing_id=...&backend=...`
                                    // The previous code did strip_prefix("pair?token=")
                                    // and stored the WHOLE query remainder
                                    // (including `&backend=...`) as the token —
                                    // WS hello then hashed garbage and auth
                                    // failed forever. Parse real query params.
                                    let parse_param = |key: &str| -> Option<String> {
                                        clean_uri.split('&').find_map(|part| {
                                            let (k, v) = part.split_once('=')?;
                                            if k.trim() == key {
                                                let raw = v.trim();
                                                let decoded = url_decode(raw);
                                                Some(decoded)
                                            } else {
                                                None
                                            }
                                        })
                                    };

                                    let code = parse_param("code").or_else(|| {
                                        // Legacy links used `pair?token=...`
                                        parse_param("token")
                                    });
                                    let pairing_id = parse_param("pairing_id");
                                    let backend = parse_param("backend")
                                        .unwrap_or_else(|| backend_url_clone.clone());

                                    let Some(code) = code else {
                                        tracing::warn!("Deep link without code/token; ignoring");
                                        let _ = stream.write_all(b"NACK").await;
                                        return;
                                    };

                                    tracing::info!(
                                        "Deep link pair: backend={} pairing_id={:?}",
                                        backend,
                                        pairing_id
                                    );

                                    // Complete the pairing SERVER-SIDE: the web
                                    // UI started a pairing code and gave it to
                                    // us; we must POST pair/complete to get the
                                    // real deviceId + token. Storing the code
                                    // itself as the token (previous behavior)
                                    // is wrong — the server issues the token.
                                    let client = reqwest::Client::builder()
                                        .timeout(std::time::Duration::from_secs(30))
                                        .build();
                                    let complete = match client {
                                        Ok(c) => {
                                            let flow = daemon_core::PairingFlow::new(&backend, &c);
                                            let fp = daemon_core::DeviceFingerprint::collect()
                                                .hash_hex();
                                            flow.complete(
                                                daemon_core::pairing::PairCompleteRequest {
                                                    code: code.clone(),
                                                    pairing_id,
                                                    device_kind: "desktop",
                                                    device_name: hostname(),
                                                    fingerprint: fp,
                                                    desired_scopes: vec![
                                                        "desktop.shell.execute".into(),
                                                        "desktop.fs.read".into(),
                                                        "desktop.fs.write".into(),
                                                        "desktop.fs.delete".into(),
                                                        "desktop.fs.verify".into(),
                                                        "desktop.fs.list".into(),
                                                        "sync.chat.push".into(),
                                                    ],
                                                },
                                            )
                                            .await
                                        }
                                        Err(e) => Err(daemon_core::DaemonError::Protocol(format!(
                                            "reqwest client: {e}"
                                        ))),
                                    };

                                    match complete {
                                        Ok(res) => {
                                            {
                                                let mut s = state_clone.write().await;
                                                s.device_id = Some(res.data.device_id.clone());
                                                s.backend_url = Some(backend.clone());
                                                let _ = s.save(&config_dir_clone).await;
                                            }

                                            // Start WS client with the REAL
                                            // backend + deviceId.
                                            let ws_state = state_clone.clone();
                                            let ws_backend = res.data.ws_url.clone();
                                            let ws_store = chat_store_clone.clone();
                                            let ws_consent = consent_clone.clone();
                                            let ws_health_clone = health_clone.clone();
                                            let mut hw = ws_handle_clone.lock().await;
                                            if let Some(h) = hw.take() {
                                                h.abort();
                                            }
                                            *hw = Some(tokio::spawn(async move {
                                                if let Err(e) = run_ws_client(
                                                    ws_state,
                                                    ws_backend,
                                                    ws_store,
                                                    ws_consent,
                                                    ws_health_clone,
                                                )
                                                .await
                                                {
                                                    tracing::error!("WS client died: {e}");
                                                }
                                            }));

                                            tracing::info!(
                                                "Paired device {}; WS client started. ACK.",
                                                res.data.device_id
                                            );
                                            let _ = stream.write_all(b"ACK").await;
                                        }
                                        Err(e) => {
                                            tracing::error!("pair/complete failed: {e}");
                                            let _ = stream.write_all(b"NACK").await;
                                        }
                                    }
                                } else {
                                    tracing::warn!("Received unrecognized IPC message");
                                }
                            }
                        });
                    }
                }
            } else {
                tracing::error!("Failed to bind IPC listener");
            }
        }
    });

    // Spawn WS client if already paired
    if is_paired {
        let ws_state = state.clone();
        let saved_url = {
            let s = state.read().await;
            s.backend_url.clone()
        };
        let ws_backend = saved_url.unwrap_or(backend_url.clone());
        let status_tx_clone = status_tx.clone();
        let ws_store = chat_store.clone();
        let ws_consent = consent.clone();
        let ws_health_clone = ws_health.clone();
        let mut hw = ws_handle.try_lock().expect("No lock contention at startup");
        *hw = Some(tokio::spawn(async move {
            let _ = status_tx_clone.send("Conectando al servidor...".to_string());
            if let Err(e) =
                run_ws_client(ws_state, ws_backend, ws_store, ws_consent, ws_health_clone).await
            {
                tracing::error!("WS client died: {e}");
                let _ = status_tx_clone.send(format!("Error de conexión: {e}"));
            }
        }));
    } else {
        let _ = status_tx.send("Esperando emparejamiento...".to_string());
    }

    let quit_rx = if !cli.headless {
        // build_tray runs INSIDE this tokio task; on Linux the appindicator
        // backend panics (GTK not initialized on this thread) and an uncaught
        // panic would kill the whole background task — dead daemon, live
        // window. catch_unwind contains it so the daemon survives trayless.
        let tray_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tray::build_tray(
                state.clone(),
                config_dir.clone(),
                local_port,
                ui_ctx.clone(),
                ui_cmd_tx.clone(),
            )
        }));
        match tray_res {
            Ok(Ok((_handle, rx))) => Some(rx),
            Ok(Err(err)) => {
                tracing::warn!("System tray not available ({err}); continuing without tray icon");
                None
            }
            Err(_) => {
                tracing::warn!(
                    "System tray panicked (Linux appindicator needs GTK on the main thread); continuing without tray icon"
                );
                None
            }
        }
    } else {
        None
    };
    tracing::info!("Daemon background tasks running.");

    // Spawn local HTTP server for zero-click pairing
    tokio::spawn({
        let server_state = server::ServerState {
            daemon_state: state.clone(),
            config_dir: config_dir.clone(),
            _backend_url: backend_url.clone(),
            pairing_nonce: Arc::new(tokio::sync::RwLock::new(None)),
            status_tx: status_tx.clone(),
            last_poll: last_poll.clone(),
            ws_handle: ws_handle.clone(),
            chat_store: chat_store.clone(),
            consent: consent.clone(),
            ws_health: ws_health.clone(),
        };
        async move {
            if let Err(e) = server::start_http_server(server_state, local_port).await {
                tracing::error!("Local HTTP server error: {e}");
            }
        }
    });

    // El estado de UI ya fue actualizado arriba (Conectando o Esperando emparejamiento)

    // Block the tokio thread until quit signal or Ctrl+C
    if let Some(rx) = quit_rx {
        match rx.await {
            Ok(()) => {
                tracing::info!("Received quit signal from tray menu");
            }
            Err(_) => {
                tracing::info!("Running daemon main loop (waiting for shutdown or process termination)...");
                std::future::pending::<()>().await;
            }
        }
    } else {
        tracing::info!("Running daemon main loop (headless/no tray)...");
        std::future::pending::<()>().await;
    }
    tracing::info!("tokio background daemon exiting");
    Ok(())
}

async fn run_ws_client(
    state: Arc<RwLock<DaemonState>>,
    backend_url: String,
    chat_store: std::sync::Arc<daemon_core::ChatStore>,
    consent: std::sync::Arc<daemon_core::ConsentBroker>,
    ws_health: std::sync::Arc<daemon_core::WsHealth>,
) -> Result<()> {
    let device_id = {
        let s = state.read().await;
        s.device_id
            .clone()
            .ok_or_else(|| DaemonError::Keyring("not paired".into()))?
    };

    let token = TokenStore::load(&device_id)
        .map_err(|e| DaemonError::Keyring(format!("token load: {e}")))?
        .ok_or_else(|| DaemonError::Keyring("token not found in keyring".into()))?;

    let fp = DeviceFingerprint::collect().hash_hex();

    let gate = CapabilityGate::new(ScopeSnapshot::default());
    let ws = WsClient::new(
        backend_url,
        token,
        device_id,
        fp,
        "desktop",
        hostname(),
        gate,
        chat_store,
        consent,
        ws_health,
    );
    ws.run().await
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

/// Minimal percent-decoder for deep-link query values (RFC 3986).
fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ── Agent-facing CLI subcommands ──────────────────────────────────────
//
// Every subcommand prints either plain JSON (--json) or a readable
// table, and exits non-zero on failure so scripts/agents can branch on
// `$?`. No daemon state is touched except where documented.

fn run_cli_command(cmd: &Cmd, cli: &Cli) -> Result<()> {
    let config_dir = config_dir_of(cli);
    match cmd {
        Cmd::Run => {
            // Unreachable via run_cli_command: `run` falls through to
            // the daemon body in main(). Kept for match completeness.
            std::process::exit(0);
        }
        Cmd::Status { json } => cmd_status(&config_dir, *json),
        Cmd::Doctor { json } => cmd_doctor(&config_dir, *json),
        Cmd::Verify { path, json } => cmd_verify(path.clone(), *json),
        Cmd::Logs { lines } => cmd_logs(&config_dir, *lines),
        Cmd::Pair {
            backend,
            code,
            pairing_id,
        } => cmd_pair(
            &config_dir,
            backend.clone(),
            code.clone(),
            pairing_id.clone(),
        ),
        Cmd::Unpair => cmd_unpair(&config_dir),
        Cmd::Stop => cmd_stop(),
    }
}

fn read_state(config_dir: &std::path::Path) -> DaemonState {
    let path = config_dir.join("state.json");
    let raw = std::fs::read(&path).unwrap_or_default();
    serde_json::from_slice(&raw).unwrap_or(DaemonState {
        device_id: None,
        scopes: daemon_protocol::Scopes::default(),
        backend_url: None,
    })
}

fn write_state(config_dir: &std::path::Path, state: &DaemonState) -> Result<()> {
    std::fs::create_dir_all(config_dir).ok();
    let raw = serde_json::to_vec_pretty(state).map_err(DaemonError::Json)?;
    std::fs::write(config_dir.join("state.json"), raw).map_err(DaemonError::Io)?;
    Ok(())
}

fn local_http_get(path: &str) -> Option<serde_json::Value> {
    let url = format!("http://127.0.0.1:7333{path}");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;
    let res = client
        .get(&url)
        .header("Origin", LOCAL_ORIGIN)
        .send()
        .ok()?;
    res.json().ok()
}

fn cmd_status(config_dir: &std::path::Path, json: bool) -> Result<()> {
    let state = read_state(config_dir);
    let remote = local_http_get("/status");
    let keyring_ok = match &state.device_id {
        Some(id) => TokenStore::load(id).map(|t| t.is_some()).unwrap_or(false),
        None => false,
    };

    let paired = state.device_id.is_some();
    let ws_connected = remote
        .as_ref()
        .and_then(|r| r.get("ws"))
        .and_then(|w| w.get("connected"))
        .and_then(|c| c.as_bool())
        .unwrap_or(false);
    let version = remote
        .as_ref()
        .and_then(|r| r.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
        .to_string();

    if json {
        let out = serde_json::json!({
            "version": version,
            "paired": paired,
            "deviceId": state.device_id,
            "keyringToken": keyring_ok,
            "backendUrl": state.backend_url,
            "wsConnected": ws_connected,
            "daemonHttp": remote.is_some(),
            "daemonRunning": remote.is_some(),
        });
        cprintln!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        cprintln!("synthhires-bridge v{version}");
        cprintln!(
            "  running      : {}",
            if remote.is_some() { "yes" } else { "no" }
        );
        cprintln!("  paired       : {}", if paired { "yes" } else { "no" });
        if let Some(id) = &state.device_id {
            cprintln!("  device_id    : {id}");
        }
        cprintln!(
            "  keyring token: {}",
            if keyring_ok { "present" } else { "missing" }
        );
        cprintln!(
            "  ws connected : {}",
            if ws_connected { "yes" } else { "no" }
        );
        if let Some(u) = &state.backend_url {
            cprintln!("  backend      : {u}");
        }
    }
    if paired && !ws_connected {
        std::process::exit(2);
    }
    if !remote.is_some() {
        std::process::exit(3);
    }
    Ok(())
}

fn cmd_doctor(config_dir: &std::path::Path, json: bool) -> Result<()> {
    let state_path = config_dir.join("state.json");
    let state = read_state(config_dir);
    let remote = local_http_get("/status");

    let mut checks: Vec<(String, bool, String)> = Vec::new();
    checks.push((
        "state.json exists".into(),
        state_path.exists(),
        state_path.display().to_string(),
    ));
    checks.push((
        "daemon HTTP (7333) reachable".into(),
        remote.is_some(),
        "http://127.0.0.1:7333/status".into(),
    ));
    let keyring_ok = match &state.device_id {
        Some(id) => TokenStore::load(id).map(|t| t.is_some()).unwrap_or(false),
        None => false,
    };
    checks.push(("keyring token present".into(), keyring_ok, "".into()));
    let ws_ok = remote
        .as_ref()
        .and_then(|r| r.get("ws"))
        .and_then(|w| w.get("connected"))
        .and_then(|c| c.as_bool())
        .unwrap_or(false);
    checks.push(("WS connected".into(), ws_ok, "".into()));

    if json {
        let out = serde_json::json!({
            "checks": checks.iter().map(|(n, ok, d)| serde_json::json!({
                "name": n, "ok": ok, "detail": d,
            })).collect::<Vec<_>>(),
            "state": {
                "paired": state.device_id.is_some(),
                "deviceId": state.device_id,
                "backendUrl": state.backend_url,
            },
            "ws": remote.as_ref().and_then(|r| r.get("ws")).cloned(),
        });
        cprintln!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        for (name, ok, detail) in &checks {
            let mark = if *ok { "[ok]" } else { "[FAIL]" };
            let extra = if detail.is_empty() {
                String::new()
            } else {
                format!(" — {detail}")
            };
            cprintln!("{mark} {name}{extra}");
        }
    }
    if checks.iter().any(|(_, ok, _)| !ok) {
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_verify(path: std::path::PathBuf, json: bool) -> Result<()> {
    // Direct empirical verification WITHOUT the WS roundtrip: the same
    // probe fs_ops::verify uses, executed here by the CLI itself.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| DaemonError::Io(std::io::Error::other(e.to_string())))?;
    let result = rt.block_on(async {
        let gate = CapabilityGate::new(ScopeSnapshot {
            capabilities: vec!["desktop.fs.verify".into()],
            always_allow_paths: vec![],
        });
        let ops = daemon_core::FsOps::new(&gate);
        ops.verify(daemon_core::fs_ops::FsVerifyRequest { path: path.clone() })
            .await
    });
    let verified = result.exists && result.readable && result.writable;
    if json {
        let out = serde_json::json!({
            "path": path.display().to_string(),
            "verified": verified,
            "exists": result.exists,
            "isDir": result.is_dir,
            "readable": result.readable,
            "writable": result.writable,
            "error": result.error,
        });
        cprintln!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        cprintln!("path     : {}", path.display());
        cprintln!("exists   : {}", if result.exists { "yes" } else { "NO" });
        cprintln!("is_dir   : {}", if result.is_dir { "yes" } else { "no" });
        cprintln!("readable : {}", if result.readable { "yes" } else { "NO" });
        cprintln!("writable : {}", if result.writable { "yes" } else { "NO" });
        if let Some(e) = &result.error {
            cprintln!("error    : {e}");
        }
    }
    if !verified {
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_logs(config_dir: &std::path::Path, lines: usize) -> Result<()> {
    // Find the newest daemon.<date>.log (the rolling appender writes
    // daily files named daemon.YYYY-MM-DD.log).
    let newest = std::fs::read_dir(config_dir)
        .ok()
        .and_then(|entries| {
            let mut files: Vec<_> = entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name().to_string_lossy().starts_with("daemon")
                        && e.path().extension().is_some_and(|x| x == "log")
                })
                .collect();
            files.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
            files.pop().map(|e| e.path())
        })
        .or_else(|| {
            let fallback = config_dir.join("daemon-fallback.log");
            fallback.exists().then_some(fallback)
        });
    let Some(log_path) = newest else {
        ceprintln!("no log files yet under {}", config_dir.display());
        std::process::exit(1);
    };
    let raw = std::fs::read_to_string(&log_path).map_err(DaemonError::Io)?;
    let tail: Vec<&str> = raw.lines().rev().take(lines).collect();
    for line in tail.iter().rev() {
        cprintln!("{line}");
    }
    Ok(())
}

fn cmd_pair(
    config_dir: &std::path::Path,
    backend: String,
    code: String,
    pairing_id: Option<String>,
) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| DaemonError::Io(std::io::Error::other(e.to_string())))?;
    rt.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| DaemonError::Protocol(format!("reqwest client: {e}")))?;
        let flow = daemon_core::PairingFlow::new(&backend, &client);
        let fp = DeviceFingerprint::collect().hash_hex();
        let res = flow
            .complete(daemon_core::pairing::PairCompleteRequest {
                code,
                pairing_id,
                device_kind: "desktop",
                device_name: hostname(),
                fingerprint: fp,
                desired_scopes: vec![
                    "desktop.shell.execute".into(),
                    "desktop.fs.read".into(),
                    "desktop.fs.write".into(),
                    "desktop.fs.delete".into(),
                    "desktop.fs.verify".into(),
                    "desktop.fs.list".into(),
                    "sync.chat.push".into(),
                ],
            })
            .await?;
        let state = DaemonState {
            device_id: Some(res.data.device_id.clone()),
            scopes: res.data.scopes.clone(),
            backend_url: Some(backend.clone()),
        };
        write_state(config_dir, &state)?;
        cprintln!("paired device: {}", res.data.device_id);
        cprintln!("ws url       : {}", res.data.ws_url);
        cprintln!("note         : restart the daemon to connect (or use `run`).");
        Ok(())
    })
}

fn cmd_unpair(config_dir: &std::path::Path) -> Result<()> {
    let state = read_state(config_dir);
    if let Some(id) = &state.device_id {
        let _ = TokenStore::delete(id);
    }
    write_state(
        config_dir,
        &DaemonState {
            device_id: None,
            scopes: daemon_protocol::Scopes::default(),
            backend_url: None,
        },
    )?;
    cprintln!("unpaired (keyring entry + state.json cleared)");
    Ok(())
}

fn cmd_stop() -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| DaemonError::Protocol(format!("reqwest client: {e}")))?;
    let res = client
        .post("http://127.0.0.1:7333/shutdown")
        .header("Origin", LOCAL_ORIGIN)
        .send()
        .map_err(|e| DaemonError::Protocol(format!("stop request: {e}")))?;
    if res.status().is_success() {
        cprintln!("shutdown requested");
        Ok(())
    } else {
        ceprintln!("daemon answered HTTP {} — is it running?", res.status());
        std::process::exit(1);
    }
}
