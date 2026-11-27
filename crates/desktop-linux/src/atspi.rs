use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use desktop_core::ResolvedRef;
use desktop_proto::{
    AppInfo, ElementAction, ErrorCode, ReadTextMode, ReadTextResult, Rect, RpcError, Snapshot,
    SnapshotParams, UiNode, WindowInfo, sanitize_text,
};
use uuid::Uuid;
use zbus::connection::Builder as ConnBuilder;
use zbus::{Connection, OwnedObjectPath, Proxy};

const DEFAULT_MAX_DEPTH: usize = 24;
const DEFAULT_MAX_NODES: usize = 250;
const WALK_BUDGET: Duration = Duration::from_millis(250);
const ATSPI_REGISTRY_DEST: &str = "org.a11y.atspi.Registry";
const ATSPI_ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";

/// Stored AT-SPI2 reference handle mapped to an `@eN` ID.
#[derive(Debug, Clone)]
pub struct AtSpiRefEntry {
    pub ref_id: String,
    pub bus_name: String,
    pub object_path: String,
    pub app: AppInfo,
    pub bounds: Option<Rect>,
    pub actions: Vec<ElementAction>,
    pub is_password: bool,
}

/// Thread-safe store mapping `@eN` element IDs to AT-SPI2 object addresses or X11 fallback refs.
#[derive(Default)]
pub struct LinuxRefStore {
    inner: Mutex<HashMap<String, AtSpiRefEntry>>,
}

impl LinuxRefStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replace(&self, entries: HashMap<String, AtSpiRefEntry>) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *map = entries;
    }

    pub fn get(&self, ref_id: &str) -> Result<AtSpiRefEntry, RpcError> {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.get(ref_id).cloned().ok_or_else(|| {
            RpcError::with_next(
                ErrorCode::StaleRef,
                format!("unknown or stale element ref {ref_id:?}"),
                "Take a fresh snapshot to obtain current @eN element refs.",
            )
        })
    }

    pub fn resolve(&self, ref_id: &str) -> Result<ResolvedRef, RpcError> {
        let entry = self.get(ref_id)?;
        Ok(ResolvedRef {
            ref_id: entry.ref_id,
            app: entry.app,
            bounds: entry.bounds,
            actions: entry.actions,
        })
    }
}

/// Run an async D-Bus future safely from synchronous `PlatformBackend` methods.
pub(crate) fn block_on_dbus<F, T>(fut: F) -> Result<T, RpcError>
where
    F: std::future::Future<Output = Result<T, RpcError>> + Send,
    T: Send,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(fut))
    } else {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("tokio runtime: {e}")))?;
        rt.block_on(fut)
    }
}

/// Probe whether the AT-SPI2 accessibility D-Bus is reachable.
#[must_use]
pub fn is_atspi_available() -> bool {
    block_on_dbus(async { connect_atspi_bus().await.map(|_| true) }).unwrap_or(false)
}

async fn connect_atspi_bus() -> Result<Connection, RpcError> {
    if let Ok(addr) = std::env::var("AT_SPI_BUS_ADDRESS")
        && !addr.trim().is_empty()
    {
        return ConnBuilder::address(addr.trim())
            .build()
            .await
            .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("AT-SPI connect: {e}")));
    }

    let session = Connection::session().await.map_err(|e| {
        RpcError::new(
            ErrorCode::BackendError,
            format!("session D-Bus unavailable: {e}"),
        )
    })?;
    let proxy = Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus")
        .await
        .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("org.a11y.Bus proxy: {e}")))?;
    let addr: String = proxy.call("GetAddress", &()).await.map_err(|e| {
        RpcError::new(
            ErrorCode::BackendError,
            format!("org.a11y.Bus.GetAddress failed: {e}"),
        )
    })?;
    if addr.trim().is_empty() {
        return Err(RpcError::new(
            ErrorCode::BackendError,
            "org.a11y.Bus returned empty accessibility bus address",
        ));
    }
    ConnBuilder::address(addr.trim())
        .build()
        .await
        .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("AT-SPI connect: {e}")))
}

/// Take an AT-SPI2 accessibility snapshot; returns `Err` if AT-SPI2 is unavailable so caller
/// can fall back cleanly to window-level X11 snapshot.
pub fn take_atspi_snapshot(
    store: &LinuxRefStore,
    params: &SnapshotParams,
    fallback_window: Option<&WindowInfo>,
    redact_passwords: bool,
) -> Result<Snapshot, RpcError> {
    let started = Instant::now();
    let max_depth = params.max_depth.unwrap_or(DEFAULT_MAX_DEPTH).min(48);
    let max_nodes = params.max_nodes.unwrap_or(DEFAULT_MAX_NODES).min(1000);
    let app_filter = params.app.clone();
    let root_ref = params.root_ref.clone();
    let existing_root = match root_ref.as_deref() {
        Some(r) => Some(store.get(r)?),
        None => None,
    };
    let win_hint = fallback_window.cloned();

    let (snapshot, entries) = block_on_dbus(async move {
        let conn = connect_atspi_bus().await?;

        let (start_bus, start_path, app_info, window_title, window_id) =
            if let Some(root_entry) = existing_root {
                (
                    root_entry.bus_name,
                    root_entry.object_path,
                    root_entry.app,
                    None,
                    win_hint.as_ref().map(|w| w.id),
                )
            } else {
                find_atspi_target(&conn, app_filter.as_deref(), win_hint.as_ref()).await?
            };

        let mut ctx = WalkCtx {
            conn: &conn,
            app: app_info.clone(),
            redact_passwords,
            max_depth,
            max_nodes,
            deadline: Instant::now() + WALK_BUDGET,
            next_id: 1,
            total_visited: 0,
            truncated: false,
            entries: HashMap::new(),
        };

        let root = walk_accessible(&mut ctx, &start_bus, &start_path, 0)
            .await
            .unwrap_or_else(|| UiNode {
                ref_id: "@e1".into(),
                role: "window".into(),
                subrole: None,
                title: window_title.clone(),
                label: None,
                value: None,
                bounds: win_hint.as_ref().map(|w| w.bounds),
                enabled: true,
                focused: win_hint.as_ref().is_some_and(|w| w.focused),
                selected: false,
                actions: Vec::new(),
                children: Vec::new(),
            });

        let emitted = ctx.entries.len().max(1);
        let snapshot = Snapshot {
            snapshot_id: format!("snap_{}", Uuid::new_v4().simple()),
            app: app_info,
            window_id,
            window_title,
            window_bounds: win_hint.as_ref().map(|w| w.bounds).or(root.bounds),
            truncated: ctx.truncated,
            total_nodes: ctx.total_visited.max(emitted),
            emitted_nodes: emitted,
            elapsed_ms: started.elapsed().as_millis() as u64,
            root,
        };
        Ok((snapshot, ctx.entries))
    })?;

    store.replace(entries);
    Ok(snapshot)
}

/// Build a clean fallback snapshot from X11 windows when AT-SPI2 is absent (e.g. minimal Xvfb).
pub fn fallback_window_snapshot(
    store: &LinuxRefStore,
    windows: &[WindowInfo],
    params: &SnapshotParams,
) -> Snapshot {
    let mut entries = HashMap::new();
    let chosen = params
        .window_id
        .and_then(|wid| windows.iter().find(|w| w.id == wid))
        .or_else(|| {
            params
                .app
                .as_deref()
                .and_then(|q| windows.iter().find(|w| w.app.matches_query(q)))
        })
        .or_else(|| windows.iter().find(|w| w.focused))
        .or_else(|| windows.first());

    let (app, window_id, window_title, window_bounds, root) = if let Some(w) = chosen {
        entries.insert(
            "@e1".to_string(),
            AtSpiRefEntry {
                ref_id: "@e1".into(),
                bus_name: String::new(),
                object_path: String::new(),
                app: w.app.clone(),
                bounds: Some(w.bounds),
                actions: vec![ElementAction::Press],
                is_password: false,
            },
        );
        let root = UiNode {
            ref_id: "@e1".into(),
            role: "window".into(),
            subrole: None,
            title: (!w.title.is_empty()).then(|| sanitize_text(&w.title, 120)),
            label: Some(sanitize_text(&w.app.name, 80)),
            value: None,
            bounds: Some(w.bounds),
            enabled: true,
            focused: w.focused,
            selected: false,
            actions: vec![ElementAction::Press],
            children: Vec::new(),
        };
        (
            w.app.clone(),
            Some(w.id),
            Some(w.title.clone()),
            Some(w.bounds),
            root,
        )
    } else {
        let app = AppInfo {
            name: "desktop".into(),
            bundle_id: None,
            pid: None,
            path: None,
            running: true,
        };
        entries.insert(
            "@e1".to_string(),
            AtSpiRefEntry {
                ref_id: "@e1".into(),
                bus_name: String::new(),
                object_path: String::new(),
                app: app.clone(),
                bounds: None,
                actions: Vec::new(),
                is_password: false,
            },
        );
        let root = UiNode {
            ref_id: "@e1".into(),
            role: "application".into(),
            subrole: None,
            title: Some("desktop (AT-SPI2 unavailable; use screenshot + coordinates)".into()),
            label: None,
            value: None,
            bounds: None,
            enabled: true,
            focused: false,
            selected: false,
            actions: Vec::new(),
            children: Vec::new(),
        };
        (app, None, None, None, root)
    };

    store.replace(entries);
    Snapshot {
        snapshot_id: format!("snap_{}", Uuid::new_v4().simple()),
        app,
        window_id,
        window_title,
        window_bounds,
        truncated: false,
        total_nodes: 1,
        emitted_nodes: 1,
        elapsed_ms: 0,
        root,
    }
}

/// Perform semantic background click via AT-SPI2 `Action.DoAction(0)`.
pub fn do_action(entry: &AtSpiRefEntry) -> Result<bool, RpcError> {
    if entry.bus_name.is_empty() || entry.object_path.is_empty() {
        return Ok(false);
    }
    let bus = entry.bus_name.clone();
    let path = entry.object_path.clone();
    block_on_dbus(async move {
        let conn = connect_atspi_bus().await?;
        let proxy = Proxy::new(&conn, bus.as_str(), path.as_str(), "org.a11y.atspi.Action")
            .await
            .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("Action proxy: {e}")))?;
        let ok: bool = proxy
            .call("DoAction", &(0i32,))
            .await
            .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("DoAction failed: {e}")))?;
        Ok(ok)
    })
}

/// Perform zero-focus background text entry via AT-SPI2 `EditableText.SetTextContents`.
pub fn set_editable_text(entry: &AtSpiRefEntry, text: &str) -> Result<bool, RpcError> {
    if entry.bus_name.is_empty() || entry.object_path.is_empty() {
        return Ok(false);
    }
    let bus = entry.bus_name.clone();
    let path = entry.object_path.clone();
    let text_owned = text.to_string();
    block_on_dbus(async move {
        let conn = connect_atspi_bus().await?;
        let proxy = Proxy::new(
            &conn,
            bus.as_str(),
            path.as_str(),
            "org.a11y.atspi.EditableText",
        )
        .await
        .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("EditableText proxy: {e}")))?;
        let ok: bool = proxy
            .call("SetTextContents", &(text_owned,))
            .await
            .map_err(|e| {
                RpcError::new(
                    ErrorCode::BackendError,
                    format!("SetTextContents failed: {e}"),
                )
            })?;
        Ok(ok)
    })
}

/// Read text from an AT-SPI2 element (`Text.GetText` or `Text.GetSelection`).
pub fn read_atspi_text(
    entry: &AtSpiRefEntry,
    mode: ReadTextMode,
    redact_passwords: bool,
) -> Result<ReadTextResult, RpcError> {
    if entry.is_password && redact_passwords {
        return Ok(ReadTextResult {
            text: "<redacted>".into(),
            source: "atspi:redacted".into(),
        });
    }
    if entry.bus_name.is_empty() || entry.object_path.is_empty() {
        return Err(RpcError::with_next(
            ErrorCode::BackendError,
            "element has no live AT-SPI2 handle",
            "Use mode=\"clipboard\" or ensure at-spi2-core is running.",
        ));
    }
    let bus = entry.bus_name.clone();
    let path = entry.object_path.clone();
    block_on_dbus(async move {
        let conn = connect_atspi_bus().await?;
        let proxy = Proxy::new(&conn, bus.as_str(), path.as_str(), "org.a11y.atspi.Text")
            .await
            .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("Text proxy: {e}")))?;
        let (raw, source): (String, &str) = match mode {
            ReadTextMode::Selection => {
                let (start, end): (i32, i32) =
                    proxy.call("GetSelection", &(0i32,)).await.unwrap_or((0, 0));
                let s = if end > start {
                    proxy
                        .call("GetText", &(start, end))
                        .await
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                (s, "atspi:selection")
            }
            _ => (
                proxy
                    .call("GetText", &(0i32, -1i32))
                    .await
                    .unwrap_or_default(),
                "atspi:text",
            ),
        };
        Ok(ReadTextResult {
            text: sanitize_text(&raw, 16_384),
            source: source.into(),
        })
    })
}

/// Invoke an application menu item via AT-SPI2 by path (e.g. `["File", "New"]`).
pub fn invoke_menu(app: &str, path: &[String]) -> Result<String, RpcError> {
    if path.is_empty() {
        return Err(RpcError::new(
            ErrorCode::InvalidInput,
            "menu path cannot be empty",
        ));
    }
    let app_owned = app.to_string();
    let path_owned = path.to_vec();
    block_on_dbus(async move {
        let conn = connect_atspi_bus().await?;
        let (bus, root_path, _, _, _) = find_atspi_target(&conn, Some(&app_owned), None).await?;

        let mut current = (bus, root_path);
        for segment in &path_owned {
            let needle = segment.trim().to_ascii_lowercase();
            let Some(found) = find_menu_child(&conn, &current.0, &current.1, &needle, 4).await
            else {
                return Err(RpcError::with_next(
                    ErrorCode::NotFound,
                    format!("menu item {segment:?} not found in {app_owned:?}"),
                    "Verify the menu item names with a snapshot of the application.",
                ));
            };
            current = found;
        }
        let proxy = Proxy::new(
            &conn,
            current.0.as_str(),
            current.1.as_str(),
            "org.a11y.atspi.Action",
        )
        .await
        .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("Action proxy: {e}")))?;
        let _: bool = proxy.call("DoAction", &(0i32,)).await.map_err(|e| {
            RpcError::new(
                ErrorCode::BackendError,
                format!("menu DoAction failed: {e}"),
            )
        })?;
        Ok(format!(
            "invoked AT-SPI menu {} > {}",
            app_owned,
            path_owned.join(" > ")
        ))
    })
}

async fn find_menu_child(
    conn: &Connection,
    bus: &str,
    path: &str,
    needle: &str,
    depth: usize,
) -> Option<(String, String)> {
    if depth == 0 {
        return None;
    }
    let children = get_children(conn, bus, path).await;
    for (cbus, cpath) in children {
        let name = get_string_prop(conn, &cbus, &cpath, "Name")
            .await
            .unwrap_or_default();
        if !name.is_empty() && name.trim().to_ascii_lowercase() == needle {
            return Some((cbus, cpath));
        }
        if let Some(found) = Box::pin(find_menu_child(conn, &cbus, &cpath, needle, depth - 1)).await
        {
            return Some(found);
        }
    }
    None
}

async fn find_atspi_target(
    conn: &Connection,
    app_filter: Option<&str>,
    win_hint: Option<&WindowInfo>,
) -> Result<(String, String, AppInfo, Option<String>, Option<u32>), RpcError> {
    let apps = get_children(conn, ATSPI_REGISTRY_DEST, ATSPI_ROOT_PATH).await;
    if apps.is_empty() {
        return Err(RpcError::new(
            ErrorCode::NotFound,
            "AT-SPI2 registry reports no accessible applications",
        ));
    }

    let filter_lc = app_filter
        .map(|s| s.trim().to_ascii_lowercase())
        .or_else(|| win_hint.map(|w| w.app.name.to_ascii_lowercase()));

    let mut chosen: Option<(String, String, String)> = None;
    for (bus, path) in &apps {
        let name = get_string_prop(conn, bus, path, "Name")
            .await
            .unwrap_or_default();
        if let Some(f) = &filter_lc {
            if name.to_ascii_lowercase().contains(f) {
                chosen = Some((bus.clone(), path.clone(), name));
                break;
            }
        } else if !name.is_empty() {
            chosen = Some((bus.clone(), path.clone(), name));
            break;
        }
    }

    let (app_bus, app_path, app_name) = chosen.ok_or_else(|| {
        RpcError::new(ErrorCode::NotFound, "no matching AT-SPI2 application found")
    })?;

    let app_info = win_hint.map(|w| w.app.clone()).unwrap_or(AppInfo {
        name: app_name,
        bundle_id: None,
        pid: None,
        path: None,
        running: true,
    });

    // Prefer first frame/window child of the application so the tree starts at the window.
    let windows = get_children(conn, &app_bus, &app_path).await;
    if let Some((wbus, wpath)) = windows.into_iter().next() {
        let wtitle = get_string_prop(conn, &wbus, &wpath, "Name").await;
        return Ok((wbus, wpath, app_info, wtitle, win_hint.map(|w| w.id)));
    }

    Ok((app_bus, app_path, app_info, None, win_hint.map(|w| w.id)))
}

struct WalkCtx<'a> {
    conn: &'a Connection,
    app: AppInfo,
    redact_passwords: bool,
    max_depth: usize,
    max_nodes: usize,
    deadline: Instant,
    next_id: usize,
    total_visited: usize,
    truncated: bool,
    entries: HashMap<String, AtSpiRefEntry>,
}

async fn walk_accessible(
    ctx: &mut WalkCtx<'_>,
    bus: &str,
    path: &str,
    depth: usize,
) -> Option<UiNode> {
    if Instant::now() >= ctx.deadline || ctx.entries.len() >= ctx.max_nodes {
        ctx.truncated = true;
        return None;
    }
    ctx.total_visited += 1;

    let raw_role = get_role_name(ctx.conn, bus, path).await.unwrap_or_default();
    let (role, is_password) = normalize_atspi_role(&raw_role);
    let name = get_string_prop(ctx.conn, bus, path, "Name")
        .await
        .map(|s| sanitize_text(&s, 120))
        .filter(|s| !s.is_empty());
    let desc = get_string_prop(ctx.conn, bus, path, "Description")
        .await
        .map(|s| sanitize_text(&s, 120))
        .filter(|s| !s.is_empty());

    let interfaces = get_interfaces(ctx.conn, bus, path).await;
    let has_action = interfaces.iter().any(|i| i == "org.a11y.atspi.Action");
    let has_editable = interfaces
        .iter()
        .any(|i| i == "org.a11y.atspi.EditableText");
    let has_text = interfaces.iter().any(|i| i == "org.a11y.atspi.Text");
    let has_component = interfaces.iter().any(|i| i == "org.a11y.atspi.Component");

    let mut actions = Vec::new();
    if has_action {
        actions.push(ElementAction::Press);
    }
    if has_editable {
        actions.push(ElementAction::SetValue);
    }

    let value = if is_password && ctx.redact_passwords {
        Some("<redacted>".to_string())
    } else if has_text && matches!(role.as_str(), "textfield" | "textarea" | "textfield:secure") {
        get_text_preview(ctx.conn, bus, path)
            .await
            .map(|s| sanitize_text(&s, 120))
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    let bounds = if has_component {
        get_extents(ctx.conn, bus, path).await
    } else {
        None
    };

    let is_anonymous_container = depth > 0
        && matches!(role.as_str(), "group" | "generic" | "panel" | "filler")
        && name.is_none()
        && desc.is_none()
        && value.is_none()
        && !has_editable;

    let mut ref_id = None;
    if !is_anonymous_container {
        let id = format!("@e{}", ctx.next_id);
        ctx.next_id += 1;
        ctx.entries.insert(
            id.clone(),
            AtSpiRefEntry {
                ref_id: id.clone(),
                bus_name: bus.to_string(),
                object_path: path.to_string(),
                app: ctx.app.clone(),
                bounds,
                actions: actions.clone(),
                is_password,
            },
        );
        ref_id = Some(id);
    }

    let mut children = Vec::new();
    if depth < ctx.max_depth {
        for (cbus, cpath) in get_children(ctx.conn, bus, path).await {
            if Instant::now() >= ctx.deadline || ctx.entries.len() >= ctx.max_nodes {
                ctx.truncated = true;
                break;
            }
            if let Some(child_node) = Box::pin(walk_accessible(ctx, &cbus, &cpath, depth + 1)).await
            {
                if child_node.ref_id.is_empty() {
                    children.extend(child_node.children);
                } else {
                    children.push(child_node);
                }
            }
        }
    }

    if is_anonymous_container {
        if children.is_empty() {
            return None;
        }
        return Some(UiNode {
            ref_id: String::new(),
            role,
            subrole: None,
            title: None,
            label: None,
            value: None,
            bounds: None,
            enabled: true,
            focused: false,
            selected: false,
            actions: Vec::new(),
            children,
        });
    }

    Some(UiNode {
        ref_id: ref_id.unwrap_or_default(),
        role,
        subrole: None,
        title: name,
        label: desc,
        value,
        bounds,
        enabled: true,
        focused: false,
        selected: false,
        actions,
        children,
    })
}

async fn get_children(conn: &Connection, bus: &str, path: &str) -> Vec<(String, String)> {
    let Ok(proxy) = Proxy::new(conn, bus, path, "org.a11y.atspi.Accessible").await else {
        return Vec::new();
    };
    let count: i32 = proxy.get_property("ChildCount").await.unwrap_or(0);
    let capped = count.clamp(0, 64);
    let mut out = Vec::with_capacity(capped as usize);
    for idx in 0..capped {
        if let Ok((cbus, cpath)) = proxy
            .call::<_, _, (String, OwnedObjectPath)>("GetChildAtIndex", &(idx,))
            .await
            && !cbus.is_empty()
            && cpath.as_str() != "/org/a11y/atspi/null"
        {
            out.push((cbus, cpath.to_string()));
        }
    }
    out
}

async fn get_string_prop(conn: &Connection, bus: &str, path: &str, prop: &str) -> Option<String> {
    let proxy = Proxy::new(conn, bus, path, "org.a11y.atspi.Accessible")
        .await
        .ok()?;
    proxy.get_property::<String>(prop).await.ok()
}

async fn get_role_name(conn: &Connection, bus: &str, path: &str) -> Option<String> {
    let proxy = Proxy::new(conn, bus, path, "org.a11y.atspi.Accessible")
        .await
        .ok()?;
    proxy.call::<_, _, String>("GetRoleName", &()).await.ok()
}

async fn get_interfaces(conn: &Connection, bus: &str, path: &str) -> Vec<String> {
    let Ok(proxy) = Proxy::new(conn, bus, path, "org.a11y.atspi.Accessible").await else {
        return Vec::new();
    };
    proxy
        .call::<_, _, Vec<String>>("GetInterfaces", &())
        .await
        .unwrap_or_default()
}

async fn get_extents(conn: &Connection, bus: &str, path: &str) -> Option<Rect> {
    let proxy = Proxy::new(conn, bus, path, "org.a11y.atspi.Component")
        .await
        .ok()?;
    // coord_type 0 = ATSPI_COORD_TYPE_SCREEN
    let (x, y, w, h): (i32, i32, i32, i32) = proxy.call("GetExtents", &(0u32,)).await.ok()?;
    if w <= 0 || h <= 0 {
        return None;
    }
    Some(Rect {
        x: f64::from(x),
        y: f64::from(y),
        width: f64::from(w),
        height: f64::from(h),
    })
}

async fn get_text_preview(conn: &Connection, bus: &str, path: &str) -> Option<String> {
    let proxy = Proxy::new(conn, bus, path, "org.a11y.atspi.Text")
        .await
        .ok()?;
    proxy
        .call::<_, _, String>("GetText", &(0i32, 256i32))
        .await
        .ok()
}

/// Normalize AT-SPI2 role names to match `desktop-macos` canonical role names.
#[must_use]
pub fn normalize_atspi_role(raw: &str) -> (String, bool) {
    let lower = raw.trim().to_ascii_lowercase();
    match lower.as_str() {
        "push button" | "push-button" | "toggle button" | "toggle-button" => {
            ("button".into(), false)
        }
        "password text" | "password-text" => ("textfield:secure".into(), true),
        "text" | "entry" | "spin button" | "spin-button" => ("textfield".into(), false),
        "document web" | "document-web" | "document frame" => ("textarea".into(), false),
        "check box" | "check-box" | "check menu item" => ("checkbox".into(), false),
        "radio button" | "radio-button" => ("radio".into(), false),
        "page tab" | "page-tab" => ("tab".into(), false),
        "link" => ("link".into(), false),
        "menu item" | "menu-item" | "menu" => ("menuitem".into(), false),
        "combo box" | "combo-box" => ("popup".into(), false),
        "label" | "static" | "heading" => ("text".into(), false),
        "frame" | "dialog" | "alert" | "window" => ("window".into(), false),
        "application" => ("application".into(), false),
        "panel" | "filler" | "grouping" | "section" | "scroll pane" | "viewport" => {
            ("group".into(), false)
        }
        "" => ("generic".into(), false),
        other => (other.replace(' ', "_"), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_atspi_roles_and_marks_passwords() {
        assert_eq!(
            normalize_atspi_role("push button"),
            ("button".into(), false)
        );
        assert_eq!(
            normalize_atspi_role("password text"),
            ("textfield:secure".into(), true)
        );
        assert_eq!(normalize_atspi_role("entry"), ("textfield".into(), false));
        assert_eq!(normalize_atspi_role("page tab"), ("tab".into(), false));
        assert_eq!(normalize_atspi_role("filler"), ("group".into(), false));
    }

    #[test]
    fn fallback_window_snapshot_populates_ref_store() {
        let store = LinuxRefStore::new();
        let win = WindowInfo {
            id: 42,
            app: AppInfo {
                name: "xterm".into(),
                bundle_id: Some("xterm".into()),
                pid: Some(1234),
                path: None,
                running: true,
            },
            title: "Terminal".into(),
            bounds: Rect {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
            },
            layer: 0,
            alpha: 1.0,
            display_index: Some(0),
            focused: true,
            minimized: false,
            on_screen: true,
        };
        let snap = fallback_window_snapshot(&store, &[win], &SnapshotParams::default());
        assert_eq!(snap.root.ref_id, "@e1");
        assert_eq!(snap.window_id, Some(42));
        let resolved = store.resolve("@e1").unwrap();
        assert_eq!(resolved.app.name, "xterm");
    }
}
