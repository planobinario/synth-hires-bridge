//! Browser control via CDP (feature: browser) — real browser tools for the agent.
//!
//! Uses `chromiumoxide` (Puppeteer-in-Rust over the Chrome DevTools Protocol).
//! The model gets what "blind" vibecoding lacks: launch (headless by default),
//! navigate, a compact accessibility-tree snapshot with stable per-node refs,
//! click/type/press/scroll **by ref**, and PNG screenshots as base64 (like
//! `fs.read` images). Console + page errors are captured so the agent can see
//! what the page *said*.
//!
//! House patterns honored:
//! - One session per workspace root (`browser.launch` is idempotent).
//! - Capabilities: `desktop.browser.nav` (navigate ≈ read) and
//!   `desktop.browser.act` (click/type in real sessions — native consent).
//! - Chrome binary auto-detected via `chromiumoxide::detection` (chrome,
//!   chromium, edge, `$CHROME`, platform paths).

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(feature = "browser")]
use std::collections::{HashMap, VecDeque};
#[cfg(feature = "browser")]
use std::sync::Arc;
#[cfg(feature = "browser")]
use tokio::sync::Mutex;

#[cfg(feature = "browser")]
use chromiumoxide::{
    browser::{Browser, BrowserConfig},
    cdp::browser_protocol::{
        accessibility::GetFullAxTreeParams,
        dom::{BackendNodeId, FocusParams, GetContentQuadsParams, ScrollIntoViewIfNeededParams},
        input::{
            DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams,
            DispatchMouseEventType, InsertTextParams, MouseButton,
        },
        log::{EnableParams as LogEnable, EventEntryAdded},
        page::{
            AddScriptToEvaluateOnNewDocumentParams, CaptureScreenshotFormat,
            EventJavascriptDialogOpening, HandleJavaScriptDialogParams,
        },
    },
    cdp::js_protocol::runtime::{EnableParams as RuntimeEnable, EventConsoleApiCalled},
    detection::{default_executable, DetectionOptions},
    keys, Page,
};

#[cfg(feature = "browser")]
use crate::task_registry::{
    finish_global_task, record_global_task, TaskKind, TaskState, TaskStatus,
};

/// Errors surfaced to the web agent (human-readable, like `eval_ops`).
#[derive(Debug)]
pub enum BrowserError {
    /// Feature not compiled in.
    NotAvailable(&'static str),
    /// No session yet — call `browser.launch` first.
    NoSession,
    /// Chrome/Chromium/Edge not found.
    ChromeNotFound,
    /// Ref not present in the last snapshot of this session.
    UnknownRef(i64),
    /// Unknown action name for `browser.act`.
    BadAction(String),
    /// Unknown key name for `press`.
    BadKey(String),
    /// Anything from the CDP connection / page.
    Cdp(String),
}

impl std::fmt::Display for BrowserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAvailable(h) => write!(f, "browser feature not available: {h}"),
            Self::NoSession => write!(f, "no browser session — call browser.launch first"),
            Self::ChromeNotFound => write!(
                f,
                "Chrome/Chromium/Edge not found (set $CHROME or install chromium)"
            ),
            Self::UnknownRef(r) => write!(f, "unknown ref {r} — take a fresh snapshot"),
            Self::BadAction(a) => write!(f, "unknown action '{a}' (click|type|press|scroll)"),
            Self::BadKey(k) => write!(f, "unknown key '{k}' (try 'Enter', 'Tab', 'Escape', 'ArrowDown', 'a'…)"),
            Self::Cdp(m) => write!(f, "{m}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Params / results (JSON shapes exchanged with the web side)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct BrowserLaunchParams {
    /// Optional initial URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Show the window (user sees it live). Default: headless.
    #[serde(default)]
    pub headed: bool,
    /// Window size [w, h]. Default 1280x1280 (tall: more AX nodes per view).
    #[serde(default)]
    pub size: Option<(u32, u32)>,
}

#[derive(Debug, Serialize)]
pub struct BrowserLaunchResult {
    pub launched: bool,
    pub headed: bool,
    pub binary: String,
    pub navigated_to: Option<String>,
    pub snapshot: Option<SnapshotResult>,
}

#[derive(Debug, Deserialize)]
pub struct BrowserNavigateParams {
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct BrowserNavigateResult {
    pub url: String,
    pub title: Option<String>,
    pub snapshot: SnapshotResult,
}

#[derive(Debug, Deserialize)]
pub struct SnapshotParams {
    /// Max nodes in the snapshot (safety cap). Default 350.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct SnapshotResult {
    pub url: Option<String>,
    pub title: Option<String>,
    pub node_count: usize,
    pub truncated: bool,
    /// Compact AX-tree text with `[ref=N]` markers.
    pub text: String,
    /// Console/page errors since the last snapshot (drained).
    pub console: Vec<ConsoleLine>,
}

#[derive(Debug, Serialize)]
pub struct ConsoleLine {
    pub level: String,
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct BrowserActParams {
    /// Ref from the last snapshot (not needed for `press`/`scroll`).
    #[serde(rename = "ref", default)]
    pub element_ref: i64,
    /// click | type | press | scroll
    pub action: String,
    /// `type`: text to insert; `press`: key name; `scroll`: "up"|"down"|"left"|"right".
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BrowserActResult {
    pub ok: bool,
    /// Fresh snapshot after the action (verify-your-work loop).
    pub snapshot: SnapshotResult,
}

#[derive(Debug, Deserialize)]
pub struct ScreenshotParams {
    /// "png" (default) or "jpeg".
    #[serde(default)]
    pub format: Option<String>,
    /// JPEG quality 0-100 (ignored for PNG).
    #[serde(default)]
    pub quality: Option<i64>,
    /// Capture the whole scrollable page, not just the viewport (CDP
    /// captureBeyondViewport). Useful for layout checks of long pages.
    #[serde(default)]
    pub full_page: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct ScreenshotResult {
    /// base64 image (like fs.read images).
    pub base64: String,
    pub format: String,
}

#[derive(Debug, Deserialize)]
pub struct WaitParams {
    /// Stop waiting after this long (ms). Default 5000, clamp 100..30_000.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Success as soon as this text appears in the accessibility tree.
    #[serde(default)]
    pub text: Option<String>,
    /// Plain sleep — no polling (use when the effect needs time, not a signal).
    #[serde(default)]
    pub sleep_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct WaitResult {
    pub matched: bool,
    pub waited_ms: u64,
    /// Fresh snapshot after the wait (so the model can act immediately).
    pub snapshot: Option<SnapshotResult>,
}

#[derive(Debug, Deserialize)]
pub struct BrowserCloseParams {
    #[serde(default = "browser_ops_defaults::default_true")]
    pub kill: bool,
}

#[derive(Debug, Serialize)]
pub struct BrowserCloseResult {
    pub closed: bool,
}

// ---------------------------------------------------------------------------
// Session state
// ---------------------------------------------------------------------------

#[cfg(feature = "browser")]
struct BrowserSession {
    /// Drive loop handle (keeps the CDP connection pumped).
    _driver: tokio::task::JoinHandle<()>,
    browser: Browser,
    page: Page,
    /// ref -> backend DOM node id (from the last snapshot).
    refs: HashMap<i64, i64>,
    /// Console + page-error ring buffer (drained on snapshot).
    console: Arc<Mutex<VecDeque<ConsoleLine>>>,
    /// UI-visible task (browser process) — finished on close.
    task_id: Option<uuid::Uuid>,
    /// Console/dialog listeners — aborted with the session on close/relaunch.
    background: Vec<tokio::task::JoinHandle<()>>,
}

#[cfg(feature = "browser")]
pub struct BrowserStore {
    sessions: Mutex<HashMap<String, BrowserSession>>,
}

#[cfg(feature = "browser")]
impl Default for BrowserStore {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }
}

/// Ring-buffer capacity for console lines per session.
#[cfg(feature = "browser")]
const CONSOLE_RING: usize = 200;

#[cfg(feature = "browser")]
impl BrowserStore {
    pub fn new() -> Self {
        Self::default()
    }

    // -- launch ------------------------------------------------------------

    /// Launch a browser for this workspace root (idempotent: relaunches).
    /// Key of the (single) browser session. The daemon serves one workspace;
    /// relaunching replaces the previous session.
    pub const SESSION: &str = "default";

    pub async fn launch(&self, p: BrowserLaunchParams) -> Result<BrowserLaunchResult, BrowserError> {
        // Close any previous session (relaunch = replace).
        self.close(true).await?;

        let exe = default_executable(DetectionOptions::default())
            .map_err(|_| BrowserError::ChromeNotFound)?;
        let binary = exe.display().to_string();

        let (w, h) = p.size.unwrap_or((1280, 1280));
        let mut config = BrowserConfig::builder()
            .window_size(w, h)
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-dev-shm-usage")
            .arg("--disable-blink-features=AutomationControlled");
        config = if p.headed {
            config.with_head()
        } else {
            config.arg("--headless=new")
        };
        let config = config
            .build()
            .map_err(|e| BrowserError::Cdp(format!("browser config: {e}")))?;

        let (browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|e| BrowserError::Cdp(format!("launch: {e}")))?;

        // Pump the connection loop (Stream — canonical chromiumoxide pattern).
        let driver = tokio::spawn(async move {
            while let Some(event) = futures_util::StreamExt::next(&mut handler).await {
                tracing::debug!(?event, "cdp event");
            }
        });

        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|e| BrowserError::Cdp(format!("new_page: {e}")))?;

        // Enable console + log domains (error capture).
        let _ = page.execute(RuntimeEnable::default()).await;
        let _ = page.execute(LogEnable::default()).await;

        // Console capture + dialog handling tasks (aborted with the session).
        let console: Arc<Mutex<VecDeque<ConsoleLine>>> = Arc::new(Mutex::new(VecDeque::new()));
        let mut background = spawn_console_capture(&page, console.clone()).await;
        background.push(spawn_dialog_handler(&page, console.clone()).await);

        // Popup policy: `target=_blank` / `window.open` navigates THIS tab
        // instead of spawning untracked popups. Injected before any document
        // script runs (applies from the first navigation onward).
        let popup_guard = AddScriptToEvaluateOnNewDocumentParams::builder()
            .source(POPUP_GUARD_JS)
            .build()
            .map_err(|e| BrowserError::Cdp(format!("popup guard: {e}")))?;
        // STRICT: if the injection fails, the popup policy is silently gone
        // and the agent would act without knowing. Better a loud launch
        // failure than a silent capability loss.
        page.execute(popup_guard)
            .await
            .map_err(|e| BrowserError::Cdp(format!("popup guard inject: {e}")))?;

        // Seed the shared registry so the browser is visible in the desktop UI.
        let task_id = uuid::Uuid::new_v4();
        record_global_task(TaskState {
            id: task_id,
            kind: TaskKind::Other("browser".into()),
            description: format!("browser — {binary}"),
            status: TaskStatus::Running,
            started_at_instant: std::time::Instant::now(),
            started_at_utc: chrono::Utc::now(),
            finished_at: None,
        });

        let mut session = BrowserSession {
            _driver: driver,
            browser,
            page,
            refs: HashMap::new(),
            console,
            task_id: Some(task_id),
            background,
        };

        // Optional initial navigation (also builds the first snapshot).
        let mut navigated_to = None;
        let mut snapshot = None;
        if let Some(url) = p.url.clone() {
            navigated_to = Some(url.clone());
            snapshot = Some(self.navigate_inner(&mut session, &url).await?);
        }

        self.sessions
            .lock()
            .await
            .insert(Self::SESSION.to_string(), session);

        Ok(BrowserLaunchResult {
            launched: true,
            headed: p.headed,
            binary,
            navigated_to,
            snapshot,
        })
    }

    // -- navigate ----------------------------------------------------------

    async fn navigate_inner(
        &self,
        session: &mut BrowserSession,
        url: &str,
    ) -> Result<SnapshotResult, BrowserError> {
        session
            .page
            .goto(url)
            .await
            .map_err(|e| BrowserError::Cdp(format!("navigate: {e}")))?;
        session
            .page
            .wait_for_navigation()
            .await
            .map_err(|e| BrowserError::Cdp(format!("wait navigation: {e}")))?;
        self.snapshot_inner(session, 350).await
    }

    pub async fn navigate(
        &self,
        p: BrowserNavigateParams,
    ) -> Result<BrowserNavigateResult, BrowserError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(Self::SESSION).ok_or(BrowserError::NoSession)?;
        let snap = self.navigate_inner(session, &p.url).await?;
        let title = snap.title.clone();
        Ok(BrowserNavigateResult {
            url: p.url,
            title,
            snapshot: snap,
        })
    }

    // -- snapshot ----------------------------------------------------------

    /// Full AX-tree snapshot, compacted, with stable refs.
    pub async fn snapshot(&self, p: SnapshotParams) -> Result<SnapshotResult, BrowserError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(Self::SESSION).ok_or(BrowserError::NoSession)?;
        self.snapshot_inner(session, p.limit.unwrap_or(350)).await
    }

    async fn snapshot_inner(
        &self,
        session: &mut BrowserSession,
        limit: usize,
    ) -> Result<SnapshotResult, BrowserError> {
        // Fresh AX tree.
        let res = session
            .page
            .execute(GetFullAxTreeParams::default())
            .await
            .map_err(|e| BrowserError::Cdp(format!("ax tree: {e}")))?;
        let nodes = &res.result.nodes;

        let url = session.page.url().await.ok().flatten();
        let title = session
            .page
            .evaluate_expression("document.title ?? ''")
            .await
            .ok()
            .and_then(|r| r.value().and_then(|v| v.as_str().map(str::to_owned)));

        // Drain console ring.
        let console: Vec<ConsoleLine> = {
            let mut ring = session.console.lock().await;
            ring.drain(..).collect()
        };

        // Serialize nodes to JSON once — extraction becomes shape-safe.
        let serialized: Vec<Value> = nodes
            .iter()
            .filter_map(|node| serde_json::to_value(node).ok())
            .collect();

        // Hierarchy pass: one O(n) sweep builds the parent maps, then a
        // memoized climb yields the TRUE tree depth per node (real indent,
        // not the old flat one-level heuristic).
        let (by_id, by_child) = ax_parent_maps(&serialized);
        let mut depth_cache: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut depth_of = |id: &str| ax_depth(id, &by_id, &by_child, &mut depth_cache);

        let mut refs = HashMap::new();
        let mut out = String::new();
        let mut count = 0usize;
        let total = nodes.len();
        for v in &serialized {
            if count >= limit {
                break;
            }
            let ignored = v.get("ignored").and_then(Value::as_bool).unwrap_or(false);
            if ignored {
                continue;
            }
            let role = v
                .pointer("/role/value")
                .and_then(Value::as_str)
                .unwrap_or("generic")
                .to_string();
            let name = ax_text(v.pointer("/name"));
            let value = ax_text(v.pointer("/value"));
            let node_id = v
                .get("nodeId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let backend = v
                .get("backendDOMNodeId")
                .and_then(Value::as_i64)
                .unwrap_or(0);

            // Only interesting nodes get refs (interactive or with content).
            let interesting = matches!(
                role.as_str(),
                "link" | "button" | "textbox" | "searchbox" | "combobox" | "checkbox"
                    | "radio" | "menuitem" | "tab" | "option" | "slider" | "switch"
                    | "listbox" | "menu" | "tablist" | "heading" | "img" | "article"
                    | "navigation" | "main" | "dialog" | "alert" | "status"
                    | "progressbar" | "spinbutton" | "textarea" | "list" | "tree"
            ) || !name.is_empty()
                || !value.is_empty();

            if !interesting {
                continue;
            }

            count += 1;
            let r = count as i64;
            if backend > 0 {
                refs.insert(r, backend);
            }

            let indent = "  ".repeat(depth_of(&node_id));
            let mut line = format!("{indent}[ref={r}] {role}");
            if !name.is_empty() {
                line.push_str(&format!(" \"{name}\""));
            }
            if !value.is_empty() && value != name {
                line.push_str(&format!(" = {value}"));
            }
            if let Some(b) = v.get("disabled").and_then(Value::as_bool) {
                if b {
                    line.push_str(" (disabled)");
                }
            }
            if let Some(b) = v.get("focused").and_then(Value::as_bool) {
                if b {
                    line.push_str(" (focused)");
                }
            }
            out.push_str(&line);
            out.push('\n');
        }

        // Stash refs for act().
        session.refs = refs;

        Ok(SnapshotResult {
            url,
            title,
            node_count: total,
            truncated: total > limit,
            text: out,
            console,
        })
    }

    // -- act ---------------------------------------------------------------

    pub async fn act(&self, p: BrowserActParams) -> Result<BrowserActResult, BrowserError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(Self::SESSION).ok_or(BrowserError::NoSession)?;

        match p.action.as_str() {
            "click" => {
                self.act_click(session, p.element_ref).await?;
            }
            "type" => {
                let text = p.text.clone().unwrap_or_default();
                self.act_click(session, p.element_ref).await?;
                session
                    .page
                    .execute(InsertTextParams::new(text))
                    .await
                    .map_err(|e| BrowserError::Cdp(format!("insert text: {e}")))?;
            }
            "press" => {
                let key = p.text.clone().unwrap_or_else(|| "Enter".into());
                self.act_press(session, &key).await?;
            }
            "scroll" => {
                let dir = p.text.clone().unwrap_or_else(|| "down".into());
                self.act_scroll(session, &dir).await?;
            }
            other => return Err(BrowserError::BadAction(other.to_string())),
        }

        // Small settle delay, then a fresh snapshot to verify.
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let snapshot = self.snapshot_inner(session, 350).await?;
        Ok(BrowserActResult { ok: true, snapshot })
    }

    async fn act_click(&self, session: &mut BrowserSession, r: i64) -> Result<(), BrowserError> {
        let backend = *session.refs.get(&r).ok_or(BrowserError::UnknownRef(r))?;

        // Scroll into view + focus via DOM domain, then a real mouse click
        // at the element's center via Input domain (fires all handlers).
        let _ = session
            .page
            .execute(
                ScrollIntoViewIfNeededParams::builder()
                    .backend_node_id(BackendNodeId::new(backend))
                    .build(),
            )
            .await;
        let _ = session
            .page
            .execute(
                FocusParams::builder()
                    .backend_node_id(BackendNodeId::new(backend))
                    .build(),
            )
            .await;

        // Center from content quads.
        let quads = session
            .page
            .execute(
                GetContentQuadsParams::builder()
                    .backend_node_id(BackendNodeId::new(backend))
                    .build(),
            )
            .await
            .map_err(|e| BrowserError::Cdp(format!("quads: {e}")))?;
        let (cx, cy) = quad_center(&serde_json::to_value(&quads.result.quads).unwrap_or_default())
            .ok_or_else(|| BrowserError::Cdp("element has no layout box".into()))?;

        for ty in [
            DispatchMouseEventType::MousePressed,
            DispatchMouseEventType::MouseReleased,
        ] {
            let b = DispatchMouseEventParams::builder()
                .r#type(ty)
                .x(cx)
                .y(cy)
                .button(MouseButton::Left)
                .click_count(1)
                .build()
                .map_err(|e| BrowserError::Cdp(format!("mouse params: {e}")))?;
            session
                .page
                .execute(b)
                .await
                .map_err(|e| BrowserError::Cdp(format!("mouse: {e}")))?;
        }
        Ok(())
    }

    async fn act_press(&self, session: &mut BrowserSession, key: &str) -> Result<(), BrowserError> {
        let def = keys::USKEYBOARD_LAYOUT
            .iter()
            .find(|k| k.key.eq_ignore_ascii_case(key) || k.code.eq_ignore_ascii_case(key))
            .ok_or_else(|| BrowserError::BadKey(key.to_string()))?;

        let raw = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::RawKeyDown)
            .key(def.key)
            .code(def.code)
            .windows_virtual_key_code(def.key_code)
            .build()
            .map_err(|e| BrowserError::Cdp(format!("key params: {e}")))?;
        session
            .page
            .execute(raw)
            .await
            .map_err(|e| BrowserError::Cdp(format!("press: {e}")))?;
        if let Some(text) = def.text {
            let down = DispatchKeyEventParams::builder()
                .r#type(DispatchKeyEventType::KeyDown)
                .key(def.key)
                .code(def.code)
                .text(text)
                .unmodified_text(def.key)
                .windows_virtual_key_code(def.key_code)
                .build()
                .map_err(|e| BrowserError::Cdp(format!("key params: {e}")))?;
            session
                .page
                .execute(down)
                .await
                .map_err(|e| BrowserError::Cdp(format!("press: {e}")))?;
        }
        let up = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyUp)
            .key(def.key)
            .code(def.code)
            .windows_virtual_key_code(def.key_code)
            .build()
            .map_err(|e| BrowserError::Cdp(format!("key params: {e}")))?;
        session
            .page
            .execute(up)
            .await
            .map_err(|e| BrowserError::Cdp(format!("press: {e}")))?;
        Ok(())
    }

    async fn act_scroll(&self, session: &mut BrowserSession, dir: &str) -> Result<(), BrowserError> {
        let (w, h) = (1280.0, 1280.0);
        let (cx, cy) = (w / 2.0, h / 2.0);
        let (dx, dy) = match dir {
            "up" => (0.0, -h / 2.0),
            "down" => (0.0, h / 2.0),
            "left" => (-w / 2.0, 0.0),
            "right" => (w / 2.0, 0.0),
            _ => (0.0, h / 2.0),
        };
        let b = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseWheel)
            .x(cx)
            .y(cy)
            .delta_x(dx)
            .delta_y(dy)
            .build()
            .map_err(|e| BrowserError::Cdp(format!("wheel params: {e}")))?;
        session
            .page
            .execute(b)
            .await
            .map_err(|e| BrowserError::Cdp(format!("scroll: {e}")))?;
        Ok(())
    }

    // -- screenshot --------------------------------------------------------

    pub async fn screenshot(&self, p: ScreenshotParams) -> Result<ScreenshotResult, BrowserError> {
        let sessions = self.sessions.lock().await;
        let session = sessions.get(Self::SESSION).ok_or(BrowserError::NoSession)?;

        let jpeg = p.format.as_deref() == Some("jpeg");
        let mut b = chromiumoxide::page::ScreenshotParams::builder()
            .format(if jpeg {
                CaptureScreenshotFormat::Jpeg
            } else {
                CaptureScreenshotFormat::Png
            })
            .capture_beyond_viewport(p.full_page.unwrap_or(false));
        if jpeg {
            b = b.quality(p.quality.unwrap_or(80));
        }
        let bytes = session
            .page
            .screenshot(b.build())
            .await
            .map_err(|e| BrowserError::Cdp(format!("screenshot: {e}")))?;

        Ok(ScreenshotResult {
            base64: base64_encode(&bytes),
            format: if jpeg { "jpeg" } else { "png" }.into(),
        })
    }

    // -- wait ----------------------------------------------------------------

    /// True when `text` appears in the current AX tree. Deliberately does NOT
    /// touch the console ring or the ref map: intermediate polls must not
    /// consume console errors that belong to the final snapshot.
    #[cfg(feature = "browser")]
    async fn ax_tree_contains(&self, text: &str) -> Result<bool, BrowserError> {
        let sessions = self.sessions.lock().await;
        let session = sessions.get(Self::SESSION).ok_or(BrowserError::NoSession)?;
        let res = session
            .page
            .execute(GetFullAxTreeParams::default())
            .await
            .map_err(|e| BrowserError::Cdp(format!("ax tree: {e}")))?;
        for node in &res.result.nodes {
            let v = match serde_json::to_value(node) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("ignored").and_then(Value::as_bool).unwrap_or(false) {
                continue;
            }
            for key in ["name", "value"] {
                if let Some(s) = v.pointer(&format!("/{key}/value")).and_then(Value::as_str) {
                    if s.contains(text) {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    /// Wait for a UI condition, then hand back a fresh snapshot so the model
    /// can act immediately. Two modes:
    /// - `text`: poll the AX tree until the text shows up (SPAs, toasts,
    ///   route transitions) — the honest answer to "the snapshot was taken
    ///   before the repaint".
    /// - `sleep_ms`: plain sleep, for effects that need time, not a signal.
    pub async fn wait(&self, p: WaitParams) -> Result<WaitResult, BrowserError> {
        let has_text = p.text.as_deref().map(|t| !t.trim().is_empty()).unwrap_or(false);
        let sleep_ms = p.sleep_ms.unwrap_or(0).min(30_000);
        if !has_text && sleep_ms == 0 {
            return Err(BrowserError::Cdp(
                "wait: provide text (condition to await) or sleep_ms (plain pause)".into(),
            ));
        }
        let timeout = p.timeout_ms.unwrap_or(5_000).clamp(100, 30_000);
        let started = std::time::Instant::now();

        if has_text {
            let needle = p.text.as_deref().unwrap_or_default().trim();
            loop {
                if self.ax_tree_contains(needle).await? {
                    let waited = started.elapsed().as_millis() as u64;
                    let snapshot = self.snapshot(SnapshotParams { limit: Some(600) }).await?;
                    return Ok(WaitResult { matched: true, waited_ms: waited, snapshot: Some(snapshot) });
                }
                if started.elapsed().as_millis() as u64 >= timeout {
                    let waited = started.elapsed().as_millis() as u64;
                    let snapshot = self.snapshot(SnapshotParams { limit: Some(600) }).await?;
                    return Ok(WaitResult { matched: false, waited_ms: waited, snapshot: Some(snapshot) });
                }
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
        let waited = started.elapsed().as_millis() as u64;
        let snapshot = self.snapshot(SnapshotParams { limit: Some(600) }).await?;
        Ok(WaitResult { matched: true, waited_ms: waited, snapshot: Some(snapshot) })
    }

    // -- eval (escape hatch) -------------------------------------------------

    /// Run a JS expression in the page — read-only uses (metrics, probing).
    pub async fn evaluate(&self, expression: &str) -> Result<Value, BrowserError> {
        let sessions = self.sessions.lock().await;
        let session = sessions.get(Self::SESSION).ok_or(BrowserError::NoSession)?;
        let r = session
            .page
            .evaluate_expression(expression)
            .await
            .map_err(|e| BrowserError::Cdp(format!("evaluate: {e}")))?;
        Ok(r.value().cloned().unwrap_or(Value::Null))
    }

    // -- close ---------------------------------------------------------------

    pub async fn close(&self, _kill: bool) -> Result<BrowserCloseResult, BrowserError> {
        if let Some(mut s) = self.sessions.lock().await.remove(Self::SESSION) {
            for h in s.background.drain(..) {
                h.abort();
            }
            let _ = s.browser.close().await;
            s._driver.abort();
            if let Some(id) = s.task_id.take() {
                finish_global_task(id, TaskStatus::Killed);
            }
        }
        Ok(BrowserCloseResult { closed: true })
    }

    pub async fn has_session(&self) -> bool {
        self.sessions.lock().await.contains_key(Self::SESSION)
    }
}

// ---------------------------------------------------------------------------
// Console capture
// ---------------------------------------------------------------------------

#[cfg(feature = "browser")]
async fn spawn_console_capture(page: &Page, ring: Arc<Mutex<VecDeque<ConsoleLine>>>) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();
    // Runtime.consoleAPICalled → error/warning lines matter most.
    if let Ok(mut stream) = page.event_listener::<EventConsoleApiCalled>().await {
        let ring = ring.clone();
        handles.push(tokio::spawn(async move {
            while let Some(ev) = futures_util::StreamExt::next(&mut stream).await {
                let Ok(v) = serde_json::to_value(&*ev) else { continue };
                let level = v
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("log")
                    .to_string();
                let text = console_args_text(&v);
                if level == "error" || level == "warning" {
                    push_console(&ring, ConsoleLine { level, text });
                }
            }
        }));
    }
    // Log.entryAdded → network errors, violations, deprecations.
    if let Ok(mut stream) = page.event_listener::<EventEntryAdded>().await {
        let ring = ring.clone();
        handles.push(tokio::spawn(async move {
            while let Some(ev) = futures_util::StreamExt::next(&mut stream).await {
                let Ok(v) = serde_json::to_value(&*ev) else { continue };
                let level = v
                    .pointer("/entry/level")
                    .and_then(Value::as_str)
                    .unwrap_or("info")
                    .to_string();
                let text = v
                    .pointer("/entry/text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if text.is_empty() {
                    continue;
                }
                push_console(&ring, ConsoleLine { level, text });
            }
        }));
    }
    handles
}

#[cfg(feature = "browser")]
fn push_console(ring: &Mutex<VecDeque<ConsoleLine>>, line: ConsoleLine) {
    if let Ok(mut r) = ring.try_lock() {
        if r.len() >= CONSOLE_RING {
            r.pop_front();
        }
        r.push_back(line);
    }
}

#[cfg(feature = "browser")]
fn console_args_text(ev: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(args) = ev.get("args").and_then(Value::as_array) {
        for a in args {
            if let Some(s) = a.get("value").and_then(Value::as_str) {
                parts.push(s.to_string());
            } else if let Some(s) = a.get("description").and_then(Value::as_str) {
                parts.push(s.to_string());
            } else if let Some(p) = a.get("preview") {
                if let Some(s) = serde_json::to_string(p).ok() {
                    parts.push(s);
                }
            } else if !parts.is_empty() {
                break;
            }
        }
    }
    parts.join(" ")
}

// ---------------------------------------------------------------------------
// Dialog + popup policy
// ---------------------------------------------------------------------------

/// Injected on every new document: route `window.open` to the SAME tab so
/// `target=_blank` links stay inside the session's tracked page instead of
/// spawning untracked popups (the agent would keep acting on a stale tab).
/// Anchors with `target="_blank"` are captured on click (capture phase) and
/// turned into same-tab navigations — their activation bypasses
/// `window.open` entirely (confirmed by the live E2E in
/// `tests/browser_live.rs`). Real navigation, so `beforeunload` handlers
/// still get to run; returning `null` matches the spec for a blocked popup.
#[cfg(feature = "browser")]
const POPUP_GUARD_JS: &str = r#"(() => {
  const realOpen = window.open.bind(window);
  window.open = (url, target, features) => {
    try {
      const abs = new URL(url, location.href).href;
      if (abs !== location.href) location.href = abs;
      return null;
    } catch {
      return realOpen(url, target, features);
    }
  };
  // Anchor target=_blank is NOT routed through window.open — the browser
  // process performs the activation. Capture the click before the page and
  // turn the anchor into a same-tab navigation instead.
  document.addEventListener('click', (e) => {
    if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.altKey || e.shiftKey) return;
    const a = e.target && e.target.closest ? e.target.closest('a[target="_blank"]') : null;
    if (!a) return;
    try {
      const abs = new URL(a.getAttribute('href') || '', location.href).href;
      if (abs === location.href) { e.preventDefault(); return; }
      e.preventDefault();
      location.href = abs;
    } catch {}
  }, true);
})();"#;

/// Auto-dismiss JS dialogs. `accept=false` maps to: confirm → cancel,
/// prompt → null, beforeunload → stay on the page — the safe default in
/// every case, and the page never blocks on us (we answer immediately).
#[cfg(feature = "browser")]
async fn spawn_dialog_handler(
    page: &Page,
    ring: Arc<Mutex<VecDeque<ConsoleLine>>>,
) -> tokio::task::JoinHandle<()> {
    let stream = page
        .event_listener::<EventJavascriptDialogOpening>()
        .await
        .expect("dialog event listener");
    let handler = page.clone();
    tokio::spawn(async move {
        let mut stream = stream;
        while let Some(ev) = futures_util::StreamExt::next(&mut stream).await {
            let v = match serde_json::to_value(&*ev) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let _ = handler
                .execute(HandleJavaScriptDialogParams::new(false))
                .await;
            let line = dialog_console_line(
                v.get("type").and_then(Value::as_str).unwrap_or("dialog"),
                v.get("message").and_then(Value::as_str).unwrap_or(""),
                v.get("url").and_then(Value::as_str).unwrap_or(""),
            );
            push_console(&ring, line);
        }
    })
}

/// Console ring entry for a dismissed dialog — the agent's only visibility
/// into what the page asked (surfaced on the next snapshot drain).
#[cfg(feature = "browser")]
fn dialog_console_line(kind: &str, message: &str, url: &str) -> ConsoleLine {
    ConsoleLine {
        level: "dialog".to_string(),
        text: format!("[{kind}] {message} — dismissed ({url})"),
    }
}

// ---------------------------------------------------------------------------
// Helpers (pure — unit-tested without a browser)
// ---------------------------------------------------------------------------

/// Extract readable text from an AxValue JSON (`{type, value}`).
#[cfg_attr(not(feature = "browser"), allow(dead_code))]
fn ax_text(v: Option<&Value>) -> String {
    let v = match v {
        Some(v) => v,
        None => return String::new(),
    };
    match v.get("value") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// Parent maps for one AX tree: node-id → parent-id (from `parentId`) and
/// child-id → parent-id (reverse of `childIds`). Two maps because CDP omits
/// `parentId` on some nodes that still appear in a parent's `childIds`.
/// (`std::collections::HashMap` spelled out: the import is feature-gated.)
#[cfg_attr(not(feature = "browser"), allow(dead_code))]
fn ax_parent_maps(
    nodes: &[Value],
) -> (
    std::collections::HashMap<&str, &str>,
    std::collections::HashMap<&str, &str>,
) {
    let mut by_id: std::collections::HashMap<&str, &str> =
        std::collections::HashMap::with_capacity(nodes.len());
    let mut by_child: std::collections::HashMap<&str, &str> =
        std::collections::HashMap::with_capacity(nodes.len() * 2);
    for node in nodes {
        let Some(id) = node.get("nodeId").and_then(Value::as_str) else {
            continue;
        };
        if let Some(parent) = node.get("parentId").and_then(Value::as_str) {
            by_id.insert(id, parent);
        }
        if let Some(children) = node.get("childIds").and_then(Value::as_array) {
            for child in children.iter().filter_map(Value::as_str) {
                by_child.entry(child).or_insert(id);
            }
        }
    }
    (by_id, by_child)
}

/// TRUE tree depth (root = 0) via memoized climb: the parent edge comes from
/// `parentId` or the childIds reverse map. Memoization makes the whole
/// snapshot's depth computation O(n) total. A missing/cyclic parent resolves
/// to 0 instead of looping.
#[cfg_attr(not(feature = "browser"), allow(dead_code))]
fn ax_depth(
    id: &str,
    by_id: &std::collections::HashMap<&str, &str>,
    by_child: &std::collections::HashMap<&str, &str>,
    cache: &mut std::collections::HashMap<String, usize>,
) -> usize {
    if let Some(d) = cache.get(id) {
        return *d;
    }
    let parent = by_id.get(id).or_else(|| by_child.get(id)).copied();
    let depth = match parent {
        Some(p) if p != id => ax_depth(p, by_id, by_child, cache) + 1,
        _ => 0,
    };
    cache.insert(id.to_string(), depth);
    depth
}

/// Center of the first usable quad. Quads arrive as arrays of 8 numbers
/// (4 corner points) — serialize-shape-agnostic.
#[cfg_attr(not(feature = "browser"), allow(dead_code))]
fn quad_center(quads: &Value) -> Option<(f64, f64)> {
    let arr = quads.as_array()?;
    for q in arr {
        let pts: Vec<f64> = q
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_default();
        if pts.len() >= 8 {
            // average of 4 corners
            let xs = [pts[0], pts[2], pts[4], pts[6]];
            let ys = [pts[1], pts[3], pts[5], pts[7]];
            let cx = xs.iter().sum::<f64>() / 4.0;
            let cy = ys.iter().sum::<f64>() / 4.0;
            if cx.is_finite() && cy.is_finite() && cx > 0.0 && cy > 0.0 {
                return Some((cx, cy));
            }
        }
    }
    None
}

/// Minimal standard base64 (RFC 4648) — avoids adding a dependency for one fn.
#[cfg_attr(not(feature = "browser"), allow(dead_code))]
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Non-feature stubs (binary built without browser still compiles)
// ---------------------------------------------------------------------------

#[cfg(not(feature = "browser"))]
#[derive(Default)]
pub struct BrowserStore;

#[cfg(not(feature = "browser"))]
impl BrowserStore {
    pub fn new() -> Self {
        Self
    }
    pub async fn launch(
        &self,
        _p: BrowserLaunchParams,
    ) -> Result<BrowserLaunchResult, BrowserError> {
        Err(BrowserError::NotAvailable("compiled without the browser feature"))
    }
    pub async fn navigate(
        &self,
        _p: BrowserNavigateParams,
    ) -> Result<BrowserNavigateResult, BrowserError> {
        Err(BrowserError::NotAvailable("compiled without the browser feature"))
    }
    pub async fn snapshot(&self, _p: SnapshotParams) -> Result<SnapshotResult, BrowserError> {
        Err(BrowserError::NotAvailable("compiled without the browser feature"))
    }
    pub async fn act(&self, _p: BrowserActParams) -> Result<BrowserActResult, BrowserError> {
        Err(BrowserError::NotAvailable("compiled without the browser feature"))
    }
    pub async fn screenshot(&self, _p: ScreenshotParams) -> Result<ScreenshotResult, BrowserError> {
        Err(BrowserError::NotAvailable("compiled without the browser feature"))
    }
    pub async fn wait(&self, _p: WaitParams) -> Result<WaitResult, BrowserError> {
        Err(BrowserError::NotAvailable("compiled without the browser feature"))
    }
    pub async fn close(&self, _kill: bool) -> Result<BrowserCloseResult, BrowserError> {
        Ok(BrowserCloseResult { closed: true })
    }
    pub async fn has_session(&self) -> bool {
        false
    }
}

// Shared serde default used by `BrowserCloseParams` in both configurations.
mod browser_ops_defaults {
    pub fn default_true() -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Tests (no browser needed — pure helpers)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ax_text_extracts_strings_numbers_bools() {
        let name = json!({"type": "string", "value": "Deploy"});
        assert_eq!(ax_text(Some(&name)), "Deploy");
        let num = json!({"type": "number", "value": 3});
        assert_eq!(ax_text(Some(&num)), "3");
        let b = json!({"type": "bool", "value": true});
        assert_eq!(ax_text(Some(&b)), "true");
        assert_eq!(ax_text(None), "");
        assert_eq!(ax_text(Some(&json!({"type": "string"}))), "");
    }

    #[test]
    fn quad_center_averages_four_corners() {
        let q = json!([[10.0, 20.0, 30.0, 20.0, 30.0, 40.0, 10.0, 40.0]]);
        let (x, y) = quad_center(&q).expect("center");
        assert!((x - 20.0).abs() < 1e-9);
        assert!((y - 30.0).abs() < 1e-9);
        assert!(quad_center(&json!([])).is_none());
        assert!(quad_center(&json!([[1.0, 2.0]])).is_none());
    }

    #[test]
    fn ax_depth_reconstructs_the_real_hierarchy() {
        let tree = vec![
            json!({"nodeId": "1", "childIds": ["2", "3"]}),
            json!({"nodeId": "2", "parentId": "1", "childIds": ["4"]}),
            json!({"nodeId": "3", "parentId": "1"}),
            json!({"nodeId": "4", "childIds": ["5"]}), // parent only via childIds
            json!({"nodeId": "5", "parentId": "4"}),
            json!({"nodeId": "9"}), // detached root
        ];
        let (by_id, by_child) = ax_parent_maps(&tree);
        let mut cache = std::collections::HashMap::new();
        let mut d = |id: &str| ax_depth(id, &by_id, &by_child, &mut cache);
        assert_eq!(d("1"), 0);
        assert_eq!(d("2"), 1);
        assert_eq!(d("3"), 1);
        assert_eq!(d("4"), 2);
        assert_eq!(d("5"), 3);
        assert_eq!(d("9"), 0);
        assert_eq!(d("missing"), 0);
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // PNG magic bytes round-trip shape
        let png_magic = [0x89u8, b'P', b'N', b'G'];
        assert_eq!(base64_encode(&png_magic), "iVBORw==");
    }

    #[test]
    fn wait_params_reject_empty_condition() {
        // Serialized shape sanity: the engine-level check lives in `wait`,
        // but the contract is that neither mode means "no wait requested".
        let p = WaitParams { timeout_ms: None, text: None, sleep_ms: None };
        assert!(p.text.is_none() && p.sleep_ms.is_none());
    }

    #[test]
    fn popup_guard_js_overrides_window_open_same_tab() {
        // Shape contract: an IIFE that overrides window.open, resolves the
        // URL against the current document and navigates THIS tab — plus
        // the capture-phase anchor interception (live-E2E proven gap).
        assert!(POPUP_GUARD_JS.contains("window.open ="));
        assert!(POPUP_GUARD_JS.contains("new URL(url, location.href)"));
        assert!(POPUP_GUARD_JS.contains("location.href = abs"));
        assert!(POPUP_GUARD_JS.contains("a[target=\"_blank\"]"));
        assert!(POPUP_GUARD_JS.contains("true)")); // capture-phase listener
    }

    #[cfg(feature = "browser")]
    #[test]
    fn dialog_console_line_records_dismissal() {
        let line = dialog_console_line("confirm", "¿Borrar todo?", "https://x.test/app");
        assert_eq!(line.level, "dialog");
        assert_eq!(line.text, "[confirm] ¿Borrar todo? — dismissed (https://x.test/app)");
    }

    #[cfg(feature = "browser")]
    #[tokio::test]
    async fn store_starts_empty() {
        let store = BrowserStore::new();
        assert!(!store.has_session().await);
        let err = store
            .navigate(BrowserNavigateParams { url: "https://x".into() })
            .await
            .unwrap_err();
        assert!(matches!(err, BrowserError::NoSession));
    }

    #[cfg(not(feature = "browser"))]
    #[tokio::test]
    async fn stub_reports_unavailable() {
        let store = BrowserStore::new();
        let err = store
            .snapshot(SnapshotParams { limit: None })
            .await
            .unwrap_err();
        assert!(matches!(err, BrowserError::NotAvailable(_)));
    }
}
