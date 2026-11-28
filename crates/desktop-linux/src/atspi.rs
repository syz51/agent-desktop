use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use desktop_core::ResolvedRef;
use desktop_core::tree::{
    AxNodeData, DEFAULT_PRIMARY_FANOUT, RecordOutcome, SnapshotWalkBuilder, WalkBudget,
    is_decorative_container,
};
use desktop_proto::{
    AppInfo, AppMatchRank, ElementAction, ErrorCode, ReadTextMode, ReadTextResult, Rect, RpcError,
    Snapshot, SnapshotParams, UiNode, WindowInfo, best_matching_window, sanitize_text,
};
use futures_util::future::join_all;
use uuid::Uuid;
use zbus::connection::Builder as ConnBuilder;
use zbus::proxy::{Builder as ProxyBuilder, CacheProperties};
use zbus::{Connection, OwnedObjectPath, Proxy};

const DEFAULT_MAX_DEPTH: usize = 24;
const DEFAULT_MAX_NODES: usize = 250;
const WALK_BUDGET: Duration = Duration::from_millis(250);
const ATSPI_REGISTRY_DEST: &str = "org.a11y.atspi.Registry";
const ATSPI_ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";

/// Cached AT-SPI2 bus address resolved from `org.a11y.Bus.GetAddress` so hot calls avoid a
/// redundant session D-Bus connection + GetAddress round-trip on every snapshot/action.
static CACHED_ATSPI_ADDR: Mutex<Option<String>> = Mutex::new(None);

/// Outcome of an AT-SPI2 snapshot attempt so callers only fall back when the bus is unavailable.
pub enum AtSpiSnapshotResult {
    Snapshot(Box<Snapshot>),
    /// AT-SPI2 bus is not running or has no registered accessible applications.
    Unavailable,
}

/// Stored AT-SPI2 reference handle mapped to an `@eN` ID.
#[derive(Debug, Clone)]
pub struct AtSpiRefEntry {
    pub ref_id: String,
    pub bus_name: String,
    pub object_path: String,
    pub app: AppInfo,
    pub window_id: Option<u32>,
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
            window_id: entry.window_id,
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

/// Build a non-caching D-Bus proxy (`CacheProperties::No`) to avoid `AddMatch`/`GetAll` overhead.
pub(crate) async fn uncached_proxy<'a>(
    conn: &'a Connection,
    dest: &'a str,
    path: &'a str,
    iface: &'a str,
) -> Result<Proxy<'a>, RpcError> {
    ProxyBuilder::new(conn)
        .destination(dest)
        .path(path)
        .interface(iface)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("D-Bus proxy build: {e}")))
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

    let cached = CACHED_ATSPI_ADDR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(addr) = cached {
        if let Ok(conn) = ConnBuilder::address(addr.as_str()).build().await {
            return Ok(conn);
        }
        // Clear stale address if the accessibility bus restarted and re-resolve once below.
        *CACHED_ATSPI_ADDR.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    let session = Connection::session().await.map_err(|e| {
        RpcError::new(
            ErrorCode::BackendError,
            format!("session D-Bus unavailable: {e}"),
        )
    })?;
    let proxy = uncached_proxy(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus").await?;
    let addr: String = proxy.call("GetAddress", &()).await.map_err(|e| {
        RpcError::new(
            ErrorCode::BackendError,
            format!("org.a11y.Bus.GetAddress failed: {e}"),
        )
    })?;
    let trimmed = addr.trim();
    if trimmed.is_empty() {
        return Err(RpcError::new(
            ErrorCode::BackendError,
            "org.a11y.Bus returned empty accessibility bus address",
        ));
    }
    let conn = ConnBuilder::address(trimmed)
        .build()
        .await
        .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("AT-SPI connect: {e}")))?;
    *CACHED_ATSPI_ADDR.lock().unwrap_or_else(|e| e.into_inner()) = Some(trimmed.to_string());
    Ok(conn)
}

/// Take an AT-SPI2 accessibility snapshot.
///
/// Propagates caller errors (`StaleRef`, explicit app `NotFound`) rather than clobbering `store`;
/// returns `Ok(AtSpiSnapshotResult::Unavailable)` only when the AT-SPI2 bus itself is unavailable.
pub fn take_atspi_snapshot(
    store: &LinuxRefStore,
    params: &SnapshotParams,
    fallback_window: Option<&WindowInfo>,
    redact_passwords: bool,
) -> Result<AtSpiSnapshotResult, RpcError> {
    let started = Instant::now();
    let max_depth = params.max_depth.unwrap_or(DEFAULT_MAX_DEPTH).min(48);
    let max_nodes = params.max_nodes.unwrap_or(DEFAULT_MAX_NODES).min(1000);
    let app_filter = params.app.clone();
    // Validate `root_ref` up-front so expired refs always return `StaleRef`.
    let existing_root = match params.root_ref.as_deref() {
        Some(r) => Some(store.get(r)?),
        None => None,
    };
    let win_hint = fallback_window.cloned();

    let result = block_on_dbus(async move {
        let Ok(conn) = connect_atspi_bus().await else {
            return Ok(None);
        };

        let (start_bus, start_path, app_info, window_title, window_id) =
            if let Some(root_entry) = existing_root {
                if root_entry.bus_name.is_empty() || root_entry.object_path.is_empty() {
                    return Ok(None);
                }
                (
                    root_entry.bus_name,
                    root_entry.object_path,
                    root_entry.app,
                    None,
                    win_hint.as_ref().map(|w| w.id).or(root_entry.window_id),
                )
            } else {
                match find_atspi_target(&conn, app_filter.as_deref(), win_hint.as_ref()).await? {
                    Some(target) => target,
                    None => return Ok(None),
                }
            };

        let fallback_root = UiNode {
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
        };

        struct LazyChildrenTail {
            bus: String,
            path: String,
            parent_idx: usize,
            child_depth: usize,
            next_idx: usize,
            total: usize,
        }

        let deadline = Instant::now() + WALK_BUDGET;
        let mut builder = SnapshotWalkBuilder::new(
            (start_bus, start_path),
            WalkBudget::new(max_depth, max_nodes),
        );
        // Defer D-Bus `GetChildAtIndex` calls past `DEFAULT_PRIMARY_FANOUT` until shallower panes
        // finish so wide lists do not burn the 250 ms budget fetching tail items up front.
        let mut lazy_tails: VecDeque<LazyChildrenTail> = VecDeque::new();

        loop {
            let pending = match builder.next_pending() {
                Some(p) => p,
                None => {
                    if builder.is_budget_saturated() {
                        if !lazy_tails.is_empty() {
                            builder.mark_truncated();
                        }
                        break;
                    }
                    if Instant::now() >= deadline {
                        if !lazy_tails.is_empty() {
                            builder.mark_truncated();
                        }
                        break;
                    }
                    let Some(mut tail) = lazy_tails.pop_front() else {
                        break;
                    };
                    if let Ok(p) =
                        uncached_proxy(&conn, &tail.bus, &tail.path, "org.a11y.atspi.Accessible")
                            .await
                    {
                        let batch_end = (tail.next_idx + DEFAULT_PRIMARY_FANOUT).min(tail.total);
                        let batch = get_children_range(&p, tail.next_idx, batch_end).await;
                        builder.enqueue_children_from(
                            tail.parent_idx,
                            tail.child_depth,
                            tail.next_idx,
                            batch,
                        );
                        tail.next_idx = batch_end;
                        if tail.next_idx < tail.total {
                            lazy_tails.push_back(tail);
                        }
                    }
                    continue;
                }
            };
            if Instant::now() >= deadline {
                builder.mark_truncated();
                break;
            }
            let ((bus, path), meta) = pending.into_parts();
            let depth = meta.depth();

            // Single non-caching Accessible proxy reused for role, name, description, interfaces, and children.
            let Ok(acc_proxy) =
                uncached_proxy(&conn, &bus, &path, "org.a11y.atspi.Accessible").await
            else {
                continue;
            };

            // Fetch all 4 independent node properties concurrently in a single D-Bus RTT.
            let (raw_role_res, name_res, desc_res, ifaces_res) = tokio::join!(
                acc_proxy.call::<_, _, String>("GetRoleName", &()),
                acc_proxy.get_property::<String>("Name"),
                acc_proxy.get_property::<String>("Description"),
                acc_proxy.call::<_, _, Vec<String>>("GetInterfaces", &()),
            );
            let raw_role = raw_role_res.unwrap_or_default();
            let (role, is_password) = normalize_atspi_role(&raw_role);
            let name = name_res
                .ok()
                .map(|s| sanitize_text(&s, 120))
                .filter(|s| !s.is_empty());
            let desc = desc_res
                .ok()
                .map(|s| sanitize_text(&s, 120))
                .filter(|s| !s.is_empty());

            let interfaces = ifaces_res.unwrap_or_default();
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

            let want_text_preview = !(is_password && redact_passwords)
                && has_text
                && matches!(role.as_str(), "textfield" | "textarea" | "textfield:secure");
            let likely_decorative = !want_text_preview
                && is_decorative_container(
                    depth,
                    &role,
                    name.as_deref(),
                    desc.as_deref(),
                    None,
                    false,
                    false,
                    &actions,
                );
            let want_extents = !likely_decorative && has_component;

            // Fetch text preview and component geometry concurrently when both are needed.
            let (raw_value, bounds) = tokio::join!(
                async {
                    if is_password && redact_passwords {
                        Some("<redacted>".to_string())
                    } else if want_text_preview {
                        get_text_preview(&conn, &bus, &path)
                            .await
                            .map(|s| sanitize_text(&s, 120))
                            .filter(|s| !s.is_empty())
                    } else {
                        None
                    }
                },
                async {
                    if want_extents {
                        get_extents(&conn, &bus, &path).await
                    } else {
                        None
                    }
                },
            );
            let value = raw_value;
            let decorative = is_decorative_container(
                depth,
                &role,
                name.as_deref(),
                desc.as_deref(),
                value.as_deref(),
                false,
                false,
                &actions,
            );
            let bounds = if decorative { None } else { bounds };

            let entry = (!decorative).then(|| AtSpiRefEntry {
                ref_id: String::new(),
                bus_name: bus.clone(),
                object_path: path.clone(),
                app: app_info.clone(),
                window_id,
                bounds,
                actions: actions.clone(),
                is_password,
            });

            let data = AxNodeData {
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
            };

            match builder.record_node(meta, data, entry) {
                RecordOutcome::Expand(node_idx) => {
                    let raw_count: i32 = acc_proxy.get_property("ChildCount").await.unwrap_or(0);
                    let total = raw_count.clamp(0, 64) as usize;
                    if raw_count > 64 {
                        builder.mark_truncated();
                    }
                    let first_end = total.min(DEFAULT_PRIMARY_FANOUT);
                    let first_batch = get_children_range(&acc_proxy, 0, first_end).await;
                    builder.enqueue_children_from(node_idx, depth + 1, 0, first_batch);
                    if first_end < total {
                        lazy_tails.push_back(LazyChildrenTail {
                            bus,
                            path,
                            parent_idx: node_idx,
                            child_depth: depth + 1,
                            next_idx: first_end,
                            total,
                        });
                    }
                }
                RecordOutcome::CheckChildrenForTruncation => {
                    let count: i32 = acc_proxy.get_property("ChildCount").await.unwrap_or(0);
                    if count > 0 {
                        builder.mark_truncated();
                    }
                }
                RecordOutcome::Done => {}
            }
        }

        let mut output = builder.finish(fallback_root);
        for (ref_id, entry) in &mut output.refs {
            entry.ref_id.clone_from(ref_id);
        }

        let snapshot = Snapshot {
            snapshot_id: format!("snap_{}", Uuid::new_v4().simple()),
            app: app_info,
            window_id,
            window_title,
            window_bounds: win_hint.as_ref().map(|w| w.bounds).or(output.root.bounds),
            truncated: output.truncated,
            total_nodes: output.total_visited,
            emitted_nodes: output.emitted_nodes,
            elapsed_ms: started.elapsed().as_millis() as u64,
            root: output.root,
        };
        Ok(Some((snapshot, output.refs)))
    })?;

    match result {
        Some((snapshot, entries)) => {
            store.replace(entries);
            Ok(AtSpiSnapshotResult::Snapshot(Box::new(snapshot)))
        }
        None => Ok(AtSpiSnapshotResult::Unavailable),
    }
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
                .and_then(|q| best_matching_window(windows, q))
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
                window_id: Some(w.id),
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
                window_id: None,
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
        let proxy =
            uncached_proxy(&conn, bus.as_str(), path.as_str(), "org.a11y.atspi.Action").await?;
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
        let proxy = uncached_proxy(
            &conn,
            bus.as_str(),
            path.as_str(),
            "org.a11y.atspi.EditableText",
        )
        .await?;
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
        let proxy =
            uncached_proxy(&conn, bus.as_str(), path.as_str(), "org.a11y.atspi.Text").await?;
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
            text: raw,
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
        let Some((bus, root_path, _, _, _)) =
            find_atspi_target(&conn, Some(&app_owned), None).await?
        else {
            return Err(RpcError::with_next(
                ErrorCode::NotFound,
                format!("no accessible application found for {app_owned:?}"),
                "Ensure at-spi2-core is running and the target app is open.",
            ));
        };

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
        let proxy = uncached_proxy(
            &conn,
            current.0.as_str(),
            current.1.as_str(),
            "org.a11y.atspi.Action",
        )
        .await?;
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
    let Ok(proxy) = uncached_proxy(conn, bus, path, "org.a11y.atspi.Accessible").await else {
        return None;
    };
    let children = get_children_via(&proxy).await;
    for (cbus, cpath) in children {
        let Ok(cproxy) = uncached_proxy(conn, &cbus, &cpath, "org.a11y.atspi.Accessible").await
        else {
            continue;
        };
        let name: String = cproxy.get_property("Name").await.unwrap_or_default();
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
) -> Result<Option<(String, String, AppInfo, Option<String>, Option<u32>)>, RpcError> {
    let Ok(root_proxy) = uncached_proxy(
        conn,
        ATSPI_REGISTRY_DEST,
        ATSPI_ROOT_PATH,
        "org.a11y.atspi.Accessible",
    )
    .await
    else {
        return Ok(None);
    };
    let apps = get_children_via(&root_proxy).await;
    if apps.is_empty() {
        return Ok(None);
    }

    if let Some(explicit) = app_filter
        && explicit.trim().is_empty()
    {
        return Err(RpcError::with_next(
            ErrorCode::NotFound,
            format!("no accessible AT-SPI2 application matching {explicit:?}"),
            "Open the application or call list_windows to inspect available apps.",
        ));
    }

    // Fetch all registered AT-SPI2 app names concurrently with a tight per-app timeout so a
    // single hung or suspended desktop app cannot stall discovery for zbus's 25 s default timeout.
    const APP_NAME_TIMEOUT: Duration = Duration::from_millis(150);
    let named_apps: Vec<Option<(String, String, String)>> =
        join_all(apps.iter().map(|(bus, path)| async move {
            tokio::time::timeout(APP_NAME_TIMEOUT, async {
                let p = uncached_proxy(conn, bus, path, "org.a11y.atspi.Accessible")
                    .await
                    .ok()?;
                let name: String = p.get_property("Name").await.unwrap_or_default();
                Some((bus.clone(), path.clone(), name))
            })
            .await
            .ok()
            .flatten()
        }))
        .await;

    let mut chosen: Option<(AppMatchRank, String, String, String)> = None;
    for (bus, path, name) in named_apps.into_iter().flatten() {
        if let Some(explicit) = app_filter {
            let probe = AppInfo {
                name: name.clone(),
                bundle_id: None,
                pid: None,
                path: None,
                running: true,
            };
            let rank = probe.match_rank(explicit).or_else(|| {
                win_hint
                    .filter(|w| w.app.matches_query(explicit))
                    .map(|w| w.app.name.as_str())
                    .and_then(|hint_name| probe.match_rank(hint_name))
            });
            if let Some(rank) = rank {
                let better = chosen
                    .as_ref()
                    .is_none_or(|(best_rank, _, _, _)| rank > *best_rank);
                if better {
                    chosen = Some((rank, bus, path, name));
                    if rank == AppMatchRank::Exact {
                        break;
                    }
                }
            }
        } else if let Some(hint) = win_hint {
            let probe = AppInfo {
                name: name.clone(),
                bundle_id: None,
                pid: None,
                path: None,
                running: true,
            };
            if let Some(rank) = probe.match_rank(&hint.app.name) {
                let better = chosen
                    .as_ref()
                    .is_none_or(|(best_rank, _, _, _)| rank > *best_rank);
                if better {
                    chosen = Some((rank, bus, path, name));
                    if rank == AppMatchRank::Exact {
                        break;
                    }
                }
            }
        } else if !name.is_empty() {
            chosen = Some((AppMatchRank::Exact, bus, path, name));
            break;
        }
    }

    let Some((_, app_bus, app_path, app_name)) = chosen else {
        if let Some(explicit) = app_filter {
            return Err(RpcError::with_next(
                ErrorCode::NotFound,
                format!("no accessible AT-SPI2 application matching {explicit:?}"),
                "Open the application or call list_windows to inspect available apps.",
            ));
        }
        return Ok(None);
    };

    let app_info = win_hint.map(|w| w.app.clone()).unwrap_or(AppInfo {
        name: app_name,
        bundle_id: None,
        pid: None,
        path: None,
        running: true,
    });

    // Prefer first frame/window child of the application so the tree starts at the window.
    if let Ok(app_proxy) =
        uncached_proxy(conn, &app_bus, &app_path, "org.a11y.atspi.Accessible").await
    {
        let windows = get_children_via(&app_proxy).await;
        if let Some((wbus, wpath)) = windows.into_iter().next() {
            let wtitle = if let Ok(wp) =
                uncached_proxy(conn, &wbus, &wpath, "org.a11y.atspi.Accessible").await
            {
                wp.get_property::<String>("Name").await.ok()
            } else {
                None
            };
            return Ok(Some((
                wbus,
                wpath,
                app_info,
                wtitle,
                win_hint.map(|w| w.id),
            )));
        }
    }

    Ok(Some((
        app_bus,
        app_path,
        app_info,
        None,
        win_hint.map(|w| w.id),
    )))
}

async fn get_children_via(proxy: &Proxy<'_>) -> Vec<(String, String)> {
    let count: i32 = proxy.get_property("ChildCount").await.unwrap_or(0);
    let capped = count.clamp(0, 64) as usize;
    get_children_range(proxy, 0, capped).await
}

async fn get_children_range(proxy: &Proxy<'_>, start: usize, end: usize) -> Vec<(String, String)> {
    if start >= end {
        return Vec::new();
    }
    // Issue all `GetChildAtIndex` calls in this batch concurrently, preserving sibling order.
    let results = join_all((start..end).map(|idx| async move {
        proxy
            .call::<_, _, (String, OwnedObjectPath)>("GetChildAtIndex", &(idx as i32,))
            .await
            .ok()
            .filter(|(cbus, cpath)| !cbus.is_empty() && cpath.as_str() != "/org/a11y/atspi/null")
            .map(|(cbus, cpath)| (cbus, cpath.to_string()))
    }))
    .await;
    results.into_iter().flatten().collect()
}

async fn get_extents(conn: &Connection, bus: &str, path: &str) -> Option<Rect> {
    let proxy = uncached_proxy(conn, bus, path, "org.a11y.atspi.Component")
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
    let proxy = uncached_proxy(conn, bus, path, "org.a11y.atspi.Text")
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
