//! Shared WebSocket health state.
//!
//! The WS client updates this on every lifecycle event (connect, hello
//! ack, heartbeat ack, error, disconnect). The local HTTP server and
//! the CLI expose it as JSON so agents (or the user) can verify the
//! daemon's real connection state instead of guessing from logs.
//!
//! All fields are cheap to read concurrently: atomics + one mutex for
//! the last error string.
//!
//! Zombie-watchdog counters: `heartbeat_acks` counts acks received since
//! process start, `stale_heartbeats` counts heartbeats that timed out
//! without an ack. A healthy connection is dominated by acks; a zombie
//! (TCP open, server gone) is dominated by stales. Exposed so ops can
//! verify the watchdog actually fired instead of guessing from logs.

use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Debug, Serialize)]
pub struct WsHealthSnapshot {
    pub connected: bool,
    pub last_connected_at_ms: u64,
    pub last_disconnected_at_ms: u64,
    pub last_heartbeat_ack_at_ms: u64,
    pub last_rtt_ms: u64,
    pub reconnects: u64,
    pub heartbeat_acks: u64,
    pub stale_heartbeats: u64,
    pub last_error: String,
}

pub struct WsHealth {
    connected: AtomicBool,
    last_connected_at: AtomicU64,
    last_disconnected_at: AtomicU64,
    last_heartbeat_ack_at: AtomicU64,
    last_rtt_ms: AtomicU64,
    reconnects: AtomicU64,
    heartbeat_acks: AtomicU64,
    stale_heartbeats: AtomicU64,
    last_error: Mutex<String>,
}

impl WsHealth {
    pub fn new() -> Self {
        Self {
            connected: AtomicBool::new(false),
            last_connected_at: AtomicU64::new(0),
            last_disconnected_at: AtomicU64::new(0),
            last_heartbeat_ack_at: AtomicU64::new(0),
            last_rtt_ms: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            heartbeat_acks: AtomicU64::new(0),
            stale_heartbeats: AtomicU64::new(0),
            last_error: Mutex::new(String::new()),
        }
    }

    pub fn mark_connected(&self) {
        self.connected.store(true, Ordering::Relaxed);
        self.last_connected_at.store(now_ms(), Ordering::Relaxed);
    }

    pub fn mark_disconnected(&self) {
        if self.connected.swap(false, Ordering::Relaxed) {
            self.last_disconnected_at.store(now_ms(), Ordering::Relaxed);
            self.reconnects.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn mark_heartbeat_ack(&self, sent_at_ms: u64) {
        self.heartbeat_acks.fetch_add(1, Ordering::Relaxed);
        let now = now_ms();
        self.last_heartbeat_ack_at.store(now, Ordering::Relaxed);
        self.last_rtt_ms
            .store(now.saturating_sub(sent_at_ms), Ordering::Relaxed);
    }

    /// One heartbeat sent over the wire never got its ack. Observable,
    /// monotonic evidence for the zombie watchdog — not a verdict by itself
    /// (the first beat can straddle a hello_ack), but it must never be
    /// silently dropped.
    pub fn mark_heartbeat_stale(&self) {
        self.stale_heartbeats.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_error(&self, msg: &str) {
        if let Ok(mut e) = self.last_error.lock() {
            *e = msg.to_string();
        }
    }

    pub fn snapshot(&self) -> WsHealthSnapshot {
        WsHealthSnapshot {
            connected: self.connected.load(Ordering::Relaxed),
            last_connected_at_ms: self.last_connected_at.load(Ordering::Relaxed),
            last_disconnected_at_ms: self.last_disconnected_at.load(Ordering::Relaxed),
            last_heartbeat_ack_at_ms: self.last_heartbeat_ack_at.load(Ordering::Relaxed),
            last_rtt_ms: self.last_rtt_ms.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
            heartbeat_acks: self.heartbeat_acks.load(Ordering::Relaxed),
            stale_heartbeats: self.stale_heartbeats.load(Ordering::Relaxed),
            last_error: self
                .last_error
                .lock()
                .map(|e| e.clone())
                .unwrap_or_default(),
        }
    }
}

impl Default for WsHealth {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
