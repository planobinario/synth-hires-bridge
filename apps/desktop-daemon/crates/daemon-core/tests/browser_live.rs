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
    BrowserActParams, BrowserEvalParams, BrowserLaunchParams, BrowserNavigateParams,
    BrowserStore, MockRule, MockSetParams, SnapshotParams, TabCloseParams, TabOpenParams,
    TabSelectParams, WaitParams,
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
            // Per-route title, body extras and page script.
            let (title, body, extra_body, script): (String, String, Option<String>, String) =
                if path.starts_with("/dest") {
                    ("opened".into(), "opened-body".into(), None, String::new())
                } else if path.starts_with("/api-404.json") {
                    (
                        "404".into(),
                        String::new(),
                        None,
                        String::new(),
                    )
                } else if path.starts_with("/img-404.png") {
                    (
                        "404".into(),
                        String::new(),
                        None,
                        String::new(),
                    )
                } else if path.contains("verbos=1") {
                    (
                        "sh-e2e-fixture".into(),
                        String::new(),
                        Some(String::from("<div id=\"hover-target\" tabindex=\"0\">hover-target</div>")),
                        String::from(
                            r#"
                  window.__hovered = 0; window.__dbl = 0; window.__ctx = 0;
                  window.__shift_tab = 0;
                  const h = document.getElementById('hover-target');
                  h.addEventListener('mouseenter', () => { window.__hovered++; });
                  h.addEventListener('dblclick', () => { window.__dbl++; });
                  h.addEventListener('contextmenu', () => { window.__ctx++; });
                  document.addEventListener('keydown', (e) => {
                    if (e.shiftKey && e.key === 'Tab') window.__shift_tab++;
                  });
                  "#,
                        ),
                    )
                } else if path.contains("netpush=1") {
                    (
                        "sh-e2e-fixture".into(),
                        String::new(),
                        None,
                        String::from(
                            "const bad = document.createElement('img'); bad.src = '/img-404.png';".to_string()
                                + " fetch('/api-404.json').catch(() => {});",
                        ),
                    )
                } else {
                    (
                        "sh-e2e-fixture".into(),
                        String::new(),
                        None,
                        if path.contains("dialog=1") {
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
                    )
                };
            // The 404 fixtures must be REAL 404s — otherwise the network
            // ring would only ever see HTTP 200s and the E2E proves nothing.
            let response_status = if title == "404" { "404 Not Found" } else { "200 OK" };
            let extra = extra_body.as_deref().unwrap_or("");
            let trigger = if path.starts_with("/dest") || path.contains("verbos=1") {
                ""
            } else {
                "<button id=\"trigger\">go</button>"
            };
            let page = format!(
                "<html><head><title>{title}</title></head><body>{body}{trigger}{extra}<script>{script}</script></body></html>"
            );
            let response = format!(
                "HTTP/1.0 {response_status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
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
            modifiers: None,
            button: None,
            click_count: None,
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
        .snapshot(SnapshotParams { limit: None, focus: None, max_nodes: None })
        .await
        .expect("snapshot must answer — the page is not stalled by dialogs");
    let snap2 = store
        .snapshot(SnapshotParams { limit: None, focus: None, max_nodes: None })
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
            modifiers: None,
            button: None,
            click_count: None,
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

// ---------------------------------------------------------------------------
// Live E2E for the parity additions: hover/double/right click + modifiers,
// desktop.browser.eval, the network ring, and back/forward/reload — against
// REAL Chrome over the local HTTP fixture.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn live_browser_verbs_eval_network_and_history() {
    if !chrome_available() {
        eprintln!("browser e2e: skipped — no Chrome/Chromium/Edge binary available");
        return;
    }
    let store = BrowserStore::new();
    let (base, _addr) = spawn_fixture_server().await;

    // -- 1. Fine-grained verbs against the live page.
    store
        .launch(BrowserLaunchParams {
            url: Some(format!("{base}/fixture?verbos=1")),
            headed: false,
            size: None,
        })
        .await
        .expect("launch (verbs fixture)");

    let snap = store
        .snapshot(SnapshotParams { limit: None, focus: None, max_nodes: None })
        .await
        .expect("snapshot for refs");
    let hover_ref = ref_for(&snap.text, "hover-target").expect("hover-target must carry a ref");

    // hover → mouseenter
    store
        .act(BrowserActParams {
            element_ref: hover_ref, action: "hover".into(), text: None,
            modifiers: None, button: None, click_count: None,
        })
        .await
        .expect("hover dispatches");

    // shift+Tab → the document keydown listener counts it (modifiers on press)
    store
        .act(BrowserActParams {
            element_ref: 0, action: "press".into(), text: Some("Tab".into()),
            modifiers: Some(vec!["shift".into()]), button: None, click_count: None,
        })
        .await
        .expect("shift+Tab dispatches");

    // double click + right click on the same target
    store
        .act(BrowserActParams {
            element_ref: hover_ref, action: "click".into(), text: None,
            modifiers: None, button: None, click_count: Some(2),
        })
        .await
        .expect("double click dispatches");
    store
        .act(BrowserActParams {
            element_ref: hover_ref, action: "click".into(), text: None,
            modifiers: None, button: Some("right".into()), click_count: None,
        }
        )
        .await
        .expect("right click dispatches");

    // Verify the page-side counters via the new eval op.
    let hovered = store
        .eval(BrowserEvalParams {
            expression: "window.__hovered + '_' + window.__dbl + '_' + window.__ctx + '_' + window.__shift_tab".into(),
        })
        .await
        .expect("eval must answer");
    assert_eq!(
        hovered.value.as_str(),
        Some("1_1_1_1"),
        "hover + dblclick + contextmenu + shift+Tab must all land on the page"
    );

    // -- 2. Network ring: the page fetched /api-404.json and an /img-404.png;
    // some drained snapshot must record both failures. Collect from EVERY
    // snapshot the daemon hands back —
    // navigate() and wait() each drain the ring into their own result (by
    // design: any snapshot carries what happened since the last one), so
    // the test aggregates them and then polls for stragglers.
    let mut net = String::new();
    {
        let nav = store
            .navigate(BrowserNavigateParams { url: format!("{base}/fixture?netpush=1") })
            .await
            .expect("navigate to netpush fixture");
        for n in &nav.snapshot.network {
            net.push_str(&format!("{}|{}|{} ;; ", n.status, n.resource_type, n.url));
            if let Some(err) = &n.error {
                net.push_str(&format!("error={err} ;; "));
            }
        }
    }
    {
        let w = store
            .wait(WaitParams { timeout_ms: Some(5_000), text: None, sleep_ms: Some(800) })
            .await
            .expect("settle for network events");
        if let Some(s) = &w.snapshot {
            for n in &s.network {
                net.push_str(&format!("{}|{}|{} ;; ", n.status, n.resource_type, n.url));
                if let Some(err) = &n.error {
                    net.push_str(&format!("error={err} ;; "));
                }
            }
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline
        && !(net.contains("api-404.json") && net.contains("img-404.png"))
    {
        let s = store
            .snapshot(SnapshotParams { limit: None, focus: None, max_nodes: None })
            .await
            .expect("net snapshot");
        for n in &s.network {
            net.push_str(&format!("{}|{}|{} ;; ", n.status, n.resource_type, n.url));
            if let Some(err) = &n.error {
                net.push_str(&format!("error={err} ;; "));
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(
        net.contains("api-404.json") && net.contains("HTTP 404"),
        "the XHR 404 must appear in the drained network ring (got: {net:?})"
    );
    assert!(
        net.contains("img-404.png") && net.contains("error="),
        "the failed image load must appear with an errorText (got: {net:?})"
    );

    // -- 3. History verbs: /fixture?verbos=1 → /dest → back → forward,
    // verified via eval of location.pathname; then reload.
    store
        .navigate(BrowserNavigateParams { url: format!("{base}/fixture?verbos=1") })
    .await
        .expect("navigate back to verbos");
    store
        .navigate(BrowserNavigateParams { url: format!("{base}/dest?h=1") })
        .await
        .expect("navigate to dest");
    store
        .act(BrowserActParams {
            element_ref: 0, action: "back".into(), text: None,
            modifiers: None, button: None, click_count: None,
        })
        .await
        .expect("back dispatches");
    let back = store
        .eval(BrowserEvalParams { expression: "location.search".into() })
        .await
        .expect("eval query after back");
    assert!(
        back.value.as_str().unwrap_or("").contains("verbos=1"),
        "back must return to the verbos fixture (got {:?})",
        back.value
    );
    store
        .act(BrowserActParams {
            element_ref: 0, action: "forward".into(), text: None,
            modifiers: None, button: None, click_count: None,
        })
        .await
        .expect("forward dispatches");
    let fwd = store
        .eval(BrowserEvalParams { expression: "location.search".into() })
        .await
        .expect("eval query after forward");
    assert!(
        fwd.value.as_str().unwrap_or("").contains("h=1"),
        "forward must return to /dest?h=1 (got {:?})",
        fwd.value
    );
    store
        .act(BrowserActParams {
            element_ref: 0, action: "reload".into(), text: None,
            modifiers: None, button: None, click_count: None,
        })
        .await
        .expect("reload dispatches");
    let after_reload = store
        .snapshot(SnapshotParams { limit: None, focus: None, max_nodes: None })
        .await
        .expect("snapshot after reload");
    assert_eq!(
        after_reload.title.as_deref(),
        Some("opened"),
        "reload must re-serve the current page"
    );

    // -- 4. Narrow-down: focus slices the tree and reports the match.
    store
        .navigate(BrowserNavigateParams { url: format!("{base}/fixture?verbos=1") })
        .await
        .expect("navigate to verbos for focus");
    let full = store
        .snapshot(SnapshotParams { limit: None, focus: None, max_nodes: None })
        .await
        .expect("full snapshot");
    let focused = store
        .snapshot(SnapshotParams {
            limit: None,
            focus: Some("hover-target".into()),
            max_nodes: Some(60),
        })
        .await
        .expect("focused snapshot");
    assert_eq!(
        focused.focus_matched,
        Some(full.node_count),
        "focus must report the total node count when the needle matches"
    );
    assert!(
        focused.text.contains("hover-target"),
        "the focused window must contain the needle"
    );
    assert!(
        focused.text.lines().count() <= full.text.lines().count(),
        "the focused window must not be larger than the full snapshot"
    );
    let miss = store
        .snapshot(SnapshotParams {
            limit: None,
            focus: Some("no-such-node-xyz".into()),
            max_nodes: None,
        })
        .await
        .expect("missed focus snapshot");
    assert!(
        miss.focus_matched.is_none(),
        "a needle that matches nothing must report focus_matched: None"
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

// ─── Multi-tab + route mocks ────────────────────────────────────────────────

#[tokio::test]
async fn live_browser_multitab_select_and_close_last_teardown() {
    if !chrome_available() {
        println!("browser e2e: skipped (no chrome/chromium/edge)");
        return;
    }
    let store = BrowserStore::new();
    let (base, _addr) = spawn_fixture_server().await;

    store
        .launch(BrowserLaunchParams { url: Some(format!("{base}/fixture?tabs=1")), headed: false, size: None })
        .await
        .expect("launch (tabs fixture)");

    // Open a second tab whose page announces its own tab id.
    let opened = store
        .tab_open(TabOpenParams { url: Some(format!("{base}/dest?tab=second")), tab_id: Some("second".into()) })
        .await
        .expect("tab_open");
    assert_eq!(opened.tab_id, "second");
    let listed = store.tab_list().await.expect("tab_list after open");
    assert_eq!(listed.active_tab, "second", "open selects the new tab");
    assert!(listed.tabs.contains(&"main".to_string()) && listed.tabs.contains(&"second".to_string()));

    // Refs from another tab never leak across a select.
    store
        .snapshot(SnapshotParams { limit: None, focus: None, max_nodes: None })
        .await
        .expect("snapshot second tab");
    let sel = store
        .tab_select(TabSelectParams { tab_id: "main".into() })
        .await
        .expect("tab_select main");
    assert_eq!(sel.active_tab, "main");

    // Closing a non-active tab keeps the session alive.
    let closed = store
        .tab_close(TabCloseParams { tab_id: "second".into() })
        .await
        .expect("tab_close second");
    assert!(closed.closed);
    assert_eq!(closed.active_tab, "main", "active tab survives non-active close");
    assert!(!closed.tabs.contains(&"second".to_string()));

    // Last tab closed = full teardown: subsequent ops say NoSession.
    let closed = store
        .tab_close(TabCloseParams { tab_id: "main".into() })
        .await
        .expect("tab_close main");
    assert_eq!(closed.active_tab, "", "last close ends the session");
    assert!(store.tab_list().await.is_err(), "session must be gone after last close");
}

#[tokio::test]
async fn live_browser_route_mock_serves_locally_and_passes_through() {
    if !chrome_available() {
        println!("browser e2e: skipped (no chrome/chromium/edge)");
        return;
    }
    let store = BrowserStore::new();
    let (base, _addr) = spawn_fixture_server().await;

    store
        .launch(BrowserLaunchParams { url: Some(format!("{base}/fixture?mock=1")), headed: false, size: None })
        .await
        .expect("launch (mock fixture)");

    // Rule 1: /api/mock-me → local 200 with fixed JSON.
    // Rule 2: /api/boom → local 500. Both WITHOUT touching the fixture server.
    store
        .mock_set(MockSetParams {
            rules: Some(vec![
                MockRule {
                    url_contains: "/api/mock-me".into(),
                    status: 200,
                    body: r#"{"mocked":true,"value":42}"#.into(),
                    content_type: Some("application/json".into()),
                },
                MockRule {
                    url_contains: "/api/boom".into(),
                    status: 500,
                    body: r#"{"error":"boom"}"#.into(),
                    content_type: Some("application/json".into()),
                },
            ]),
        })
        .await
        .expect("mock_set");

    // eval fetches BOTH mocked URLs and the REAL one (pass-through still
    // works). The fixture's 404 route serves HTML, so the real request is
    // checked by status + body shape, not .json().
    let probe = r#"(async () => {
        const ok = await (await fetch('/api/mock-me')).json();
        let boomStatus = 0;
        try {
            const r = await fetch('/api/boom');
            boomStatus = r.status;
        } catch (e) { boomStatus = -1; }
        const realR = await fetch('/api-404.json');
        const realText = await realR.text();
        return { ok, boomStatus, realStatus: realR.status, realIsHtml: realText.includes('<html') };
    })()"#;

    let waited = store
        .wait(WaitParams { timeout_ms: Some(8_000), text: None, sleep_ms: Some(1_200) })
        .await
        .expect("wait for fetches");
    let _ = waited;
    let result = store
        .eval(BrowserEvalParams { expression: probe.into() })
        .await
        .expect("eval probe");
    let v = result.value;
    assert_eq!(v["ok"]["mocked"], serde_json::json!(true), "mocked 200 served locally");
    assert_eq!(v["ok"]["value"], serde_json::json!(42));
    assert_eq!(v["boomStatus"], serde_json::json!(500), "mocked 500 reached the page");
    assert_eq!(v["realStatus"], serde_json::json!(404), "real request passed through (no hang)");
    assert_eq!(v["realIsHtml"], serde_json::json!(true), "real response body came from the network");
}
