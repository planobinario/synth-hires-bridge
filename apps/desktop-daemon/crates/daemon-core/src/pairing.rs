//! Pairing flow glue: exchange a short-lived code for a device token.

use crate::{keyring::TokenStore, Result};
use daemon_protocol::Scopes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairCompleteRequest {
    pub code: String,
    pub pairing_id: Option<String>,
    pub device_kind: &'static str,
    pub device_name: String,
    pub fingerprint: String,
    pub desired_scopes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairCompleteData {
    pub device_id: String,
    pub token: String,
    pub token_expires_at: String,
    pub ws_url: String,
    pub scopes: Scopes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairCompleteResponse {
    pub success: bool,
    pub data: PairCompleteData,
}

pub struct PairingFlow<'a> {
    backend_url: &'a str,
    client: &'a reqwest::Client,
}

impl<'a> PairingFlow<'a> {
    pub fn new(backend_url: &'a str, client: &'a reqwest::Client) -> Self {
        Self {
            backend_url,
            client,
        }
    }

    pub async fn complete(&self, mut req: PairCompleteRequest) -> Result<PairCompleteResponse> {
        // Keep CLI/deep-link pairing aligned with the web's default scope.
        // Existing callers can request a subset, but a Desktop client must
        // never silently omit a capability that this binary implements.
        if req.device_kind == "desktop" {
            for capability in [
                "desktop.shell.execute",
                "desktop.fs.read",
                "desktop.fs.write",
                "desktop.fs.delete",
                "desktop.fs.verify",
                "desktop.fs.list",
                "desktop.fs.watch",
                "desktop.process.list",
                "desktop.process.kill",
                "desktop.network.fetch",
                "sync.chat.push",
            ] {
                if !req.desired_scopes.iter().any(|value| value == capability) {
                    req.desired_scopes.push(capability.to_string());
                }
            }
        }

        let url = format!(
            "{}/api/devices/pair/complete",
            self.backend_url.trim_end_matches('/')
        );
        let response = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| crate::DaemonError::Protocol(format!("pair/complete http: {e}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(crate::DaemonError::Protocol(format!(
                "pair/complete {status}: {body}"
            )));
        }

        let body: PairCompleteResponse = response
            .json()
            .await
            .map_err(|e| crate::DaemonError::Protocol(format!("pair/complete decode: {e}")))?;
        let ws_url = ws_endpoint_for_origin(&self.backend_url, &body.data.ws_url);
        TokenStore::save(&body.data.device_id, &body.data.token)?;
        Ok(PairCompleteResponse {
            data: PairCompleteData { ws_url, ..body.data },
            success: body.success,
        })
    }
}

/// Builds the WebSocket endpoint the WS client dials. Idempotent: the saved
/// state may hold a plain origin (deep-link flow), a full http(s) URL that
/// already contains the path, or an already-normalized ws(s) endpoint from
/// a previous run — each must land on exactly one path suffix, never a
/// doubled `/api/devices/ws/api/devices/ws` (that 404 was caught live by
/// the paired-daemon E2E the moment the first normalized state re-ran
/// through this function).
pub fn ws_endpoint_for_origin(backend_origin: &str, ws_path: &str) -> String {
    // Server already returned a full ws:// / wss:// URL: use it verbatim.
    if ws_path.starts_with("ws://") || ws_path.starts_with("wss://") {
        return ws_path.to_string();
    }
    // State already holds a normalized ws endpoint (previous run): verbatim,
    // never a doubled path.
    if backend_origin.starts_with("ws://") || backend_origin.starts_with("wss://") {
        return backend_origin.to_string();
    }
    let trimmed = backend_origin.trim_end_matches('/');
    // http(s) URL that already ends with the ws path: convert scheme only.
    if trimmed.ends_with(ws_path) {
        return trimmed
            .replace("https://", "wss://")
            .replace("http://", "ws://");
    }
    let origin = trimmed
        .replace("https://", "wss://")
        .replace("http://", "ws://");
    format!("{origin}{}", ws_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_endpoint_converts_http_origins() {
        assert_eq!(
            ws_endpoint_for_origin("http://127.0.0.1:4331", "/api/devices/ws"),
            "ws://127.0.0.1:4331/api/devices/ws"
        );
        assert_eq!(
            ws_endpoint_for_origin("https://synthhires.com", "/api/devices/ws"),
            "wss://synthhires.com/api/devices/ws"
        );
        assert_eq!(
            ws_endpoint_for_origin("http://host/", "/api/devices/ws"),
            "ws://host/api/devices/ws"
        );
    }

    #[test]
    fn ws_endpoint_keeps_absolute_ws_urls() {
        assert_eq!(
            ws_endpoint_for_origin("http://ignored", "ws://a/b"),
            "ws://a/b"
        );
        assert_eq!(
            ws_endpoint_for_origin("http://ignored", "wss://a/b"),
            "wss://a/b"
        );    }

    #[test]
    fn ws_endpoint_is_idempotent_over_saved_states() {
        // Plain origin (deep-link state): appends the path.
        assert_eq!(
            ws_endpoint_for_origin("http://127.0.0.1:8799", "/api/devices/ws"),
            "ws://127.0.0.1:8799/api/devices/ws"
        );
        // Already-normalized ws endpoint (a previous run's state): verbatim,
        // never a doubled path.
        assert_eq!(
            ws_endpoint_for_origin("ws://127.0.0.1:8799/api/devices/ws", "/api/devices/ws"),
            "ws://127.0.0.1:8799/api/devices/ws"
        );
        // Full http URL that already carries the path: scheme-only convert.
        assert_eq!(
            ws_endpoint_for_origin("http://127.0.0.1:8799/api/devices/ws", "/api/devices/ws"),
            "ws://127.0.0.1:8799/api/devices/ws"
        );
        // Running the output back through the function is a no-op.
        let once = ws_endpoint_for_origin("http://host", "/api/devices/ws");
        assert_eq!(
            ws_endpoint_for_origin(&once, "/api/devices/ws"),
            once
        );
    }
}

pub fn fingerprint_hash(hostname: &str, os: &str, machine_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(hostname.as_bytes());
    hash.update(b"|");
    hash.update(os.as_bytes());
    hash.update(b"|");
    hash.update(machine_id.as_bytes());
    hex::encode(hash.finalize())
}
