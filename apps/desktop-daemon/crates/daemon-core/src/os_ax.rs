//! Native desktop accessibility (feature: `os`) — REAL computer use.
//!
//! The browser slice gives the agent an AX tree for ONE app (Chrome) over
//! CDP. This module gives it the REST of the desktop: any application that
//! exposes its accessibility tree to the OS — buttons, menus, text fields,
//! checkboxes — with SEMANTIC actions (press, set text, select) instead of
//! synthetic mouse/keyboard events. The Codex/Sky methodology, applied at
//! the OS tier:
//!
//! - Perception: the platform accessibility tree (AT-SPI on Linux, AXUIElement
//!   on macOS, UI Automation on Windows), rendered compact and referenceable.
//! - Grounding: element refs from the last snapshot (`[ref=N]`), never pixel
//!   coordinates.
//! - Actions: native accessibility actions (AT-SPI `DoAction`,
//!   `SetTextContents`, `SelectChild`) — no focus stealing, no input
//!   injection, background-safe by design.
//!
//! House patterns honored:
//! - One shared engine behind `WsClient` (lazy connection per backend).
//! - Capabilities: `desktop.os.snapshot` (read) and `desktop.os.act` (write —
//!   same consent tier as shell; mapped in ws_client like browser.act).
//! - The tree format mirrors browser_ops (`[ref=N] role "name"` + states +
//!   native actions), so the model transfers knowledge between tiers.
//!
//! Platform support:
//! - Linux: full (atspi/zbus — pure Rust, no C dependencies).
//! - macOS: slot — `accessibility`-crate backend behind the same shape.
//! - Windows: slot — UI Automation backend behind the same shape.

use serde::{Deserialize, Serialize};

/// Unit params for the two verbs that take none. Empty structs (not `()`):
/// serde deserializes `{}` into them, and the web always sends a params map.
#[derive(Debug, Deserialize)]
pub struct OsAvailableParams {}

#[derive(Debug, Deserialize)]
pub struct OsAppsParams {}

/// Snapshot of one application's accessibility tree, agent-facing.
#[derive(Debug, Serialize)]
pub struct OsSnapshotResult {
    /// D-Bus destination (Linux) of the inspected application.
    pub app: String,
    /// Application root name, when the provider exposes one.
    pub root_name: Option<String>,
    /// Number of nodes in the FULL tree (before the render cap).
    pub node_count: usize,
    /// True when the render cap cut the text (raise `limit` / use `focus`).
    pub truncated: bool,
    /// Compact AX-tree text with `[ref=N]` markers (same format as browser).
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct OsSnapshotParams {
    /// Target application. On Linux: the D-Bus destination (e.g.
    /// `org.gnome.Nautilus`) or a distinctive suffix (`nautilus`).
    pub app: String,
    /// Max RENDERED nodes (safety cap). Default 300. The walk always reads
    /// the whole tree; this bounds only the agent-facing text.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Narrow-down: start the rendered window at the first node whose
    /// name contains this text (case-insensitive).
    #[serde(default)]
    pub focus: Option<String>,
}

/// One accessible application on the platform's accessibility bus.
#[derive(Debug, Serialize)]
pub struct OsAppEntry {
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct OsAppsResult {
    pub apps: Vec<OsAppEntry>,
}

#[derive(Debug, Deserialize)]
pub struct OsActParams {
    /// Ref from the last `desktop.os.snapshot` of this session.
    #[serde(rename = "ref")]
    pub element_ref: i64,
    /// press | set_text | select
    pub action: String,
    /// `set_text`: replacement text. `select`: child index ("2").
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OsActResult {
    pub ok: bool,
    pub action: String,
    /// What the element answered (native action executed, confirmation…).
    pub detail: String,
    /// Fresh snapshot so the model can verify its work.
    pub snapshot: OsSnapshotResult,
}

#[derive(Debug, Serialize)]
pub struct OsAvailableResult {
    pub available: bool,
    pub backend: &'static str,
    /// Hint for the operator when the bus is not there.
    pub hint: Option<String>,
}

/// Errors surfaced to the web agent (human-readable, like `BrowserError`).
#[derive(Debug)]
pub enum OsError {
    /// Feature not compiled in.
    NotAvailable(&'static str),
    /// The platform bus/service is not there (headless box, no AT-SPI…).
    BusUnavailable(String),
    /// Named application not on the bus.
    UnknownApp(String),
    /// Ref not present in the last snapshot of this session.
    UnknownRef(i64),
    /// Unknown action name / bad params for `desktop.os.act`.
    BadAction(String),
    /// The element rejected the action (no native actions, read-only…).
    Rejected(String),
    /// Anything from the platform accessibility stack.
    Atspi(String),
}

impl std::fmt::Display for OsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAvailable(h) => write!(f, "os feature not available: {h}"),
            Self::BusUnavailable(m) => write!(
                f,
                "accessibility bus unavailable: {m} (is a desktop session running? at-spi bus?)"
            ),
            Self::UnknownApp(a) => write!(
                f,
                "no accessibility provider named {a:?} on the bus — call desktop_os_apps first"
            ),
            Self::UnknownRef(r) => {
                write!(f, "unknown ref {r} — take a fresh desktop_os_snapshot")
            }
            Self::BadAction(a) => write!(f, "unknown action '{a}' (press | set_text | select)"),
            Self::Rejected(m) => write!(f, "element rejected the action: {m}"),
            Self::Atspi(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for OsError {}

#[cfg(all(target_os = "linux", feature = "os"))]
impl From<zbus::Error> for OsError {
    fn from(e: zbus::Error) -> Self {
        linux_backend::map_err_public(e)
    }
}

// ---------------------------------------------------------------------------
// Platform-neutral state machine (testable without a desktop session)
// ---------------------------------------------------------------------------

/// One node of the walk, platform-rendered: everything the TEXT renderer
/// needs, extracted once from the native stack. (The engine consumes these
/// behind the `os` feature; without it only the tests do — same pattern as
/// browser_ops' helpers.)
#[cfg_attr(not(feature = "os"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OsNode {
    pub role: String,
    pub name: String,
    pub states: OsNodeStates,
    /// Native action names, in provider order (index 0 == default action).
    pub actions: Vec<String>,
    /// The node's platform identity (Linux: "destination|object path").
    pub identity: String,
}

#[cfg_attr(not(feature = "os"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OsNodeStates {
    pub enabled: bool,
    pub focused: bool,
    pub checked: bool,
    pub expanded: Option<bool>,
    pub editable: bool,
}

#[cfg_attr(not(feature = "os"), allow(dead_code))]
impl Default for OsNodeStates {
    /// Optimistic-by-default: many providers simply OMIT the Enabled state
    /// instead of asserting it (AT-SPI semantics), so "unknown" must render
    /// as enabled — only an explicit negative is a real [disabled].
    fn default() -> Self {
        Self {
            enabled: true,
            focused: false,
            checked: false,
            expanded: None,
            editable: false,
        }
    }
}

/// Roles that always earn a line (same spirit as browser INTERESTING_ROLES;
/// native stacks use richer role vocabularies so matching is looser).
#[cfg_attr(not(feature = "os"), allow(dead_code))]
pub(crate) const INTERESTING_ROLE_PATTERNS: &[&str] = &[
    "button", "check", "radio", "menu", "item", "entry", "text", "label", "link", "tab",
    "toggle", "switch", "slider", "combo", "list", "tree", "table", "dialog", "alert",
    "notification", "window", "frame", "panel", "push", "spin", "progress", "status",
    "page", "scroll", "option",
];

/// Is this role interesting enough to render? Name-less containers are the
/// noise floor of native trees; a named anything is content.
#[cfg_attr(not(feature = "os"), allow(dead_code))]
pub(crate) fn role_is_interesting(role: &str, name: &str) -> bool {
    if !name.trim().is_empty() {
        return true;
    }
    let lower = role.to_lowercase();
    INTERESTING_ROLE_PATTERNS.iter().any(|p| lower.contains(p))
}

/// Render ONE line: `[ref=N] role "name"` + states + `(actions)`.
/// `max_name` caps the name (density: an entry can hold a whole document).
#[cfg_attr(not(feature = "os"), allow(dead_code))]
pub(crate) fn render_node_line(r: i64, node: &OsNode, max_name: usize) -> String {
    let mut line = format!(
        "[ref={r}] {} \"{}\"",
        node.role,
        cap_text(&node.name, max_name)
    );
    if !node.states.enabled {
        line.push_str(" [disabled]");
    }
    if node.states.focused {
        line.push_str(" [focused]");
    }
    if node.states.checked {
        line.push_str(" [checked]");
    }
    if let Some(exp) = node.states.expanded {
        line.push_str(if exp { " [expanded]" } else { " [collapsed]" });
    }
    if node.states.editable {
        line.push_str(" [editable]");
    }
    if !node.actions.is_empty() {
        line.push_str(&format!(" ({})", node.actions.join(", ")));
    }
    line
}

/// Hard cap for one rendered string, with an honest ellipsis marker.
#[cfg_attr(not(feature = "os"), allow(dead_code))]
pub(crate) fn cap_text(s: &str, max: usize) -> std::borrow::Cow<'_, str> {
    if s.chars().count() <= max {
        return std::borrow::Cow::Borrowed(s);
    }
    let cut: usize = s.chars().take(max).map(char::len_utf8).sum();
    std::borrow::Cow::Owned(format!("{}…", &s[..cut]))
}

/// Build the agent-facing snapshot text from the walked nodes (already in
/// DFS order). Applies `focus` (window at the first name hit, keeping a
/// little preceding context), then renders up to `limit` interesting nodes,
/// assigning refs in render order. Returns (text, identities, total, truncated).
#[cfg_attr(not(feature = "os"), allow(dead_code))]
pub(crate) fn render_tree(
    nodes: &[OsNode],
    limit: usize,
    focus: Option<&str>,
) -> (String, Vec<String>, usize, bool) {
    let mut start = 0usize;
    if let Some(needle) = focus
        .map(|f| f.trim().to_lowercase())
        .filter(|f| !f.is_empty())
    {
        if let Some(hit) = nodes
            .iter()
            .position(|n| n.name.to_lowercase().contains(&needle))
        {
            start = hit.saturating_sub(3);
        }
        // No hit → fall through with the whole tree (caller checks refs).
    }
    let mut out = String::new();
    let mut refs = Vec::new();
    let mut count = 0usize;
    for node in &nodes[start..] {
        if count >= limit {
            return (out, refs, nodes.len(), true);
        }
        if !role_is_interesting(&node.role, &node.name) {
            continue;
        }
        count += 1;
        let r = count as i64;
        refs.push(node.identity.clone());
        out.push_str(&render_node_line(r, node, 160));
        out.push('\n');
    }
    (out, refs, nodes.len(), false)
}

/// Refs are stable IDENTITIES (Linux: destination + object path), not walk
/// indices — a relayout between snapshot and act keeps refs valid while the
/// object lives. Scoped per app: act() resolves ref → (app, identity).
#[cfg_attr(not(feature = "os"), allow(dead_code))]
pub(crate) struct OsRefs {
    identity_by_ref: std::collections::HashMap<i64, String>,
}

#[cfg_attr(not(feature = "os"), allow(dead_code))]
impl OsRefs {
    pub(crate) fn new(refs: Vec<String>) -> Self {
        let identity_by_ref = refs
            .into_iter()
            .enumerate()
            .map(|(i, id)| ((i + 1) as i64, id))
            .collect();
        Self { identity_by_ref }
    }
    pub(crate) fn get(&self, r: i64) -> Option<&str> {
        self.identity_by_ref.get(&r).map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

#[cfg(feature = "os")]
pub struct OsAxEngine {
    /// Lazy platform connection (Linux AT-SPI today).
    inner: tokio::sync::Mutex<Option<crate::os_ax::linux_backend::LinuxConn>>,
    /// Last snapshot per app: ref → identity (valid across relayouts).
    sessions: tokio::sync::Mutex<std::collections::HashMap<String, OsRefs>>,
}

#[cfg(feature = "os")]
impl Default for OsAxEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "os")]
impl OsAxEngine {
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::Mutex::new(None),
            sessions: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub const BACKEND_NAME: &'static str = "atspi";

    /// Liveness probe: compiles to the feature, but reports honestly when
    /// the machine has no accessibility bus (headless, CI).
    pub async fn available(&self) -> Result<OsAvailableResult, OsError> {
        match self.ensure_backend().await {
            Ok(()) => Ok(OsAvailableResult {
                available: true,
                backend: Self::BACKEND_NAME,
                hint: None,
            }),
            Err(OsError::BusUnavailable(m)) => Ok(OsAvailableResult {
                available: false,
                backend: Self::BACKEND_NAME,
                hint: Some(m),
            }),
            Err(e) => Err(e),
        }
    }

    /// All accessibility providers on the bus (application roots).
    pub async fn apps(&self) -> Result<OsAppsResult, OsError> {
        let mut guard = self.inner.lock().await;
        let conn = Self::ensure_backend_inner(&mut guard).await?;
        let names = crate::os_ax::linux_backend::list_apps(conn).await?;
        Ok(OsAppsResult {
            apps: names.into_iter().map(|name| OsAppEntry { name }).collect(),
        })
    }

    pub async fn snapshot(&self, p: OsSnapshotParams) -> Result<OsSnapshotResult, OsError> {
        let app = p.app.trim().to_string();
        if app.is_empty() || app.eq_ignore_ascii_case("list") {
            return Err(OsError::BadAction(
                "snapshot needs an app name — call desktop_os_apps first".into(),
            ));
        }
        let mut guard = self.inner.lock().await;
        let conn = Self::ensure_backend_inner(&mut guard).await?;
        let (root_name, nodes) = crate::os_ax::linux_backend::walk_app(conn, &app).await?;
        let limit = p.limit.unwrap_or(300).clamp(20, 2_000);
        let (text, refs, node_count, truncated) =
            render_tree(&nodes, limit, p.focus.as_deref());
        self.sessions
            .lock()
            .await
            .insert(app.clone(), OsRefs::new(refs));
        Ok(OsSnapshotResult {
            app,
            root_name,
            node_count,
            truncated,
            text,
        })
    }

    pub async fn act(&self, p: OsActParams) -> Result<OsActResult, OsError> {
        let action = p.action.as_str();
        if !matches!(action, "press" | "set_text" | "select") {
            return Err(OsError::BadAction(action.to_string()));
        }
        if action == "set_text" && p.text.as_deref().unwrap_or("").is_empty() {
            return Err(OsError::BadAction("set_text needs `text`".into()));
        }
        if action == "select" && p.text.is_none() {
            return Err(OsError::BadAction(
                "select needs the child INDEX in `text` (e.g. \"2\")".into(),
            ));
        }
        let mut guard = self.inner.lock().await;
        let conn = Self::ensure_backend_inner(&mut guard).await?;

        // Resolve the ref against EVERY app's last snapshot (refs are only
        // produced by snapshots, so a hit pins both the app and the node).
        let mut sessions = self.sessions.lock().await;
        let mut owner: Option<(String, String)> = None;
        for (app, refs) in sessions.iter() {
            if let Some(id) = refs.get(p.element_ref) {
                owner = Some((app.clone(), id.to_string()));
                break;
            }
        }
        let (app, identity) = owner.ok_or(OsError::UnknownRef(p.element_ref))?;

        let detail = crate::os_ax::linux_backend::do_action_by_identity(
            conn, &identity, action, p.text.as_deref(),
        )
        .await?;

        // Verification snapshot (same app), so the model sees the effect.
        let (root_name, nodes) = crate::os_ax::linux_backend::walk_app(conn, &app).await?;
        let (text, refs, node_count, truncated) = render_tree(&nodes, 300, None);
        sessions.insert(app.clone(), OsRefs::new(refs));
        Ok(OsActResult {
            ok: true,
            action: action.to_string(),
            detail,
            snapshot: OsSnapshotResult {
                app,
                root_name,
                node_count,
                truncated,
                text,
            },
        })
    }

    async fn ensure_backend(&self) -> Result<(), OsError> {
        let mut guard = self.inner.lock().await;
        Self::ensure_backend_inner(&mut guard).await.map(|_| ())
    }

    async fn ensure_backend_inner(
        guard: &mut Option<crate::os_ax::linux_backend::LinuxConn>,
    ) -> Result<&crate::os_ax::linux_backend::LinuxConn, OsError> {
        if guard.is_none() {
            *guard = Some(crate::os_ax::linux_backend::connect().await?);
        }
        Ok(guard.as_ref().expect("just ensured"))
    }
}

// ---------------------------------------------------------------------------
// Non-feature stub (binary built without `os` still compiles)
// ---------------------------------------------------------------------------

#[cfg(not(feature = "os"))]
#[derive(Default)]
pub struct OsAxEngine;

#[cfg(not(feature = "os"))]
impl OsAxEngine {
    pub fn new() -> Self {
        Self
    }
    pub async fn available(&self) -> Result<OsAvailableResult, OsError> {
        Err(OsError::NotAvailable("compiled without the os feature"))
    }
    pub async fn apps(&self) -> Result<OsAppsResult, OsError> {
        Err(OsError::NotAvailable("compiled without the os feature"))
    }
    pub async fn snapshot(&self, _p: OsSnapshotParams) -> Result<OsSnapshotResult, OsError> {
        Err(OsError::NotAvailable("compiled without the os feature"))
    }
    pub async fn act(&self, _p: OsActParams) -> Result<OsActResult, OsError> {
        Err(OsError::NotAvailable("compiled without the os feature"))
    }
}

// ---------------------------------------------------------------------------
// Linux backend — AT-SPI over the session bus (atspi + zbus, pure Rust)
// ---------------------------------------------------------------------------

#[cfg(all(target_os = "linux", feature = "os"))]
pub(crate) mod linux_backend {
    use super::{OsError, OsNode, OsNodeStates};
    use atspi::proxy::accessible::AccessibleProxy;
    use atspi::proxy::proxy_ext::ProxyExt;
    use atspi::AccessibilityConnection;
    use zbus::names::BusName;

    /// Depth / size guards: a runaway provider must never wedge the action
    /// loop (native trees CAN be enormous — e.g. full desktops).
    const MAX_NODES: usize = 4_000;
    const MAX_DEPTH: usize = 40;

    pub(crate) struct LinuxConn {
        pub conn: AccessibilityConnection,
    }

    /// Public shim for the `From<zbus::Error>` impl (module-private fn).
    pub(crate) fn map_err_public(e: zbus::Error) -> OsError {
        map_err(e)
    }

    fn map_err(e: impl std::fmt::Display) -> OsError {
        // The common "no at-spi bus here" failures surface as zbus errors;
        // classify them honestly so the web can tell the user what to do.
        let s = e.to_string();
        if s.contains("InterfaceNotAvailable") {
            // The node simply does not implement the requested interface
            // (no Action, no EditableText…): an element-level refusal.
            OsError::Rejected(s)
        } else if s.contains("NameHasNoOwner")
            || s.contains("not provided")
            || s.contains("Access denied")
            || s.contains("No such file or directory")
            || s.contains("Could not connect")
            || s.contains("ServerAddressNotFound")
            || s.contains("relay")
        {
            OsError::BusUnavailable(s)
        } else {
            OsError::Atspi(s)
        }
    }

    pub(crate) async fn connect() -> Result<LinuxConn, OsError> {
        let conn = AccessibilityConnection::new()
            .await
            .map_err(map_err)?;
        Ok(LinuxConn { conn })
    }

    fn bus_name(dest: &str) -> Result<BusName<'static>, OsError> {
        BusName::try_from(dest.to_string())
            .map_err(|_| OsError::UnknownApp(dest.to_string()))
    }

    fn object_path(path: &str) -> Result<zbus::zvariant::ObjectPath<'static>, OsError> {
        zbus::zvariant::ObjectPath::try_from(path.to_string())
            .map_err(|e| OsError::Atspi(format!("bad object path {path:?}: {e}")))
    }

    async fn accessible_proxy(
        conn: &LinuxConn,
        dest: &str,
        path: &str,
    ) -> Result<AccessibleProxy<'static>, OsError> {
        AccessibleProxy::builder(&conn.conn.connection())
            .destination(bus_name(dest)?)?
            .path(object_path(path)?)?
            .build()
            .await
            .map_err(map_err)
    }

    /// The bus-wide root: the running apps' roots are the CHILDREN of the
    /// registry's accessible root — destination org.a11y.atspi.Registry,
    /// path /org/a11y/atspi/accessible/root. The destination must be set
    /// EXPLICITLY: the proxy defaults would use the interface name (nothing
    /// owns it → ServiceUnknown at the first call).
    async fn registry_root(conn: &LinuxConn) -> Result<AccessibleProxy<'static>, OsError> {
        AccessibleProxy::builder(&conn.conn.connection())
            .destination(bus_name("org.a11y.atspi.Registry")?)?
            .path(object_path("/org/a11y/atspi/accessible/root")?)?
            .build()
            .await
            .map_err(map_err)
    }

    /// Handles for one bus provider: (unique destination, friendly name,
    /// root node name). The friendly name comes from the Application
    /// interface (toolkit name); the root name is the app's own accessible
    /// name (e.g. "zenity") — often the most distinctive handle.
    async fn app_identity(
        conn: &LinuxConn,
        child: &atspi::object_ref::ObjectRefOwned,
    ) -> (String, String, String) {
        let unique = child
            .name()
            .map(|n| n.as_str().to_string())
            .unwrap_or_default();
        let path = child.path().to_string();
        let (friendly, root_name) = match accessible_proxy(conn, &unique, &path).await {
            Ok(node) => {
                let root_name = node.name().await.unwrap_or_default();
                let friendly = match node.proxies().await {
                    Ok(px) => match px.application().await {
                        Ok(ap) => ap.toolkit_name().await.unwrap_or_default(),
                        Err(_) => String::new(),
                    },
                    Err(_) => String::new(),
                };
                (friendly, root_name)
            }
            Err(_) => (String::new(), String::new()),
        };
        (unique, friendly, root_name)
    }

    /// All accessibility providers on the bus. Display name packs every
    /// handle: "friendly root — unique" (any token resolves downstream).
    pub(crate) async fn list_apps(conn: &LinuxConn) -> Result<Vec<String>, OsError> {
        let root = registry_root(conn).await?;
        let children = root.get_children().await.map_err(map_err)?;
        let mut out = Vec::with_capacity(children.len());
        for c in children {
            let (unique, friendly, root_name) = app_identity(conn, &c).await;
            let mut left = String::new();
            for handle in [friendly, root_name] {
                if !handle.is_empty() {
                    if !left.is_empty() {
                        left.push(' ');
                    }
                    left.push_str(&handle);
                }
            }
            if left.is_empty() {
                out.push(unique);
            } else {
                out.push(format!("{left} — {unique}"));
            }
        }
        Ok(out)
    }

    /// Resolve an app name: unique bus name, friendly/toolkit name, root
    /// node name, the exact display string from list_apps, or a substring
    /// of any of them (the model usually copies a distinctive fragment).
    pub(crate) async fn resolve_app(
        conn: &LinuxConn,
        app: &str,
    ) -> Result<(String, String), OsError> {
        let root = registry_root(conn).await?;
        let children = root.get_children().await.map_err(map_err)?;
        let needle = app.to_lowercase();
        for child in children {
            let (unique, friendly, root_name) = app_identity(conn, &child).await;
            let display = format!("{friendly} {root_name} — {unique}");
            let hit = unique.eq_ignore_ascii_case(app)
                || friendly.eq_ignore_ascii_case(app)
                || root_name.eq_ignore_ascii_case(app)
                || display.to_lowercase().contains(&needle);
            if hit && !unique.is_empty() {
                return Ok((unique, child.path().to_string()));
            }
        }
        Err(OsError::UnknownApp(app.to_string()))
    }

    /// Depth-first walk of one app's tree with an explicit stack (async
    /// recursion needs boxing; the stack makes the depth cap trivial).
    pub(crate) async fn walk_app(
        conn: &LinuxConn,
        app: &str,
    ) -> Result<(Option<String>, Vec<OsNode>), OsError> {
        let (dest, root_path) = resolve_app(conn, app).await?;
        let root = accessible_proxy(conn, &dest, &root_path).await?;
        let root_name = root.name().await.unwrap_or_default();

        let mut nodes = Vec::with_capacity(256);
        let mut stack: Vec<(String, usize)> = vec![(root_path, 0)];
        while let Some((path, depth)) = stack.pop() {
            if nodes.len() >= MAX_NODES || depth > MAX_DEPTH {
                continue;
            }
            let Ok(proxy) = accessible_proxy(conn, &dest, &path).await else {
                continue; // died mid-walk (window closed) — keep what we have
            };
            let role = proxy
                .get_role_name()
                .await
                .unwrap_or_else(|_| "unknown".to_string());
            let name = proxy.name().await.unwrap_or_default();
            let states = read_states(&proxy).await;
            let actions = read_actions(&proxy).await;
            nodes.push(OsNode {
                role,
                name,
                states,
                actions,
                identity: format!("{dest}|{path}"),
            });
            if let Ok(children) = proxy.get_children().await {
                // Reverse so the DFS stack pops children in document order.
                for child in children.into_iter().rev() {
                    stack.push((child.path().to_string(), depth + 1));
                }
            }
        }
        Ok((Some(root_name), nodes))
    }

    async fn read_states(proxy: &AccessibleProxy<'static>) -> OsNodeStates {
        let mut s = OsNodeStates::default();
        if let Ok(set) = proxy.get_state().await {
            // GTK3's atk-bridge asserts SENSITIVE (not ENABLED) for usable
            // widgets; a disabled one asserts neither. "Enabled" therefore
            // means: any of Enabled/Sensitive/Active asserted, or the node
            // asserts no state at all (optimistic default, never a wall of
            // phantom [disabled]).
            s.enabled = set.contains(atspi::State::Enabled)
                || set.contains(atspi::State::Sensitive)
                || set.contains(atspi::State::Active)
                || set.is_empty();
            s.focused = set.contains(atspi::State::Focused);
            s.checked = set.contains(atspi::State::Checked);
            s.editable = set.contains(atspi::State::Editable);
            if set.contains(atspi::State::Expandable) {
                s.expanded = Some(set.contains(atspi::State::Expanded));
            }
        }
        s
    }

    /// Machine-readable native action names, in provider order. Bounded:
    /// providers advertising absurd counts get truncated, never polled hot.
    /// Nodes without the Action interface render without an action list
    /// (the canonical conversion: node.proxies() → .action()).
    async fn read_actions(node: &AccessibleProxy<'static>) -> Vec<String> {
        let Ok(px) = node.proxies().await else {
            return Vec::new();
        };
        let Ok(ap) = px.action().await else {
            return Vec::new();
        };
        let n = ap.n_actions().await.unwrap_or(0).min(16);
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            match ap.get_name(i).await {
                Ok(k) => out.push(k),
                Err(_) => break,
            }
        }
        out
    }

    /// Execute a semantic action on an identity ("destination|/object/path").
    /// press → native default action (index 0, the AXPress equivalent);
    /// set_text → EditableText.SetTextContents (the AXSetValue equivalent);
    /// select → Selection.SelectChild (child index from `arg`).
    pub(crate) async fn do_action_by_identity(
        conn: &LinuxConn,
        identity: &str,
        action: &str,
        arg: Option<&str>,
    ) -> Result<String, OsError> {
        let Some((dest, path)) = identity.split_once('|') else {
            return Err(OsError::Atspi(format!(
                "malformed node identity {identity:?} (take a fresh snapshot)"
            )));
        };
        let node = accessible_proxy(conn, dest, path).await?;
        let px = node.proxies().await.map_err(map_err)?;
        match action {
            "press" => {
                let ap = px.action().await.map_err(map_err)?;
                let n = ap.n_actions().await.map_err(map_err)?;
                if n < 1 {
                    return Err(OsError::Rejected(
                        "element exposes no native actions".into(),
                    ));
                }
                let name = ap
                    .get_name(0)
                    .await
                    .unwrap_or_else(|_| "default".to_string());
                let ok = ap.do_action(0).await.map_err(map_err)?;
                if ok {
                    Ok(format!("pressed native action '{name}'"))
                } else {
                    Err(OsError::Rejected(format!(
                        "native action '{name}' returned false (disabled?)"
                    )))
                }
            }
            "set_text" => {
                let text = arg.ok_or_else(|| OsError::BadAction("set_text needs `text`".into()))?;
                let ep = px.editable_text().await.map_err(map_err)?;
                let ok = ep.set_text_contents(text).await.map_err(map_err)?;
                if ok {
                    Ok("text contents replaced".into())
                } else {
                    Err(OsError::Rejected(
                        "element did not accept SetTextContents (read-only?)".into(),
                    ))
                }
            }
            "select" => {
                let idx: i32 = arg
                    .and_then(|t| t.trim().parse().ok())
                    .ok_or_else(|| {
                        OsError::BadAction("select needs the child INDEX in `text`".into())
                    })?;
                let sp = px.selection().await.map_err(map_err)?;
                let ok = sp.select_child(idx).await.map_err(map_err)?;
                if ok {
                    Ok(format!("selected child {idx}"))
                } else {
                    Err(OsError::Rejected(format!(
                        "SelectChild({idx}) returned false (no such child or not selectable)"
                    )))
                }
            }
            other => Err(OsError::BadAction(other.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — platform-neutral pieces (renderer, refs, caps, contracts)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn node(role: &str, name: &str, actions: &[&str]) -> OsNode {
        OsNode {
            role: role.into(),
            name: name.into(),
            states: OsNodeStates::default(),
            actions: actions.iter().map(|s| s.to_string()).collect(),
            identity: format!("org.gnome.Files|/org/a11y/atspi/accessible/{name}"),
        }
    }

    #[test]
    fn renderer_produces_refs_states_and_actions() {
        let nodes = vec![
            node("frame", "Files", &[]),
            node("push button", "Save", &["press"]),
            {
                let mut n = node("check box", "Autosave", &["press"]);
                n.states.checked = true;
                n.states.enabled = false;
                n
            },
            {
                let mut n = node("entry", "Search", &["settext"]);
                n.states.editable = true;
                n
            },
        ];
        let (text, refs, count, truncated) = render_tree(&nodes, 50, None);
        assert!(!truncated);
        assert_eq!(count, 4);
        assert!(text.contains("[ref=2] push button \"Save\" (press)"));
        assert!(text.contains("[ref=3] check box \"Autosave\" [disabled] [checked] (press)"));
        assert!(text.contains("[ref=4] entry \"Search\" [editable] (settext)"));
        assert_eq!(refs.len(), 4);
        let rm = OsRefs::new(refs);
        assert_eq!(
            rm.get(2),
            Some("org.gnome.Files|/org/a11y/atspi/accessible/Save")
        );
        assert!(rm.get(99).is_none());
    }

    #[test]
    fn focus_and_limit_window_the_tree() {
        let nodes: Vec<OsNode> = (0..40)
            .map(|i| node("panel", &format!("section-{i}"), &[]))
            .collect();
        let (text, refs, total, truncated) = render_tree(&nodes, 10, Some("section-30"));
        assert_eq!(total, 40);
        assert!(truncated, "the cap cut the walk");
        assert!(text.contains("section-30"));
        assert!(text.contains("section-27"), "focus keeps preceding context");
        assert!(refs.len() <= 10);
        // No hit → whole tree from the top.
        let (text2, _, _, trunc2) = render_tree(&nodes, 10, Some("nope"));
        assert!(trunc2 && text2.contains("section-0"));
    }

    #[test]
    fn nameless_noise_is_skipped_but_named_anything_renders() {
        let nodes = vec![
            node("filler", "", &[]),
            node("whatever", "Has a name", &[]),
            node("list item", "", &[]), // role pattern → interesting
        ];
        let (text, refs, _, _) = render_tree(&nodes, 50, None);
        assert!(text.contains("Has a name"));
        assert!(text.contains("list item"));
        assert!(!text.contains("filler"));
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn cap_text_is_char_safe_and_honest() {
        assert_eq!(cap_text("short", 10), "short");
        let long = "áé👍".repeat(50);
        let capped = cap_text(&long, 10);
        assert!(capped.ends_with('…'));
        assert!(capped.chars().count() <= 11);
    }

    #[test]
    fn unknown_actions_are_rejected_by_contract() {
        assert!(OsError::BadAction("explode".into())
            .to_string()
            .contains("press | set_text | select"));
        assert!(OsError::UnknownRef(7).to_string().contains("desktop_os_snapshot"));
        assert!(OsError::UnknownApp("x".into())
            .to_string()
            .contains("desktop_os_apps"));
    }
}
