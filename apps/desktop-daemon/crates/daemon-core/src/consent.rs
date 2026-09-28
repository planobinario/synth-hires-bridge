//! Local consent broker.
//!
//! The WS client raises a consent prompt for actions that fall outside
//! the device's `alwaysAllowPaths`. The egui UI polls `pending()` on
//! every frame, shows a native dialog, and answers through `answer()`.
//! The WS client awaits the answer with a bounded timeout before it
//! executes (or refuses) the action.
//!
//! Threading model: std::sync::Mutex because the UI thread (sync,
//! egui) and the tokio runtime (async, ws_client) share the broker.
//! Critical sections are tiny and never await.

use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone)]
pub struct ConsentPrompt {
    pub action_id: String,
    pub capability: String,
    pub summary: String,
    /// Path the action targets (fs ops only; shell actions have None).
    pub path: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ConsentAnswer {
    pub approved: bool,
    /// "Always allow this path" — the WS client adds the path to the
    /// local gate snapshot AND pushes the scope update to the server.
    pub remember: bool,
}

#[derive(Default)]
struct ConsentState {
    pending: HashMap<String, (ConsentPrompt, oneshot::Sender<ConsentAnswer>)>,
}

pub struct ConsentBroker {
    state: Mutex<ConsentState>,
    /// Wake-up channel for the WS read loop: the loop must NEVER block on a
    /// consent (it would stop reading heartbeats/acks/consent_responses —
    /// a self-inflicted zombie). Instead, whoever answers (web relay frame
    /// handled by the loop itself, or the local egui UI) goes through
    /// `answer()`, which pushes the decision here so the loop's select!
    /// can resume the parked action.
    notifier: Mutex<Option<mpsc::UnboundedSender<(String, ConsentAnswer)>>>,
}

impl ConsentBroker {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(ConsentState::default()),
            notifier: Mutex::new(None),
        }
    }

    /// The WS client installs its decision channel at startup.
    pub fn set_notifier(&self, tx: mpsc::UnboundedSender<(String, ConsentAnswer)>) {
        if let Ok(mut n) = self.notifier.lock() {
            *n = Some(tx);
        }
    }

    /// Register a prompt; the returned receiver resolves when the UI
    /// answers. If the caller drops the receiver (timeout), the prompt
    /// stays visible until the user answers or dismisses it.
    pub fn ask(&self, prompt: ConsentPrompt) -> oneshot::Receiver<ConsentAnswer> {
        let (tx, rx) = oneshot::channel();
        if let Ok(mut st) = self.state.lock() {
            st.pending.insert(prompt.action_id.clone(), (prompt, tx));
        }
        rx
    }

    /// Snapshot of pending prompts for the UI.
    pub fn pending(&self) -> Vec<ConsentPrompt> {
        match self.state.lock() {
            Ok(s) => s.pending.values().map(|(p, _)| p.clone()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// UI-side answer. Returns false when the prompt no longer exists
    /// (already answered, or the WS caller timed out).
    pub fn answer(&self, action_id: &str, answer: ConsentAnswer) -> bool {
        let resolved = {
            let Ok(mut st) = self.state.lock() else {
                return false;
            };
            if let Some((_, tx)) = st.pending.remove(action_id) {
                let _ = tx.send(answer);
                true
            } else {
                false
            }
        };
        if resolved {
            // Wake the parked action regardless of who answered (web or
            // local UI). Fire-and-forget: a missing/late receiver just
            // means the action was already swept.
            if let Ok(n) = self.notifier.lock() {
                if let Some(tx) = n.as_ref() {
                    let _ = tx.send((action_id.to_string(), answer));
                }
            }
        }
        resolved
    }
}

impl Default for ConsentBroker {
    fn default() -> Self {
        Self::new()
    }
}
