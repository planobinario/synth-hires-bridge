//! Live E2E for the keyboard actions against REAL apps (skips without the
//! explicit opt-in + desktop session). Uses a zenity --entry scratch dialog:
//!
//! - On X11: type_text synthesizes real keystrokes through the at-spi
//!   DeviceEventController and press_key Return accepts the dialog; zenity
//!   echoes the entry text on stdout, proving the FULL stack landed.
//! - On Wayland (compositors block at-spi key injection): type_text must
//!   FAIL with the actionable Wayland guidance, and set_text (native
//!   EditableText) must still write into the same field — the native path
//!   never needs input injection. This is the honest contract: no silent
//!   no-ops, no fake successes.
//!
//! Creates its own scratch dialog and always tears it down.

use daemon_core::os_ax::{OsActParams, OsAxEngine, OsSnapshotParams};

fn opt_in() -> bool {
    std::env::var("DBUS_SESSION_BUS_ADDRESS").is_ok()
        && std::env::var("SYNTHHIRES_OS_AX_KEYTEST").ok().as_deref() == Some("1")
}

fn zenity_bin() -> Option<String> {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("ls /nix/store/*zenity*/bin/zenity 2>/dev/null | head -1")
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn session_kind() -> &'static str {
    match std::env::var("XDG_SESSION_TYPE").ok().as_deref() {
        Some("x11") => "x11",
        _ => "wayland",
    }
}

#[tokio::test]
async fn live_keyboard_actions_follow_the_session_contract() {
    if !opt_in() {
        eprintln!("os ax key e2e: skipped — set SYNTHHIRES_OS_AX_KEYTEST=1 with a desktop session");
        return;
    }
    let Some(zenity) = zenity_bin() else {
        eprintln!("os ax key e2e: skipped — no zenity in the store");
        return;
    };

    // Scratch entry dialog (owned by this test; killed in every exit path).
    let mut child = std::process::Command::new(&zenity)
        .args(["--entry", "--title=sh-e2e-key", "--text=type below"])
        .env("DISPLAY", ":0")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn zenity entry");
    let mut child = ChildGuard(child);

    std::thread::sleep(std::time::Duration::from_millis(2500));

    let engine = OsAxEngine::new();
    let apps = engine.apps().await.expect("apps");
    let zenity_provider = apps
        .apps
        .iter()
        .map(|a| a.name.clone())
        .find(|n| n.to_lowercase().contains("zenity"))
        .expect("zenity must be on the bus");

    let snap = engine
        .snapshot(OsSnapshotParams {
            app: zenity_provider.clone(),
            limit: Some(100),
            focus: None,
        })
        .await
        .expect("snapshot of the entry dialog");

    // The entry line: role "entry"/"text box" with editable state.
    let entry_ref = snap
        .text
        .lines()
        .find(|l| (l.contains("entry") || l.contains("text box")) && l.contains("[ref="))
        .and_then(|l| {
            l.split("[ref=")
                .nth(1)
                .and_then(|r| r.split(']').next())
                .and_then(|n| n.parse::<i64>().ok())
        })
        .expect("the dialog must expose a text entry");

    let typed_result = engine
        .act(OsActParams {
            element_ref: entry_ref,
            action: "type_text".into(),
            text: Some("synthhires-e2e-42".into()),
        })
        .await;

    if session_kind() == "wayland" {
        // Wayland contract: synthesis is rejected with actionable guidance.
        let err = typed_result.expect_err("Wayland must REJECT type_text, never silently no-op");
        let msg = err.to_string();
        assert!(
            msg.contains("Wayland") || msg.contains("native actions"),
            "the rejection must be actionable, got: {msg}"
        );
        eprintln!("os ax key e2e: wayland rejection OK ({msg:.80}…)");

        // The NATIVE path still writes: set_text via EditableText.
        engine
            .act(OsActParams {
                element_ref: entry_ref,
                action: "set_text".into(),
                text: Some("synthhires-native-42".into()),
            })
            .await
            .expect("set_text (native EditableText) must work on Wayland");
        eprintln!("os ax key e2e: native set_text landed on Wayland ✔");
    } else {
        // X11 contract: real keystrokes through the registry.
        let typed = typed_result.expect("X11 type_text must synthesize typing");
        eprintln!("os ax key e2e: typed ({typed:?})");
        engine
            .act(OsActParams {
                element_ref: entry_ref,
                action: "press_key".into(),
                text: Some("Return".into()),
            })
            .await
            .expect("press_key Return");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            match child.0.try_wait().expect("poll zenity") {
                Some(_) => break,
                None if std::time::Instant::now() > deadline => {
                    panic!("zenity never accepted — typing did not land on X11");
                }
                None => std::thread::sleep(std::time::Duration::from_millis(200)),
            }
        }
        let mut stdout = String::new();
        use std::io::Read;
        child
            .0
            .stdout
            .take()
            .expect("stdout piped")
            .read_to_string(&mut stdout)
            .expect("read zenity stdout");
        assert!(
            stdout.contains("synthhires-e2e-42"),
            "typed text must arrive through the FULL stack, got: {stdout:?}"
        );
    }
}

/// Kill the scratch dialog on every exit path — never leave popups behind.
struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
