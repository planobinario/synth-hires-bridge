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

    // CORS estricto con PNA (Private Network Access)
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _parts| {
            let o = origin.as_bytes();
            o == b"https://synthhires.com"
                || o == b"http://localhost:4321"
                || o == b"http://127.0.0.1:4321"
                || o == b"http://localhost:8787"
                || o == b"http://127.0.0.1:8787"
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

fn check_origin(headers: &axum::http::HeaderMap) -> Result<(), (StatusCode, &'static str)> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if origin != "https://synthhires.com"
        && origin != "http://localhost:4321"
        && origin != "http://127.0.0.1:4321"
        && origin != "http://localhost:8787"
        && origin != "http://127.0.0.1:8787"
    {
        tracing::warn!("Rejecting request from invalid origin: {}", origin);
        return Err((StatusCode::FORBIDDEN, "Invalid Origin"));
    }
    Ok(())
}

async fn handle_status(
    headers: axum::http::HeaderMap,
    State(state): State<ServerState>,
) -> Result<Json<StatusResponse>, (StatusCode, &'static str)> {
    check_origin(&headers)?;
    // Registrar el poll del frontend para saber que la UI está abierta
    state.last_poll.store(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        std::sync::atomic::Ordering::Relaxed,
    );

    let is_paired = state.daemon_state.read().await.device_id.is_some();

    // Si ya está emparejado, no exponemos nonce.
    // Si no está emparejado, comprobamos el TTL del nonce. Si expira o no hay, creamos uno nuevo.
    let nonce = if !is_paired {
        let mut n = state.pairing_nonce.write().await;
        let now = std::time::Instant::now();
        if let Some((_, created_at)) = &*n {
            if now.duration_since(*created_at) > std::time::Duration::from_secs(120) {
                // Expired
                *n = None;
            }
        }
        if n.is_none() {
            *n = Some((uuid::Uuid::new_v4().to_string(), now));
        }
        n.as_ref().map(|(val, _)| val.clone())
    } else {
        None
    };

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
async fn handle_shutdown(
    headers: axum::http::HeaderMap,
    State(_state): State<ServerState>,
) -> Result<Json<serde_json::Value>, (StatusCode, &'static str)> {
    check_origin(&headers)?;
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
    check_origin(&headers)?;
    let is_paired = state.daemon_state.read().await.device_id.is_some();
    if is_paired {
        return Err((StatusCode::CONFLICT, "Already paired"));
    }

    // Validar el Nonce
    {
        let mut n = state.pairing_nonce.write().await;
        let valid = match &*n {
            Some((val, created_at)) => {
                val == &payload.nonce
                    && std::time::Instant::now().duration_since(*created_at)
                        <= std::time::Duration::from_secs(120)
            }
            None => false,
        };
        if !valid {
            tracing::warn!("Rejecting /pair request: invalid, expired, or missing nonce");
            return Err((StatusCode::FORBIDDEN, "Invalid nonce"));
        }
        // Consumir el nonce para que no se pueda reusar (previene replay attacks)
        *n = None;
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
async fn handle_unpair(
    headers: axum::http::HeaderMap,
    State(state): State<ServerState>,
) -> Result<Json<serde_json::Value>, (StatusCode, &'static str)> {
    check_origin(&headers)?;

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
