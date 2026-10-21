use axum::{
    extract::State,
    http::{header, Method, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::DaemonState;

#[derive(Clone)]
pub struct ServerState {
    pub daemon_state: Arc<RwLock<DaemonState>>,
    pub config_dir: std::path::PathBuf,
    pub _backend_url: String,
    /// Port this server was bound to — used by the Host pinning guard
    /// (check_host) so --local-port daemons don't accept Host: 127.0.0.1:7333.
    pub local_port: u16,
    pub pairing_nonce: Arc<RwLock<Option<(String, std::time::Instant)>>>,
    pub status_tx: tokio::sync::watch::Sender<String>,
    pub last_poll: Arc<std::sync::atomic::AtomicU64>,
    pub ws_handle: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    pub chat_store: std::sync::Arc<daemon_core::ChatStore>,
    pub consent: std::sync::Arc<daemon_core::ConsentBroker>,
    pub ws_health: std::sync::Arc<daemon_core::WsHealth>,
}

#[derive(Serialize)]
pub struct StatusResponse {
    paired: bool,
    nonce: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_id: Option<String>,
    version: &'static str,
    ws: daemon_core::WsHealthSnapshot,
    /// Authoritative host OS identification (distro, kernel, arch, libc).
    os: daemon_core::OsInfo,
}

#[derive(Deserialize)]
pub struct PairRequest {
    token: String,
    backend_url: String,
    nonce: String,
    #[serde(default)]
    device_id: Option<String>,
}

pub async fn start_http_server(
    state: ServerState,
    port: u16,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Generar un nonce único al arrancar (solo para emparejamientos nuevos)
    {
        let is_paired = state.daemon_state.read().await.device_id.is_some();
        if !is_paired {
            let mut w = state.pairing_nonce.write().await;
            *w = Some((uuid::Uuid::new_v4().to_string(), std::time::Instant::now()));
        }
    }

    let mut state = state;
    state.local_port = port;

    // CORS estricto con PNA (Private Network Access)
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _parts| {
            origin.to_str().map(is_allowed_origin).unwrap_or(false)
        }))
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE])
        // Cabecera esencial para PNA en navegadores basados en Chromium
        .allow_private_network(true);

    let app = Router::new()
        .route("/status", get(handle_status))
        .route("/pair", post(handle_pair))
        .route("/unpair", post(handle_unpair))
        .route("/shutdown", post(handle_shutdown))
        .layer(cors)
        .with_state(state);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    tracing::info!("Starting secure local HTTP server on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn is_allowed_origin(origin: &str) -> bool {
    if origin == "https://synthhires.com" {
        return true;
    }
    if let Some(rest) = origin.strip_prefix("http://localhost:") {
        return rest.chars().all(|c| c.is_ascii_digit());
    }
    if let Some(rest) = origin.strip_prefix("http://127.0.0.1:") {
        return rest.chars().all(|c| c.is_ascii_digit());
    }
    false
}

fn check_origin(headers: &axum::http::HeaderMap) -> Result<(), (StatusCode, &'static str)> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !is_allowed_origin(origin) {
        tracing::warn!("Rejecting request from invalid origin: {}", origin);
        return Err((StatusCode::FORBIDDEN, "Invalid Origin"));
    }
    Ok(())
}

/// DNS-rebinding guard: the server binds 127.0.0.1, but a rebinding
/// domain (evil.com → 127.0.0.1) would still reach the socket. Browsers
/// send the ATTACKER origin (rejected by check_origin), yet non-browser
/// clients set any Origin they want — so additionally pin the Host to
/// this machine's loopback + the configured port. Both checks must pass.
fn check_host(
    headers: &axum::http::HeaderMap,
    port: u16,
) -> Result<(), (StatusCode, &'static str)> {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // Strip a single trailing-dot FQDN form and compare case-insensitively.
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let expected_ip = format!("127.0.0.1:{port}");
    let expected_local = format!("localhost:{port}");
    if host == expected_ip || host == expected_local {
        return Ok(());
    }
    tracing::warn!("Rejecting request with unexpected Host: {}", host);
    Err((StatusCode::FORBIDDEN, "Invalid Host"))
}

fn check_local(
    headers: &axum::http::HeaderMap,
    port: u16,
) -> Result<(), (StatusCode, &'static str)> {
    check_origin(headers)?;
    check_host(headers, port)?;
    Ok(())
}

/// Single-use local-action nonce (120 s TTL). Consumes on success —
/// every mutating local endpoint (/pair, /unpair, /shutdown) takes one,
/// so a second local process cannot replay a captured call and must win
/// a live race against the legitimate client instead.
async fn take_nonce(
    state: &ServerState,
    presented: &str,
) -> Result<(), (StatusCode, &'static str)> {
    let mut n = state.pairing_nonce.write().await;
    let valid = match &*n {
        Some((val, created_at)) => {
            val == presented
                && std::time::Instant::now().duration_since(*created_at)
                    <= std::time::Duration::from_secs(120)
        }
        None => false,
    };
    if !valid {
        tracing::warn!("Rejecting local request: invalid, expired, or missing nonce");
        return Err((StatusCode::FORBIDDEN, "Invalid nonce"));
    }
    *n = None;
    Ok(())
}

async fn mint_nonce(state: &ServerState) -> String {
    let mut n = state.pairing_nonce.write().await;
    let now = std::time::Instant::now();
    if let Some((_, created_at)) = &*n {
        if now.duration_since(*created_at) <= std::time::Duration::from_secs(120) {
            if let Some((val, _)) = &*n {
                return val.clone();
            }
        }
    }
    let fresh = uuid::Uuid::new_v4().to_string();
    *n = Some((fresh.clone(), now));
    fresh
}

async fn handle_status(
    headers: axum::http::HeaderMap,
    State(state): State<ServerState>,
) -> Result<Json<StatusResponse>, (StatusCode, &'static str)> {
    check_local(&headers, state.local_port)?;
    // Registrar el poll del frontend para saber que la UI está abierta
    state.last_poll.store(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        std::sync::atomic::Ordering::Relaxed,
    );

    let is_paired = state.daemon_state.read().await.device_id.is_some();

    // El nonce es el amarre anti-replay de TODAS las acciones mutantes
    // locales (/pair, /unpair, /shutdown): se emite siempre (emparejado
    // o no), con TTL 120 s y consumo single-use. Sin él, cualquier
    // proceso local podría desemparejar o apagar el daemon.
    let nonce = Some(mint_nonce(&state).await);

    let device_id = state.daemon_state.read().await.device_id.clone();

    Ok(Json(StatusResponse {
        paired: is_paired,
        nonce,
        device_id,
        version: env!("CARGO_PKG_VERSION"),
        ws: state.ws_health.snapshot(),
        os: daemon_core::OsInfo::cached().clone(),
    }))
}

/// Graceful shutdown: local endpoint so the web UI (or a CLI agent)
/// can stop the daemon cleanly instead of killing the process.
/// Requires a fresh single-use nonce (see /status): an unauthenticated
/// local POST must never be able to kill the daemon.
#[derive(Deserialize)]
pub struct ShutdownRequest {
    #[serde(default)]
    pub nonce: String,
}

async fn handle_shutdown(
    headers: axum::http::HeaderMap,
    State(state): State<ServerState>,
    Json(payload): Json<ShutdownRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, &'static str)> {
    check_local(&headers, state.local_port)?;
    take_nonce(&state, &payload.nonce).await?;
    tracing::info!("Shutdown requested via local HTTP");
    // Give the response a moment to flush before the process exits.
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        std::process::exit(0);
    });
    Ok(Json(
        serde_json::json!({ "success": true, "message": "shutting down" }),
    ))
}

async fn handle_pair(
    headers: axum::http::HeaderMap,
    State(state): State<ServerState>,
    Json(payload): Json<PairRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, &'static str)> {
    check_local(&headers, state.local_port)?;
    let is_paired = state.daemon_state.read().await.device_id.is_some();
    if is_paired {
        return Err((StatusCode::CONFLICT, "Already paired"));
    }

    // Nonce single-use (previene replay attacks).
    take_nonce(&state, &payload.nonce).await?;

    // Validar el backend ANTES de persistir nada: un backend arbitrario
    // aquí redirigiría el WS a un servidor atacante con el token real.
    if let Err(e) = daemon_core::pairing::validate_backend_url(&payload.backend_url) {
        tracing::warn!("Rejecting /pair: invalid backend_url: {e}");
        return Err((StatusCode::BAD_REQUEST, "Invalid backend_url"));
    }

    tracing::info!("Valid pairing request received via local HTTP. Token handoff starting...");

    // The web UI knows the REAL deviceId from pair/complete. Without it
    // (legacy clients) we cannot key the OS keyring correctly, because
    // the daemon loads the token by deviceId at startup. Reject instead
    // of storing the token AS the deviceId — that corrupts state.json.
    let device_id = match payload.device_id.clone() {
        Some(id) if !id.trim().is_empty() => id,
        _ => {
            tracing::error!("Rejecting /pair: missing device_id");
            return Err((StatusCode::BAD_REQUEST, "Missing device_id"));
        }
    };

    // Guardar token en el llavero local (Keyring) bajo el deviceId REAL
    if daemon_core::keyring::TokenStore::save(&device_id, &payload.token).is_err() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to save token to keyring",
        ));
    }

    // Actualizar estado
    {
        let mut s = state.daemon_state.write().await;
        s.device_id = Some(device_id);
        s.backend_url = Some(payload.backend_url.clone());
        let _ = s.save(&state.config_dir).await;
    }

    // Iniciar el WS Client de forma asíncrona
    let ws_state = state.daemon_state.clone();
    let status_tx = state.status_tx.clone();
    let ws_store = state.chat_store.clone();
    let ws_consent = state.consent.clone();
    let ws_health = state.ws_health.clone();
    let mut hw = state.ws_handle.lock().await;
    if let Some(h) = hw.take() {
        h.abort();
    }
    *hw = Some(tokio::spawn(async move {
        let _ = status_tx.send("Conectando al servidor...".to_string());
        if let Err(e) = crate::run_ws_client(
            ws_state,
            payload.backend_url,
            ws_store,
            ws_consent,
            ws_health,
        )
        .await
        {
            tracing::error!("WS client died: {e}");
            let _ = status_tx.send(format!("Error de conexión: {e}"));
        }
    }));

    Ok(Json(serde_json::json!({ "success": true })))
}

/// Local unpair so the web UI can re-bind the daemon when it was
/// paired under a different account/session. Clears the keyring entry,
/// state.json, and the WS client; the pairing nonce is regenerated so
/// the browser can immediately POST /pair with its own device.
/// Requires a fresh single-use nonce: without it, any local process
/// could silently steal the device binding (unpair → pair-as-attacker).
#[derive(Deserialize)]
pub struct UnpairRequest {
    #[serde(default)]
    pub nonce: String,
}

async fn handle_unpair(
    headers: axum::http::HeaderMap,
    State(state): State<ServerState>,
    Json(payload): Json<UnpairRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, &'static str)> {
    check_local(&headers, state.local_port)?;
    take_nonce(&state, &payload.nonce).await?;

    let device_id = {
        let s = state.daemon_state.read().await;
        s.device_id.clone()
    };
    if let Some(id) = device_id {
        let _ = daemon_core::keyring::TokenStore::delete(&id);
    }
    {
        let mut s = state.daemon_state.write().await;
        s.device_id = None;
        s.backend_url = None;
        let _ = s.save(&state.config_dir).await;
    }
    // Stop the WS client
    {
        let mut hw = state.ws_handle.lock().await;
        if let Some(h) = hw.take() {
            h.abort();
        }
    }
    // Fresh nonce so the browser can pair immediately
    {
        let mut n = state.pairing_nonce.write().await;
        *n = Some((uuid::Uuid::new_v4().to_string(), std::time::Instant::now()));
    }
    let _ = state
        .status_tx
        .send("Esperando emparejamiento...".to_string());
    tracing::info!("Unpaired via local HTTP; ready for re-pairing");
    Ok(Json(serde_json::json!({ "success": true })))
}

#[cfg(test)]
mod tests {
    use super::is_allowed_origin;
    use super::{check_host, check_local};
    use axum::http::{header, HeaderMap};

    #[test]
    fn test_allowed_origin_production() {
        assert!(is_allowed_origin("https://synthhires.com"));
    }

    #[test]
    fn test_allowed_localhost_ports() {
        assert!(is_allowed_origin("http://localhost:3000"));
        assert!(is_allowed_origin("http://localhost:8788"));
        assert!(is_allowed_origin("http://127.0.0.1:8788"));
        assert!(is_allowed_origin("http://127.0.0.1:4323"));
    }

    #[test]
    fn test_rejected_origin_http_prod() {
        // Production origin is HTTPS-only — a stray http:// must be rejected.
        assert!(!is_allowed_origin("http://synthhires.com"));
    }

    #[test]
    fn test_rejected_origin_non_numeric_port() {
        // Non-numeric port segment → reject (path injection defense).
        assert!(!is_allowed_origin("http://localhost:abc"));
        assert!(!is_allowed_origin("http://127.0.0.1:878a"));
    }

    #[test]
    fn test_rejected_origin_path_injection() {
        // A path or extra suffix after the port must be rejected — this
        // is the host-guard invariant: only `<host>:<digits>` is allowed.
        assert!(!is_allowed_origin("http://localhost:3000/malicious"));
        assert!(!is_allowed_origin("http://127.0.0.1:8788/__bridge/consent-relay"));
    }

    #[test]
    fn test_rejected_origin_arbitrary_domain() {
        assert!(!is_allowed_origin("https://evil.example.com"));
        assert!(!is_allowed_origin("https://synthhires.com.evil.com"));
    }

    #[test]
    fn test_rejected_empty_origin() {
        assert!(!is_allowed_origin(""));
        assert!(!is_allowed_origin("null"));
    }

    #[test]
    fn test_rejected_https_localhost() {
        // HTTPS to localhost is not in the allow-list (no localhost cert).
        assert!(!is_allowed_origin("https://localhost:3000"));
        assert!(!is_allowed_origin("https://127.0.0.1:3000"));
    }

    fn headers_with(host: &str, origin: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, host.parse().unwrap());
        h.insert(header::ORIGIN, origin.parse().unwrap());
        h
    }

    #[test]
    fn test_host_pinning_loopback_ok() {
        let h = headers_with("127.0.0.1:7333", "https://synthhires.com");
        assert!(check_host(&h, 7333).is_ok());
        let h = headers_with("localhost:7333", "http://localhost:4321");
        assert!(check_host(&h, 7333).is_ok());
    }

    #[test]
    fn test_host_pinning_rejects_rebinding_and_wrong_port() {
        // DNS-rebinding: attacker domain resolving to 127.0.0.1.
        let h = headers_with("evil.example.com", "https://synthhires.com");
        assert!(check_host(&h, 7333).is_err());
        // Right host, wrong port (custom --local-port daemon).
        let h = headers_with("127.0.0.1:9999", "https://synthhires.com");
        assert!(check_host(&h, 7333).is_err());
        // Missing Host entirely.
        let mut h = HeaderMap::new();
        h.insert(header::ORIGIN, "https://synthhires.com".parse().unwrap());
        assert!(check_host(&h, 7333).is_err());
    }

    #[test]
    fn test_check_local_requires_both() {
        // Good origin + bad host → reject.
        let h = headers_with("evil.example.com", "https://synthhires.com");
        assert!(check_local(&h, 7333).is_err());
        // Good host + bad origin → reject.
        let h = headers_with("127.0.0.1:7333", "https://evil.example.com");
        assert!(check_local(&h, 7333).is_err());
        // Both good → pass.
        let h = headers_with("127.0.0.1:7333", "https://synthhires.com");
        assert!(check_local(&h, 7333).is_ok());
    }
}
