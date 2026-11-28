use std::path::Path;
use std::ptr;
use std::sync::OnceLock;

use desktop_proto::{
    AppInfo, AppMatchRank, DisplayInfo, ErrorCode, FocusMode, Point, Rect, RpcError, WindowInfo,
    filter_windows_by_app,
};
use objc2_app_kit::{
    NSApplicationActivationOptions, NSApplicationActivationPolicy, NSRunningApplication,
    NSWorkspace,
};
use objc2_foundation::{NSBundle, NSString, NSURL};

use crate::ffi::{
    self, CFArrayRef, CGDirectDisplayID, CGRect, CfOwned, K_CG_NULL_WINDOW_ID,
    K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS, K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY, cf_dict_get,
    cf_to_f64, cf_to_i64, cf_to_string,
};

static LOCAL_HOST_NAME: OnceLock<String> = OnceLock::new();

#[must_use]
pub fn local_host_name() -> String {
    LOCAL_HOST_NAME
        .get_or_init(|| {
            std::process::Command::new("scutil")
                .args(["--get", "LocalHostName"])
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                        (!s.is_empty()).then_some(s)
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| "local-mac".into())
        })
        .clone()
}

pub fn displays() -> Result<Vec<DisplayInfo>, RpcError> {
    let mut count: u32 = 0;
    // SAFETY: Queries the number of active displays.
    let rc = unsafe { ffi::CGGetActiveDisplayList(0, ptr::null_mut(), &mut count) };
    if rc != 0 {
        return Err(RpcError::new(
            ErrorCode::BackendError,
            format!("CGGetActiveDisplayList failed ({rc})"),
        ));
    }
    let mut ids = vec![0 as CGDirectDisplayID; count as usize];
    if count > 0 {
        // SAFETY: `ids` has capacity `count`.
        let rc = unsafe { ffi::CGGetActiveDisplayList(count, ids.as_mut_ptr(), &mut count) };
        if rc != 0 {
            return Err(RpcError::new(
                ErrorCode::BackendError,
                format!("CGGetActiveDisplayList read failed ({rc})"),
            ));
        }
        ids.truncate(count as usize);
    }

    let mut out = Vec::with_capacity(ids.len());
    for (idx, id) in ids.into_iter().enumerate() {
        // SAFETY: `id` is an active display ID from CoreGraphics.
        let bounds = unsafe { ffi::CGDisplayBounds(id) };
        let main = unsafe { ffi::CGDisplayIsMain(id) } != 0;
        let mode = unsafe { ffi::CGDisplayCopyDisplayMode(id) };
        let (pw, ph) = if !mode.is_null() {
            let pw = unsafe { ffi::CGDisplayModeGetPixelWidth(mode) } as u32;
            let ph = unsafe { ffi::CGDisplayModeGetPixelHeight(mode) } as u32;
            unsafe { ffi::CGDisplayModeRelease(mode) };
            (pw, ph)
        } else {
            (
                bounds.size.width.max(1.0) as u32,
                bounds.size.height.max(1.0) as u32,
            )
        };
        let scale = if bounds.size.width > 0.0 {
            f64::from(pw) / bounds.size.width
        } else {
            1.0
        };
        out.push(DisplayInfo {
            index: idx as u32,
            id,
            main,
            bounds: Rect {
                x: bounds.origin.x,
                y: bounds.origin.y,
                width: bounds.size.width,
                height: bounds.size.height,
            },
            pixel_width: pw,
            pixel_height: ph,
            scale,
        });
    }
    Ok(out)
}

pub fn app_info_from_running(app: &NSRunningApplication) -> AppInfo {
    let name = app
        .localizedName()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let bundle_id = app.bundleIdentifier().map(|s| s.to_string());
    let pid = Some(app.processIdentifier() as u32);
    let path = app
        .bundleURL()
        .and_then(|u| u.path())
        .map(|p| p.to_string());
    AppInfo {
        name,
        bundle_id,
        pid,
        path,
        running: true,
    }
}

pub fn running_app_for_pid(pid: i32) -> Option<AppInfo> {
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    Some(app_info_from_running(&app))
}

#[must_use]
pub fn frontmost_app() -> Option<AppInfo> {
    let ws = NSWorkspace::sharedWorkspace();
    let app = ws.frontmostApplication()?;
    Some(app_info_from_running(&app))
}

#[must_use]
pub fn frontmost_pid() -> Option<i32> {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|a| a.processIdentifier())
}

pub fn activate_pid(pid: i32) -> bool {
    let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) else {
        return false;
    };
    #[allow(deprecated)]
    app.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps)
}

fn bundle_info_from_path(path: &Path) -> Option<AppInfo> {
    let ns_path = NSString::from_str(&path.to_string_lossy());
    let bundle = NSBundle::bundleWithPath(&ns_path)?;
    let bundle_id = bundle.bundleIdentifier().map(|s| s.to_string());
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some(AppInfo {
        name,
        bundle_id,
        pid: None,
        path: Some(path.to_string_lossy().into_owned()),
        running: false,
    })
}

fn activation_policy_priority(policy: NSApplicationActivationPolicy) -> u8 {
    if policy == NSApplicationActivationPolicy::Regular {
        2
    } else if policy == NSApplicationActivationPolicy::Accessory {
        1
    } else {
        0
    }
}

pub(crate) fn select_best_app_candidate<I>(
    candidates: I,
    query: &str,
) -> Option<(AppMatchRank, AppInfo)>
where
    I: IntoIterator<Item = (AppInfo, u8)>,
{
    let mut best: Option<(AppMatchRank, u8, usize, AppInfo)> = None;
    for (info, policy_priority) in candidates {
        let Some(rank) = info.match_rank(query) else {
            continue;
        };
        let name_len = info.name.len();
        let better = match &best {
            None => true,
            Some((best_rank, best_priority, best_len, _)) => {
                (rank, policy_priority, std::cmp::Reverse(name_len))
                    > (*best_rank, *best_priority, std::cmp::Reverse(*best_len))
            }
        };
        if better {
            best = Some((rank, policy_priority, name_len, info));
        }
    }
    best.map(|(rank, _, _, info)| (rank, info))
}

pub fn find_app(query: &str) -> Option<AppInfo> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    let ws = NSWorkspace::sharedWorkspace();
    let running = ws.runningApplications();
    let best_running = select_best_app_candidate(
        running.iter().map(|app| {
            let prio = activation_policy_priority(app.activationPolicy());
            (app_info_from_running(&app), prio)
        }),
        q,
    );
    if let Some((AppMatchRank::Exact, info)) = best_running {
        return Some(info);
    }

    // Try exact bundle ID lookup via NSWorkspace before settling for a non-exact running match.
    let ns_bid = NSString::from_str(q);
    if let Some(url) = ws.URLForApplicationWithBundleIdentifier(&ns_bid)
        && let Some(p) = url.path()
        && let Some(info) = bundle_info_from_path(Path::new(&p.to_string()))
    {
        return Some(info);
    }

    // Scan standard application directories by exact `.app` name (including CoreServices for Finder, etc.).
    let home = std::env::var("HOME").unwrap_or_default();
    let dirs = [
        "/Applications".to_string(),
        "/System/Applications".to_string(),
        "/System/Applications/Utilities".to_string(),
        "/System/Library/CoreServices".to_string(),
        format!("{home}/Applications"),
    ];
    let file_name = if q.ends_with(".app") {
        q.to_string()
    } else {
        format!("{q}.app")
    };
    for dir in &dirs {
        let candidate = Path::new(dir).join(&file_name);
        if candidate.exists()
            && let Some(info) = bundle_info_from_path(&candidate)
        {
            return Some(info);
        }
    }

    // If a running app matched by word-prefix or substring, prefer it over launching an uninstalled/stopped bundle.
    if let Some((_, info)) = best_running {
        return Some(info);
    }

    // Fallback: match installed `.app` bundles on disk by word-prefix or substring (e.g. "T3 Code" -> "T3 Code (Beta).app").
    let mut disk_candidates = Vec::new();
    for dir in &dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("app") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let probe = AppInfo {
                name: stem.to_string(),
                bundle_id: None,
                pid: None,
                path: Some(path.to_string_lossy().into_owned()),
                running: false,
            };
            if probe.match_rank(q).is_some()
                && let Some(info) = bundle_info_from_path(&path)
            {
                disk_candidates.push((info, 0));
            }
        }
    }
    select_best_app_candidate(disk_candidates, q).map(|(_, info)| info)
}

pub(crate) fn should_activate_running(focus_mode: FocusMode) -> bool {
    focus_mode == FocusMode::Foreground
}

pub(crate) fn open_command_args(
    app_path: Option<&str>,
    query: &str,
    focus_mode: FocusMode,
) -> Vec<String> {
    let mut args = Vec::with_capacity(3);
    if focus_mode != FocusMode::Foreground {
        args.push("-g".into());
    }
    if let Some(path) = app_path {
        args.push(path.into());
    } else {
        let flag = if query.contains('.') { "-b" } else { "-a" };
        args.push(flag.into());
        args.push(query.into());
    }
    args
}

pub fn open_app(query: &str, focus_mode: FocusMode) -> Result<AppInfo, RpcError> {
    let app = find_app(query).ok_or_else(|| {
        RpcError::with_next(
            ErrorCode::NotFound,
            format!("app {query:?} not found"),
            "Pass a valid macOS application name (e.g. \"Notes\") or bundle ID (e.g. \"com.apple.Notes\").",
        )
    })?;
    if let Some(pid) = app.pid {
        if !should_activate_running(focus_mode) {
            return Ok(app);
        }
        if activate_pid(pid as i32) {
            return Ok(app);
        }
    }
    let args = open_command_args(app.path.as_deref(), query, focus_mode);
    let status = std::process::Command::new("/usr/bin/open")
        .args(&args)
        .status()
        .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("open failed: {e}")))?;
    if !status.success() {
        return Err(RpcError::new(
            ErrorCode::BackendError,
            format!("open {} exited with {status}", args.join(" ")),
        ));
    }
    Ok(app)
}

pub fn windows(app_filter: Option<&str>) -> Result<Vec<WindowInfo>, RpcError> {
    if let Some(filter) = app_filter
        && filter.trim().is_empty()
    {
        return Ok(Vec::new());
    }
    let displays = displays()?;
    let front_pid = frontmost_pid();

    // SAFETY: CoreGraphics returns an owned CFArray of CFDictionary entries.
    let list_raw: CFArrayRef = unsafe {
        ffi::CGWindowListCopyWindowInfo(
            K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY | K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS,
            K_CG_NULL_WINDOW_ID,
        )
    };
    let Some(list) = (unsafe { CfOwned::from_owned(list_raw) }) else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    // SAFETY: `list` is a valid CFArrayRef.
    let count = unsafe { ffi::CFArrayGetCount(list.as_ptr()) };
    for i in 0..count {
        // SAFETY: `i < count` is within bounds.
        let dict = unsafe { ffi::CFArrayGetValueAtIndex(list.as_ptr(), i) };
        if dict.is_null() {
            continue;
        }
        let pid = unsafe { cf_to_i64(cf_dict_get(dict, "kCGWindowOwnerPID")) }.unwrap_or(-1) as i32;
        if pid <= 0 {
            continue;
        }
        let Some(app) = running_app_for_pid(pid) else {
            continue;
        };
        if let Some(filter) = app_filter
            && !app.matches_query(filter)
        {
            continue;
        }
        let alpha = unsafe { cf_to_f64(cf_dict_get(dict, "kCGWindowAlpha")) }.unwrap_or(1.0);
        if alpha <= 0.0 {
            continue;
        }
        let layer = unsafe { cf_to_i64(cf_dict_get(dict, "kCGWindowLayer")) }.unwrap_or(0) as i32;
        // Skip system chrome/overlays above normal floating window layers unless explicitly filtered.
        if app_filter.is_none() && layer != 0 {
            continue;
        }
        let wid = unsafe { cf_to_i64(cf_dict_get(dict, "kCGWindowNumber")) }.unwrap_or(0) as u32;
        let title = unsafe { cf_to_string(cf_dict_get(dict, "kCGWindowName")) }.unwrap_or_default();
        let bounds_dict = unsafe { cf_dict_get(dict, "kCGWindowBounds") };
        let mut cg_rect = CGRect::default();
        if bounds_dict.is_null()
            || !unsafe { ffi::CGRectMakeWithDictionaryRepresentation(bounds_dict, &mut cg_rect) }
        {
            continue;
        }
        let bounds = Rect {
            x: cg_rect.origin.x,
            y: cg_rect.origin.y,
            width: cg_rect.size.width,
            height: cg_rect.size.height,
        };
        if bounds.is_empty() {
            continue;
        }
        let center = bounds.center();
        let display_index = displays
            .iter()
            .find(|d| d.bounds.contains(center))
            .map(|d| d.index);
        let focused = front_pid == Some(pid) && !out.iter().any(|w: &WindowInfo| w.focused);
        out.push(WindowInfo {
            id: wid,
            title,
            app,
            bounds,
            layer,
            alpha,
            focused,
            minimized: false,
            on_screen: true,
            display_index,
        });
    }
    if let Some(filter) = app_filter {
        return Ok(filter_windows_by_app(out, filter));
    }
    Ok(out)
}

pub fn app_at_point(point: Point) -> Result<Option<AppInfo>, RpcError> {
    // SAFETY: CoreGraphics returns an owned CFArray of on-screen windows front-to-back.
    let list_raw = unsafe {
        ffi::CGWindowListCopyWindowInfo(
            K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY | K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS,
            K_CG_NULL_WINDOW_ID,
        )
    };
    let Some(list) = (unsafe { CfOwned::from_owned(list_raw) }) else {
        return Ok(frontmost_app());
    };
    let count = unsafe { ffi::CFArrayGetCount(list.as_ptr()) };
    for i in 0..count {
        let dict = unsafe { ffi::CFArrayGetValueAtIndex(list.as_ptr(), i) };
        if dict.is_null() {
            continue;
        }
        let alpha = unsafe { cf_to_f64(cf_dict_get(dict, "kCGWindowAlpha")) }.unwrap_or(1.0);
        if alpha <= 0.0 {
            continue;
        }
        let bounds_dict = unsafe { cf_dict_get(dict, "kCGWindowBounds") };
        let mut cg_rect = CGRect::default();
        if bounds_dict.is_null()
            || !unsafe { ffi::CGRectMakeWithDictionaryRepresentation(bounds_dict, &mut cg_rect) }
        {
            continue;
        }
        let rect = Rect {
            x: cg_rect.origin.x,
            y: cg_rect.origin.y,
            width: cg_rect.size.width,
            height: cg_rect.size.height,
        };
        if !rect.contains(point) {
            continue;
        }
        let pid = unsafe { cf_to_i64(cf_dict_get(dict, "kCGWindowOwnerPID")) }.unwrap_or(-1) as i32;
        if let Some(app) = running_app_for_pid(pid) {
            // Pass through transparent full-screen Dock wallpaper/backdrop layers.
            if app.bundle_id.as_deref() == Some("com.apple.dock") {
                continue;
            }
            return Ok(Some(app));
        }
    }
    Ok(frontmost_app())
}

pub fn focus_window(window_id: u32) -> Result<WindowInfo, RpcError> {
    let all = windows(None)?;
    let win = all.into_iter().find(|w| w.id == window_id).ok_or_else(|| {
        RpcError::with_next(
            ErrorCode::NotFound,
            format!("window w{window_id} not found"),
            "Call list_windows to inspect currently open window IDs.",
        )
    })?;
    if let Some(pid) = win.app.pid {
        activate_pid(pid as i32);
    }
    let _ = crate::ax::raise_window_by_title(win.app.pid.unwrap_or(0) as i32, &win.title);
    Ok(win)
}

#[allow(dead_code)]
fn _keep_nsurl_import(_: Option<&NSURL>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_app_respects_focus_mode_activation_and_command_args() {
        assert!(!should_activate_running(FocusMode::Background));
        assert!(!should_activate_running(FocusMode::Auto));
        assert!(should_activate_running(FocusMode::Foreground));

        assert_eq!(
            open_command_args(
                Some("/Applications/Notes.app"),
                "Notes",
                FocusMode::Background
            ),
            vec!["-g", "/Applications/Notes.app"]
        );
        assert_eq!(
            open_command_args(Some("/Applications/Notes.app"), "Notes", FocusMode::Auto),
            vec!["-g", "/Applications/Notes.app"]
        );
        assert_eq!(
            open_command_args(
                Some("/Applications/Notes.app"),
                "Notes",
                FocusMode::Foreground
            ),
            vec!["/Applications/Notes.app"]
        );

        assert_eq!(
            open_command_args(None, "Notes", FocusMode::Background),
            vec!["-g", "-a", "Notes"]
        );
        assert_eq!(
            open_command_args(None, "com.apple.Notes", FocusMode::Auto),
            vec!["-g", "-b", "com.apple.Notes"]
        );
        assert_eq!(
            open_command_args(None, "com.apple.Notes", FocusMode::Foreground),
            vec!["-b", "com.apple.Notes"]
        );
    }

    #[test]
    fn open_app_running_finder_in_background_and_auto_does_not_steal_focus() {
        let Some(finder) = find_app("Finder").filter(|a| a.pid.is_some()) else {
            return;
        };
        let before = frontmost_pid();
        let app_bg = open_app("Finder", FocusMode::Background).unwrap();
        assert_eq!(app_bg.bundle_id.as_deref(), Some("com.apple.finder"));
        let app_auto = open_app("Finder", FocusMode::Auto).unwrap();
        assert_eq!(app_auto.bundle_id.as_deref(), Some("com.apple.finder"));
        if before != finder.pid.map(|p| p as i32) {
            assert_eq!(frontmost_pid(), before);
        }
    }

    #[test]
    fn select_best_app_candidate_prefers_exact_and_regular_gui_app_over_helpers() {
        let helper = AppInfo {
            name: "T3 Code (Beta) Helper (Renderer)".into(),
            bundle_id: Some("com.t3tools.t3code.helper.renderer".into()),
            pid: Some(101),
            path: None,
            running: true,
        };
        let main_beta = AppInfo {
            name: "T3 Code (Beta)".into(),
            bundle_id: Some("com.t3tools.t3code".into()),
            pid: Some(100),
            path: Some("/Applications/T3 Code (Beta).app".into()),
            running: true,
        };
        let exact_app = AppInfo {
            name: "T3 Code".into(),
            bundle_id: Some("com.t3tools.t3code.stable".into()),
            pid: Some(102),
            path: Some("/Applications/T3 Code.app".into()),
            running: true,
        };

        // Helper appears first in the list, yet the Regular GUI app wins over Prohibited helper.
        let chosen =
            select_best_app_candidate(vec![(helper.clone(), 0), (main_beta.clone(), 2)], "T3 Code")
                .unwrap();
        assert_eq!(chosen.0, AppMatchRank::WordPrefix);
        assert_eq!(chosen.1.name, "T3 Code (Beta)");

        // Even with equal activation priority, shorter name ("T3 Code (Beta)") beats longer helper.
        let chosen_same_prio =
            select_best_app_candidate(vec![(helper.clone(), 2), (main_beta.clone(), 2)], "T3 Code")
                .unwrap();
        assert_eq!(chosen_same_prio.1.name, "T3 Code (Beta)");

        // Exact match always beats word-prefix even if listed last.
        let chosen_exact =
            select_best_app_candidate(vec![(helper, 0), (main_beta, 2), (exact_app, 2)], "T3 Code")
                .unwrap();
        assert_eq!(chosen_exact.0, AppMatchRank::Exact);
        assert_eq!(chosen_exact.1.name, "T3 Code");

        // Empty query returns None.
        assert!(select_best_app_candidate(Vec::<(AppInfo, u8)>::new(), "").is_none());
    }
}
