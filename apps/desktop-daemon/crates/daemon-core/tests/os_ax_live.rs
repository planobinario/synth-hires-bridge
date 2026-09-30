//! Live E2E against the REAL desktop accessibility bus (skips when absent).
//!
//! Proves the `os` slice on a real session — READ-ONLY by design: this test
//! must NEVER press/set_text on the user's real applications, so it stops at
//! observation (apps list + snapshot + render invariants). The action path
//! is covered by os_ax's unit contract (ref→identity, action dispatch
//! semantics) and needs an interactive desktop with a scratch app to be
//! safe — that remains an operator-driven E2E, not CI.
//!
//! Skips (prints `os ax e2e: skipped …`) when there is no session bus /
//! at-spi registry, so CI machines without a desktop stay green.

use daemon_core::os_ax::{OsAxEngine, OsSnapshotParams};

async fn bus_available() -> bool {
    // A session bus address is the minimum; the engine's own probe is the
    // honest check (it may exist but refuse, e.g. ssh -X without at-spi).
    if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err() {
        return false;
    }
    matches!(
        OsAxEngine::new().available().await,
        Ok(daemon_core::os_ax::OsAvailableResult {
            available: true,
            ..
        })
    )
}

#[tokio::test]
async fn live_os_ax_lists_apps_and_snapshots_one_readonly() {
    if !bus_available().await {
        eprintln!("os ax e2e: skipped — no desktop accessibility bus available");
        return;
    }
    let engine = OsAxEngine::new();

    // 1. The bus exposes SOMETHING (a desktop session always has providers:
    //    the shell itself registers on GNOME/KDE).
    let apps = engine.apps().await.expect("apps listing must not fail on a live bus");
    eprintln!("os ax e2e: providers = {:?}", apps.apps.iter().map(|a| &a.name).collect::<Vec<_>>());
    assert!(
        !apps.apps.is_empty(),
        "a live desktop session always exposes at least one accessibility provider"
    );

    // 2. Snapshot the FIRST provider end-to-end (walk + render + refs).
    //    Whatever app it is, the render invariants must hold.
    let first = apps.apps[0].name.clone();
    let snap = engine
        .snapshot(OsSnapshotParams {
            app: first.clone(),
            limit: Some(200),
            focus: None,
        })
        .await
        .expect("snapshot of a listed provider must resolve");
    eprintln!(
        "os ax e2e: app={} root={:?} nodes={} rendered_bytes={}",
        snap.app,
        snap.root_name,
        snap.node_count,
        snap.text.len()
    );
    assert_eq!(snap.app, first);
    assert!(snap.node_count > 0, "a provider's tree is never empty");
    // Refs are dense from 1 (renderer contract the model relies on).
    assert!(snap.text.contains("[ref=1]"), "refs must start at 1:\n{}", snap.text);
    for (n, line) in snap.text.lines().enumerate() {
        let expect = format!("[ref={}", n + 1);
        assert!(line.contains(&expect), "refs must be dense: {line}");
        if n > 400 {
            break; // bounded sanity, not the whole file
        }
    }

    // 3. Focus narrows the window and keeps refs consistent.
    if let Some(line) = snap.text.lines().next() {
        if let Some(name_start) = line.find('"') {
            let name = &line[name_start + 1..];
            if let Some(end) = name.find('"') {
                let needle = &name[..end];
                if needle.len() > 2 {
                    let focused = engine
                        .snapshot(OsSnapshotParams {
                            app: first.clone(),
                            limit: Some(50),
                            focus: Some(needle.to_string()),
                        })
                        .await
                        .expect("focused snapshot");
                    assert!(
                        focused.text.contains(needle),
                        "focus must land the window on the needle"
                    );
                }
            }
        }
    }

    // 4. Unknown app → actionable error, never a panic.
    let err = engine
        .snapshot(OsSnapshotParams {
            app: "no-such-app-xyz".into(),
            limit: None,
            focus: None,
        })
        .await
        .expect_err("unknown app must error");
    assert!(
        err.to_string().contains("desktop_os_apps"),
        "the error must point the model at the discovery verb: {err}"
    );

    // 5. Unknown ref → actionable error (read-only proof of the contract).
    let err = engine
        .act(daemon_core::os_ax::OsActParams {
            element_ref: 9_999,
            action: "press".into(),
            text: None,
        })
        .await
        .expect_err("unknown ref must error");
    assert!(
        err.to_string().contains("desktop_os_snapshot"),
        "the error must point the model at the snapshot verb: {err}"
    );
}
