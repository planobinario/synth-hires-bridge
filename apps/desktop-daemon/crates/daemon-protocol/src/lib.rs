//! Mirror of `src/lib/agent/bridge-protocol.ts`.
//!
//! This crate is the single source of truth for the Rust wire format.
//! The TypeScript module in the main repo MUST be regenerated from
//! here when shapes change. The conversion is mechanical: serde
//! rename_all = "camelCase" matches the JSON field names exactly.
//!
//! Protocol version is hard-coded to 1; breaking changes require
//! bumping to 2 in BOTH crates in lockstep (CI guard).

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BridgeFrame {
    Hello(HelloFrame),
    HelloAck(HelloAckFrame),
    Heartbeat(HeartbeatFrame),
    HeartbeatAck(HeartbeatAckFrame),
    ActionRequest(ActionRequestFrame),
    ActionCancel(ActionCancelFrame),
    ActionResult(ActionResultFrame),
    ActionStream(ActionStreamFrame),
    ConsentPrompt(ConsentPromptFrame),
    ConsentResponse(ConsentResponseFrame),
    ScopeUpdate(ScopeUpdateFrame),
    Resume(ResumeFrame),
    Revoke(RevokeFrame),
    Error(ErrorFrame),
}

// The TS wire format (src/lib/agent/bridge-protocol.ts) is camelCase
// for EVERY field on every frame. Without this attribute the daemon
// fails to deserialize real frames from the web app (e.g.
// `conversationId` vs `conversation_id`) and silently drops them.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloFrame {
    pub v: u32,
    pub token_hash: String,
    pub fingerprint: String,
    pub device_kind: DeviceKind,
    pub device_name: String,
    pub client_version: String,
    /// OS the daemon runs on (std::env::consts::OS: windows/macos/linux/android/…).
    /// Defaulted for wire-compat with daemons that predate this field.
    #[serde(default)]
    pub os: String,
    /// CPU architecture (std::env::consts::ARCH: x86_64/aarch64/…).
    #[serde(default)]
    pub arch: String,
    /// Optional capability negotiation (protocol v1.1). Older daemons do not
    /// send this field; the web treats a missing/empty list as "legacy daemon"
    /// and only dispatches the classic v1 capability set. Never gate the
    /// CLASSIC capabilities behind this — it only unlocks NEW actions such as
    /// `desktop.fs.patch` (hashline edits). Keep in lockstep with the web's
    /// BRIDGE_FEATURES (src/lib/agent/bridge-protocol.ts).
    #[serde(default)]
    pub features: Vec<String>,
    /// One-shot manifest of tools already on the user's machine (CLIs,
    /// language servers, debug adapters — daemon-core/tools_probe.rs).
    /// Protocol v1.1: serde-defaulted, older webs ignore it. Empty/missing
    /// means "unknown", never "absent" — consumers must not gate features
    /// off a missing field, only off an explicit probe result.
    #[serde(default)]
    pub tools: Vec<crate::ToolProbe>,
}

/// One probed tool (protocol v1.1). Keep in lockstep with the web's
/// BridgeToolProbe (src/lib/agent/bridge-protocol.ts).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolProbe {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub kind: crate::ToolsProbeKind,
}

/// What the probed tool powers (mirrors tools_probe::ToolKind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolsProbeKind {
    Shell,
    Vcs,
    Lsp,
    Dap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloAckFrame {
    pub v: u32,
    pub device_id: String,
    pub scopes: Scopes,
    pub heartbeat_interval_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    Desktop,
    Mobile,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scopes {
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub always_allow_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatFrame {
    pub v: u32,
    pub t: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatAckFrame {
    pub v: u32,
    pub t: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRequestFrame {
    pub v: u32,
    pub id: String,
    pub capability: String,
    pub params: serde_json::Value,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub skip_consent_prompt: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionCancelFrame {
    pub v: u32,
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionResultFrame {
    pub v: u32,
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ActionError>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionStreamFrame {
    pub v: u32,
    pub id: String,
    pub seq: u64,
    pub channel: StreamChannel,
    pub data: String,
    #[serde(default)]
    pub eof: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StreamChannel {
    Stdout,
    Stderr,
    Log,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsentPromptFrame {
    pub v: u32,
    pub id: String,
    pub capability: String,
    pub summary: String,
    pub params_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsentResponseFrame {
    pub v: u32,
    pub id: String,
    pub approved: bool,
    pub remember: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeUpdateFrame {
    pub v: u32,
    pub scopes: Scopes,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeFrame {
    pub v: u32,
    pub device_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevokeFrame {
    pub v: u32,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorFrame {
    pub v: u32,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close: Option<u16>,
}

/// Close codes from src/lib/agent/bridge-protocol.ts BRIDGE_CLOSE_CODES.
pub mod close_codes {
    pub const NORMAL: u16 = 1000;
    pub const GOING_AWAY: u16 = 1001;
    pub const AUTH_FAILED: u16 = 4001;
    pub const CAPABILITY_NOT_GRANTED: u16 = 4003;
    pub const RATE_LIMITED: u16 = 4029;
    pub const REVOKED: u16 = 4401;
    pub const PROTOCOL_MISMATCH: u16 = 4400;
}

/// Crockford-base32 used by the TS `bridge-codes.ts`. Both endpoints
/// of the bridge surface use this so the QR code in the web UI is
/// decodable by the Rust daemon without a translation step.
pub const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

// ── Chat sync payloads (sync.chat.push) ─────────────────────────────
//
// The server pushes conversation snapshots to paired devices as
// regular `action_request` frames with capability `sync.chat.push`.
// The daemon persists them to its local SQLite store and ACKs with
// an `action_result`. Payload shapes mirror the web app's
// `saveAndSyncChat` objects (id, title, model, provider, messages,
// updatedAt) so no extra transformation lives on either side.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatSyncMessage {
    pub id: String,
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub created_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatSyncConversation {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub workspace_ref: Option<serde_json::Value>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub is_pinned: Option<bool>,
    #[serde(default)]
    pub updated_at: Option<u64>,
    #[serde(default)]
    pub messages: Vec<ChatSyncMessage>,
}

/// Parse helper: extract `sync.chat.push` params from an opaque
/// `serde_json::Value` params bag on an ActionRequestFrame.
pub fn parse_chat_push_params(
    params: &serde_json::Value,
) -> Result<Vec<ChatSyncConversation>, String> {
    let convs = params
        .get("conversations")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "sync.chat.push params.conversations must be an array".to_string())?;
    let mut out = Vec::with_capacity(convs.len());
    for c in convs {
        match serde_json::from_value::<ChatSyncConversation>(c.clone()) {
            Ok(conv) => out.push(conv),
            Err(e) => return Err(format!("invalid conversation entry: {e}")),
        }
    }
    Ok(out)
}

/// Generate a 6-char pairing code (30 bits of entropy). MUST match
/// the algorithm in `src/lib/agent/bridge-codes.ts → toBase32`.
pub fn generate_pairing_code() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 5];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    to_crockford(&bytes, 6)
}

pub fn to_crockford(bytes: &[u8], chars: usize) -> String {
    let mut out = String::with_capacity(chars);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in bytes {
        buffer = (buffer << 8) | byte as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(CROCKFORD[((buffer >> bits) & 0x1f) as usize] as char);
            if out.len() == chars {
                return out;
            }
        }
    }
    if bits > 0 && out.len() < chars {
        out.push(CROCKFORD[((buffer << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}
