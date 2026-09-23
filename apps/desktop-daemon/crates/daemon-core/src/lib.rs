//! SynthHires Desktop Daemon — core library.
//!
//! Modules:
//!   • `capability` — local enforcement of the device's granted scope.
//!   • `shell`      — executes commands with native process boundaries.
//!   • `fs_ops`     — read/write/delete/list/verify with allow-list checks.
//!   • `system_ops` — bounded process, HTTP, and filesystem-watch actions.
//!   • `keyring`    — OS secure storage wrapper.
//!   • `fingerprint` — host + OS + machine-id hash.

pub mod ast_ops;
pub mod audit;
// Always compiled: real impl behind the `browser` feature, API-preserving stubs
// without it (ws_client dispatches uniformly either way).
pub mod browser_ops;
pub mod dap_ops;
pub mod autoupdate;
pub mod capability;
pub mod eval_session;
pub mod chat_store;
pub mod consent;
pub mod fingerprint;
pub mod fs_ops;
pub mod git_ops;
pub mod health;
pub mod jni_android;
pub mod lsp_ops;
pub mod memory_store;
pub mod keyring;
pub mod os_info;
pub mod pairing;
pub mod proc_ops;
pub mod pty_ops;
pub mod shell;
pub mod system_ops;
pub mod task_registry;
pub mod ws_client;

pub use audit::AuditLog;
pub use autoupdate::{check_for_update, download_and_verify, SignedManifest, UpdateStatus};
pub use capability::{CapabilityGate, GateDecision};
pub use chat_store::ChatStore;
pub use consent::{ConsentAnswer, ConsentBroker, ConsentPrompt};
pub use fingerprint::DeviceFingerprint;
pub use fs_ops::FsOps;
pub use health::{WsHealth, WsHealthSnapshot};
pub use keyring::TokenStore;
pub use os_info::OsInfo;
pub use pairing::PairingFlow;
pub use shell::ShellRunner;
pub use system_ops::{fetch_network, kill_process, list_processes, watch_filesystem};
pub use ws_client::WsClient;

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("capability not granted: {0}")]
    CapabilityDenied(String),
    #[error("path outside alwaysAllow: {0}")]
    PathDenied(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("keyring error: {0}")]
    Keyring(String),
    #[error("websocket error: {0}")]
    Ws(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("user denied consent")]
    UserDenied,
    #[error("action cancelled")]
    Cancelled,
    #[error("timed out after {0}ms")]
    Timeout(u64),
    /// JS-level eval failure: the transport succeeded, the model receives
    /// stdout/stderr and the session stays alive for the next turn.
    #[error("eval failed: {message}")]
    EvalFailed {
        session_id: String,
        created_session: bool,
        message: String,
        stdout: String,
        stderr: String,
    },
}

impl From<tungstenite::Error> for DaemonError {
    fn from(e: tungstenite::Error) -> Self {
        DaemonError::Ws(format!("{e}"))
    }
}

pub type Result<T> = std::result::Result<T, DaemonError>;
