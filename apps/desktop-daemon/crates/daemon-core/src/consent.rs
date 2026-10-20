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

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a sample prompt with the given id.
    fn make_prompt(id: &str) -> ConsentPrompt {
        ConsentPrompt {
            action_id: id.to_string(),
            capability: "desktop.fs.write".to_string(),
            summary: "Write to file".to_string(),
            path: Some("/home/user/notes.txt".to_string()),
        }
    }

    #[test]
    fn test_ask_creates_pending_prompt() {
        let broker = ConsentBroker::new();
        let _rx = broker.ask(make_prompt("action_42"));
        let pending = broker.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].action_id, "action_42");
    }

    #[test]
    fn test_answer_approved_resolves_receiver() {
        let broker = ConsentBroker::new();
        let mut rx = broker.ask(make_prompt("action_01"));
        // Not yet answered — receiver should be pending. (`is_closed` does
        // not exist on tokio 1.53's Receiver; try_recv(Empty) is the same
        // fact without consuming anything.)
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        let ok = broker.answer("action_01", ConsentAnswer { approved: true, remember: false });
        assert!(ok);
        // Receiver should now be ready with the answer.
        let ans = rx.try_recv().expect("receiver should resolve");
        assert!(ans.approved);
        assert!(!ans.remember);
    }

    #[test]
    fn test_answer_remember_flag_propagates() {
        let broker = ConsentBroker::new();
        let mut rx = broker.ask(make_prompt("action_02"));
        let ok = broker.answer("action_02", ConsentAnswer { approved: true, remember: true });
        assert!(ok);
        let ans = rx.try_recv().expect("receiver should resolve");
        assert!(ans.approved);
        assert!(ans.remember);
    }

    #[test]
    fn test_answer_denied() {
        let broker = ConsentBroker::new();
        let mut rx = broker.ask(make_prompt("action_03"));
        let ok = broker.answer("action_03", ConsentAnswer { approved: false, remember: false });
        assert!(ok);
        let ans = rx.try_recv().expect("receiver should resolve");
        assert!(!ans.approved);
    }

    #[test]
    fn test_answer_unknown_id_returns_false() {
        let broker = ConsentBroker::new();
        broker.ask(make_prompt("action_04"));
        let ok = broker.answer("nonexistent", ConsentAnswer { approved: true, remember: false });
        assert!(!ok);
    }

    #[test]
    fn test_answer_removes_from_pending() {
        let broker = ConsentBroker::new();
        broker.ask(make_prompt("action_05"));
        assert_eq!(broker.pending().len(), 1);
        broker.answer("action_05", ConsentAnswer { approved: true, remember: false });
        assert!(broker.pending().is_empty());
    }

    // ---- Notifier (wake-up) tests ----

    #[tokio::test]
    async fn test_notified_wakes_parked_action() {
        let broker = ConsentBroker::new();
        // Install the decision channel the WS client would create.
        let (notif_tx, mut notif_rx) = mpsc::unbounded_channel();
        broker.set_notifier(notif_tx);
        let mut rx = broker.ask(make_prompt("action_06"));
        // Answering should:
        //   1. deliver the ConsentAnswer to the oneshot receiver (rx)
        //   2. fire a (action_id, answer) tuple into the notifier channel
        let ok = broker.answer("action_06", ConsentAnswer { approved: true, remember: false });
        assert!(ok);
        // Immediate answer on the oneshot.
        let ans = rx.try_recv().expect("oneshot should deliver");
        assert!(ans.approved);
        // The notifier should have woken the WS loop.
        let woke = notif_rx.try_recv().expect("notifier should wake");
        assert_eq!(woke.0, "action_06");
        assert!(woke.1.approved);
    }

    #[tokio::test]
    async fn test_no_notifier_does_not_panic() {
        // set_notifier not called → answering a real prompt must still work
        // (the answer reaches the oneshot), just the wake-up is skipped.
        let broker = ConsentBroker::new();
        let mut rx = broker.ask(make_prompt("action_07"));
        let ok = broker.answer("action_07", ConsentAnswer { approved: false, remember: false });
        assert!(ok);
        let ans = rx.try_recv().expect("oneshot should still deliver");
        assert!(!ans.approved);
    }

    #[tokio::test]
    async fn test_double_answer_idempotent() {
        // A second answer for the same id should return false (prompt
        // already removed) and NOT fire a second notification.
        let broker = ConsentBroker::new();
        let (notif_tx, mut notif_rx) = mpsc::unbounded_channel();
        broker.set_notifier(notif_tx);
        let mut rx = broker.ask(make_prompt("action_08"));
        assert!(broker.answer("action_08", ConsentAnswer { approved: true, remember: false }));
        assert!(!broker.answer("action_08", ConsentAnswer { approved: false, remember: false }));
        // oneshot resolves to the FIRST answer.
        let ans = rx.try_recv().expect("oneshot should deliver first answer");
        assert!(ans.approved);
        // Exactly one notification fired.
        notif_rx.try_recv().expect("first notification");
        assert!(notif_rx.try_recv().is_err(), "no duplicate notification");
    }

    #[test]
    fn test_multiple_concurrent_prompts() {
        let broker = ConsentBroker::new();
        let mut rx_a = broker.ask(make_prompt("a"));
        let mut rx_b = broker.ask(make_prompt("b"));
        let mut rx_c = broker.ask(make_prompt("c"));
        assert_eq!(broker.pending().len(), 3);
        // Answer in non-sequential order.
        assert!(broker.answer("c", ConsentAnswer { approved: true, remember: false }));
        assert!(broker.answer("a", ConsentAnswer { approved: false, remember: false }));
        assert!(broker.answer("b", ConsentAnswer { approved: true, remember: true }));
        assert_eq!(rx_a.try_recv().unwrap().approved, false);
        assert_eq!(rx_b.try_recv().unwrap().approved, true);
        assert_eq!(rx_c.try_recv().unwrap().approved, true);
        assert!(broker.pending().is_empty());
    }

    #[test]
    fn test_default_impl() {
        let broker = ConsentBroker::default();
        assert!(broker.pending().is_empty());
    }

    #[test]
    fn test_answer_denied_no_wake() {
        let broker = ConsentBroker::new();
        let (notif_tx, mut notif_rx) = mpsc::unbounded_channel();
        broker.set_notifier(notif_tx);
        let mut rx = broker.ask(make_prompt("action_10"));
        let ok = broker.answer("action_10", ConsentAnswer { approved: false, remember: false });
        assert!(ok);
        let ans = rx.try_recv().expect("receiver should resolve");
        assert!(!ans.approved);
        // Even a denied answer wakes the parked action.
        let woke = notif_rx.try_recv().expect("notifier should wake even on deny");
        assert_eq!(woke.1.approved, false);
    }
}
