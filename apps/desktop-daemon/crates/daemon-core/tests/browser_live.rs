//! Live E2E against REAL Chrome (skips silently if no browser binary).
//!
//! Proves the browser hardening of c6b6f22 on a live page, not on paper:
//!   1. `alert()` no longer freezes the page — the dialog handler dismisses
//!      it and logs the message into the console ring.
//!   2. Same-tab popup policy: `window.open` resolves to a real navigation
//!      in the tracked tab (the injected guard works).
//!   3. Anchor `target=_blank` is captured on click and converted to a
//!      same-tab navigation (anchor activation bypasses `window.open` —
//!      proven live when this test first ran and failed).
//!
//! Skips (prints `browser e2e: skipped …`) when Chrome/Chromium/Edge is not
//! installed, so CI machines without a browser stay green. No network: a
//! data: URL hosts the fixture page.

use chromiumoxide::detection::{default_executable, DetectionOptions};
use daemon_core::browser_ops::{
    BrowserActParams, BrowserLaunchParams, BrowserStore, SnapshotParams, WaitParams,
};

/// Local HTTP server serving `/fixture` (the given body JS) and `/dest`
/// (the popup destination). Chrome blocks top-frame navigation to `data:`
/// URLs, so the same-tab policy needs REAL http:// destinations to prove
/// itself — this is exactly the kind of thing only a live E2E catches.
async fn spawn_fixture_server() -> (String, std::net::SocketAddr) {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut request = String::new();
            let _ = reader.read_line(&mut request);
            let path = request
                .split_whitespace()
                .nth(1)
                .unwrap_or("/fixture")
                .to_string();
            let (status, title, body) = if path.starts_with("/dest") {
                ("200 OK", "opened", "opened-body")
            } else {
                ("200 OK", "sh-e2e-fixture", "")
            };
            let page = format!(
                "<html><head><title>{title}</title></head><body>{body}{}<script>{}</script></body></html>",
                if path.starts_with("/dest") { "" } else { "<button id=\"trigger\">go</button>" },
                if path.starts_with("/dest") {
                    String::new()
                } else if path.contains("dialog=1") {
                    String::from("alert('e2e alert body');")
                } else if path.contains("anchor=1") {
                    String::from(
                        r#"const a = document.createElement('a');
                       a.href = '/dest?via=anchor'; a.target = '_blank';
                       a.textContent = 'go-anchor'; document.body.appendChild(a);"#,
                    )
                } else {
                    String::from(
                        r#"document.getElementById('trigger').addEventListener('click', () => {
                         window.open('/dest?via=windowopen');
                       });"#,
                    )
                },
            );
            let response = format!(
                "HTTP/1.0 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                page.len(),
                page
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{addr}"), addr)
}

fn chrome_available() -> bool {
    default_executable(DetectionOptions::default()).is_ok()
}

/// data: URL fixture. JS: `open_new_tab()` uses window.open (intercepted by
/// the guard); the anchor is plain target=_blank.
fn fixture(body_js: &str) -> String {
    let html = format!(
        "<html><head><title>sh-e2e-fixture</title></head>\
<body>\
<button id=\"trigger\">go</button>\
<script>{body_js}</script>\
</body></html>"
    );
    let b64 = {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = html.as_bytes();
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[((n >> 6) & 0x3f) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[(n & 0x3f) as usize] as char
            } else {
                '='
            });
        }
        out
    };
    format!("data:text/html;base64,{b64}")
}

/// Extract the snapshot ref of the node whose line contains `needle`.
/// The AX snapshot text marks interactive nodes with `[ref=N]`.
fn ref_for(text: &str, needle: &str) -> Option<i64> {
    for line in text.lines() {
        if !line.contains(needle) {
            continue;
        }
        let start = line.find("[ref=")? + 5;
        let digits: String = line[start..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(n) = digits.parse::<i64>() {
            return Some(n);
        }
    }
    None
}

#[tokio::test]
async fn live_browser_dialogs_do_not_freeze_and_popups_stay_in_session() {
    if !chrome_available() {
        eprintln!("browser e2e: skipped — no Chrome/Chromium/Edge binary available");
        return;
    }
    let store = BrowserStore::new();
    let (base, _addr) = spawn_fixture_server().await;

    // -- Scenario A: window.open through the guard → SAME tab navigation
    // over REAL http (data: top-frame navigation is blocked by Chrome).
    let launch = store
        .launch(BrowserLaunchParams {
            url: Some(format!("{base}/fixture")),
            headed: false,
            size: None,
        })
        .await
        .expect("launch should succeed with a real browser");
    assert!(launch.launched);
    let snap_text = launch
        .snapshot
        .as_ref()
        .expect("launch with url returns a snapshot")
        .text
        .clone();
    let btn_ref = ref_for(&snap_text, "go").expect("the button must carry a [ref=N]");

    store
        .act(BrowserActParams {
            element_ref: btn_ref,
            action: "click".into(),
            text: None,
        })
        .await
        .expect("click by snapshot ref should work");

    let wait = store
        .wait(WaitParams {
            timeout_ms: Some(5_000),
            text: Some("opened-body".into()),
            sleep_ms: None,
        })
        .await
        .expect("wait should not time out if the same-tab navigation happened");
    let title = wait
        .snapshot
        .expect("wait returns a fresh snapshot")
        .title;
    assert!(
        title.as_deref() == Some("opened"),
        "window.open must navigate the SAME tracked tab after the guard (title was {title:?})",
    );
    store.close(true).await.expect("close");

    // -- Scenario B: alert() fires during load — the page must stay alive,
    // and SOME drained snapshot must carry what the page asked. The launch
    // snapshot may itself be the one that drains the ring, so it counts too.
    let launch_b = store
        .launch(BrowserLaunchParams {
            url: Some(format!("{base}/fixture?dialog=1")),
            headed: false,
            size: None,
        })
        .await
        .expect("relaunch replaces the previous session");

    // The snapshot itself proves the page is not frozen (GetFullAxTree runs
    // in the page and would not answer while a dialog is open).
    let snap = store
        .snapshot(SnapshotParams { limit: None })
        .await
        .expect("snapshot must answer — the page is not stalled by dialogs");
    let snap2 = store
        .snapshot(SnapshotParams { limit: None })
        .await
        .expect("second snapshot");

    let dialog_text: String = launch_b
        .snapshot
        .iter()
        .flat_map(|s| s.console.iter())
        .chain(snap.console.iter())
        .chain(snap2.console.iter())
        .map(|l| format!("{}|{}", l.level, l.text))
        .collect::<Vec<_>>()
        .join(" ;; ");
    assert!(
        dialog_text.contains("[alert] e2e alert body"),
        "the alert message must be logged for the agent (got: {dialog_text:?})"
    );
    assert!(
        dialog_text.contains("dismissed"),
        "the log must record the dismissal (got: {dialog_text:?})"
    );
    store.close(true).await.expect("close");

    // -- Scenario C: plain <a target="_blank"> — anchor activation bypasses
    // window.open (proven live before the fix), so the guard also intercepts
    // the click in the capture phase and navigates the SAME tab.
    let launch_c = store
        .launch(BrowserLaunchParams {
            url: Some(format!("{base}/fixture?anchor=1")),
            headed: false,
            size: None,
        })
        .await
        .expect("relaunch for scenario C");
    let anchor_ref = ref_for(
        &launch_c
            .snapshot
            .as_ref()
            .expect("scenario C snapshot")
            .text,
        "go-anchor",
    )
    .expect("the anchor must carry a [ref=N]");

    store
        .act(BrowserActParams {
            element_ref: anchor_ref,
            action: "click".into(),
            text: None,
        })
        .await
        .expect("anchor click dispatches");

    let wait_c = store
        .wait(WaitParams {
            timeout_ms: Some(5_000),
            text: Some("opened-body".into()),
            sleep_ms: None,
        })
        .await
        .expect("anchor target=_blank must become a same-tab navigation");
    let title_c = wait_c
        .snapshot
        .expect("wait returns a fresh snapshot")
        .title;
    assert_eq!(
        title_c.as_deref(),
        Some("opened"),
        "anchor target=_blank must be captured and navigated in the SAME tracked tab \
         (no untracked popup, no stale agent tab) — got {title_c:?}"
    );
    store.close(true).await.expect("close");
}

// The fixture helper must never panic on valid ASCII input.
#[test]
fn fixture_encodes_data_url() {
    let url = fixture("document.title='x'");
    assert!(url.starts_with("data:text/html;base64,"));
    assert!(!url.contains('<'), "base64 must not contain raw HTML");
}

// ref_for must find the ref on the SAME line as the node text.
#[test]
fn ref_for_parses_snapshot_markers() {
    let snap = "button \"go\" [ref=12]\ntext \"go\"\nlink \"go-anchor\" [ref=7]";
    assert_eq!(ref_for(snap, "go-anchor"), Some(7));
    assert_eq!(ref_for(snap, "\"go\""), Some(12));
    assert_eq!(ref_for(snap, "absent"), None);
    assert_eq!(ref_for("no markers here", "here"), None);
}
