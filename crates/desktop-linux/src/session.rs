use std::path::Path;

use desktop_proto::AppInfo;

/// Active Linux display/session mode detected or configured for `desktopd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionKind {
    /// Interactive or virtual X11 session (`$DISPLAY`).
    X11 { display: String },
    /// Wayland compositor (`$WAYLAND_DISPLAY`), optionally paired with XWayland (`$DISPLAY`).
    Wayland {
        wayland_display: String,
        xwayland_display: Option<String>,
    },
    /// Private daemon-owned headless `Xvfb` virtual display.
    HeadlessXvfb { display: String },
}

impl SessionKind {
    /// X11 display string if X11, XWayland, or managed Xvfb is available.
    #[must_use]
    pub fn x11_display(&self) -> Option<&str> {
        match self {
            Self::X11 { display } | Self::HeadlessXvfb { display } => Some(display.as_str()),
            Self::Wayland {
                xwayland_display, ..
            } => xwayland_display.as_deref(),
        }
    }

    #[must_use]
    pub fn is_wayland(&self) -> bool {
        matches!(self, Self::Wayland { .. })
    }

    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::X11 { .. } => "x11",
            Self::Wayland { .. } => "wayland",
            Self::HeadlessXvfb { .. } => "xvfb",
        }
    }
}

/// Detect the active graphical session from the current process environment.
#[must_use]
pub fn detect_session(display_override: Option<&str>) -> Option<SessionKind> {
    if let Some(d) = display_override.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(SessionKind::X11 {
            display: d.to_string(),
        });
    }
    let wayland = std::env::var("WAYLAND_DISPLAY").ok();
    let display = std::env::var("DISPLAY").ok();
    let session_type = std::env::var("XDG_SESSION_TYPE").ok();
    detect_session_from_env(
        wayland.as_deref(),
        display.as_deref(),
        session_type.as_deref(),
    )
}

/// Pure detection helper from environment variables (unit-tested).
#[must_use]
pub fn detect_session_from_env(
    wayland_display: Option<&str>,
    x11_display: Option<&str>,
    xdg_session_type: Option<&str>,
) -> Option<SessionKind> {
    let wayland = wayland_display.map(str::trim).filter(|s| !s.is_empty());
    let x11 = x11_display.map(str::trim).filter(|s| !s.is_empty());
    let kind_hint = xdg_session_type.map(str::trim).map(str::to_ascii_lowercase);

    if let Some(w) = wayland {
        // Respect explicit X11 override only if XDG_SESSION_TYPE=x11 and DISPLAY is set.
        if kind_hint.as_deref() == Some("x11")
            && let Some(d) = x11
        {
            return Some(SessionKind::X11 {
                display: d.to_string(),
            });
        }
        return Some(SessionKind::Wayland {
            wayland_display: w.to_string(),
            xwayland_display: x11.map(ToString::to_string),
        });
    }

    x11.map(|d| SessionKind::X11 {
        display: d.to_string(),
    })
}

/// Best-effort local hostname without requiring FFI.
#[must_use]
pub fn local_host_name() -> String {
    if let Ok(raw) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(env_host) = std::env::var("HOSTNAME")
        && !env_host.trim().is_empty()
    {
        return env_host.trim().to_string();
    }
    if let Ok(out) = std::process::Command::new("hostname").output()
        && out.status.success()
    {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    "linux-host".to_string()
}

/// Resolve application metadata from `/proc/<pid>` with fallback to X11 `WM_CLASS`.
#[must_use]
pub fn app_info_from_pid(pid: u32, wm_class: Option<(&str, &str)>) -> AppInfo {
    let proc_comm = if pid > 0 {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    let proc_exe = if pid > 0 {
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    let class_name = wm_class.and_then(|(inst, cls)| {
        if !cls.trim().is_empty() {
            Some(cls.trim().to_string())
        } else if !inst.trim().is_empty() {
            Some(inst.trim().to_string())
        } else {
            None
        }
    });

    let name = class_name
        .clone()
        .or(proc_comm)
        .or_else(|| {
            proc_exe
                .as_deref()
                .and_then(|p| Path::new(p).file_name())
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let bundle_id = wm_class.and_then(|(inst, cls)| {
        let pick = if !inst.trim().is_empty() {
            inst.trim()
        } else {
            cls.trim()
        };
        if pick.is_empty() {
            None
        } else {
            Some(pick.to_string())
        }
    });

    AppInfo {
        name,
        bundle_id,
        pid: (pid > 0).then_some(pid),
        path: proc_exe,
        running: true,
    }
}

/// Check whether an executable exists on `PATH` without spawning a process.
#[must_use]
pub fn command_exists(bin: &str) -> bool {
    if bin.contains('/') {
        let p = Path::new(bin);
        return p.is_file();
    }
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| dir.join(bin).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_x11_wayland_and_empty_sessions() {
        assert_eq!(
            detect_session_from_env(None, Some(":0"), None),
            Some(SessionKind::X11 {
                display: ":0".into()
            })
        );
        assert_eq!(
            detect_session_from_env(Some("wayland-0"), Some(":1"), Some("wayland")),
            Some(SessionKind::Wayland {
                wayland_display: "wayland-0".into(),
                xwayland_display: Some(":1".into()),
            })
        );
        assert_eq!(
            detect_session_from_env(Some("wayland-0"), Some(":0"), Some("x11")),
            Some(SessionKind::X11 {
                display: ":0".into()
            })
        );
        assert_eq!(detect_session_from_env(None, None, None), None);
    }

    #[test]
    fn falls_back_to_wm_class_when_proc_unavailable() {
        let info = app_info_from_pid(0, Some(("firefox", "Firefox")));
        assert_eq!(info.name, "Firefox");
        assert_eq!(info.bundle_id.as_deref(), Some("firefox"));
        assert_eq!(info.pid, None);
        assert!(info.running);
    }
}
