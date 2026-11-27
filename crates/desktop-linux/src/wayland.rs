use std::collections::HashMap;
use std::io::Write as _;
use std::process::{Command, Stdio};

use desktop_core::{AnnotateBox, RawScreenshot, StepExecInfo};
use desktop_proto::{
    ErrorCode, ImageFormat, KeyCode, KeyCombo, MouseButton, Point, Rect, RpcError,
    ScreenshotTarget, ScrollDirection,
};
use uuid::Uuid;
use zbus::{Connection, OwnedObjectPath, Variant};

use crate::atspi::{block_on_dbus, uncached_proxy};
use crate::image_encode::{decode_to_rgba, encode_rgba_screenshot};
use crate::session::command_exists;

/// Diagnostics summary of available Wayland helper capabilities on this host.
#[derive(Debug, Clone)]
pub struct WaylandCapabilities {
    pub has_grim: bool,
    pub has_wl_clipboard: bool,
    pub has_ydotool: bool,
    pub has_wtype: bool,
}

impl WaylandCapabilities {
    #[must_use]
    pub fn probe() -> Self {
        Self {
            has_grim: command_exists("grim"),
            has_wl_clipboard: command_exists("wl-copy") && command_exists("wl-paste"),
            has_ydotool: command_exists("ydotool"),
            has_wtype: command_exists("wtype"),
        }
    }
}

/// Probe whether `org.freedesktop.portal.Desktop` is available on the session D-Bus.
#[must_use]
pub fn is_screenshot_portal_available() -> bool {
    block_on_dbus(async {
        let conn = Connection::session()
            .await
            .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("session bus: {e}")))?;
        let proxy = uncached_proxy(
            &conn,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .await?;
        let has_owner: bool = proxy
            .call("NameHasOwner", &("org.freedesktop.portal.Desktop",))
            .await
            .unwrap_or(false);
        Ok(has_owner)
    })
    .unwrap_or(false)
}

/// Capture a screenshot on a pure Wayland session via `grim` (wlroots) or XDG Screenshot portal.
pub fn capture_wayland(
    target: &ScreenshotTarget,
    format: ImageFormat,
    annotate: &[AnnotateBox],
) -> Result<RawScreenshot, RpcError> {
    // Fast path 1: `grim` on wlroots compositors (Sway, Hyprland, River, Wayfire).
    if command_exists("grim") {
        let mut cmd = Command::new("grim");
        cmd.args(["-t", "png"]);
        let mut custom_bounds: Option<Rect> = None;
        if let ScreenshotTarget::Region {
            x,
            y,
            width,
            height,
            ..
        } = target
        {
            cmd.args([
                "-g",
                &format!(
                    "{},{} {}x{}",
                    x.round() as i32,
                    y.round() as i32,
                    width.round().max(1.0) as u32,
                    height.round().max(1.0) as u32
                ),
            ]);
            custom_bounds = Some(Rect {
                x: *x,
                y: *y,
                width: *width,
                height: *height,
            });
        }
        cmd.arg("-");
        let out = cmd.output().map_err(|e| {
            RpcError::new(
                ErrorCode::BackendError,
                format!("grim execution failed: {e}"),
            )
        })?;
        if out.status.success() && !out.stdout.is_empty() {
            let (rgba, w, h) = decode_to_rgba(&out.stdout)?;
            let bounds = custom_bounds.unwrap_or(Rect {
                x: 0.0,
                y: 0.0,
                width: f64::from(w),
                height: f64::from(h),
            });
            return encode_rgba_screenshot(rgba, w, h, bounds, format, annotate);
        }
    }

    // Fast path 2: XDG Desktop Portal Screenshot (GNOME, KDE, wlroots portal).
    let png_bytes = portal_screenshot_bytes()?;
    let (rgba, w, h) = decode_to_rgba(&png_bytes)?;
    let bounds = Rect {
        x: 0.0,
        y: 0.0,
        width: f64::from(w),
        height: f64::from(h),
    };
    encode_rgba_screenshot(rgba, w, h, bounds, format, annotate)
}

/// Compute the deterministic XDG Desktop Portal request object path per the portal spec.
#[must_use]
pub fn portal_request_path(unique_name: &str, handle_token: &str) -> String {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    format!("/org/freedesktop/portal/desktop/request/{sender}/{handle_token}")
}

/// Decode a `file://` URI (handling percent-encoded UTF-8 bytes such as `%20`).
#[must_use]
pub fn decode_file_uri(uri: &str) -> String {
    let raw = uri
        .strip_prefix("file://localhost")
        .or_else(|| uri.strip_prefix("file://"))
        .unwrap_or(uri);
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn portal_screenshot_bytes() -> Result<Vec<u8>, RpcError> {
    block_on_dbus(async {
        let conn = Connection::session().await.map_err(|e| {
            RpcError::with_next(
                ErrorCode::BackendError,
                format!("session D-Bus unavailable for Wayland screenshot portal: {e}"),
                "Run inside a Wayland session with xdg-desktop-portal, install grim, or run desktopd --xvfb.",
            )
        })?;
        let unique_name = conn
            .unique_name()
            .map(|n| n.as_str().to_string())
            .ok_or_else(|| {
                RpcError::new(
                    ErrorCode::BackendError,
                    "session D-Bus connection has no unique name",
                )
            })?;

        let token = format!("agent_desktop_{}", Uuid::new_v4().simple());
        let expected_path = portal_request_path(&unique_name, &token);

        // Subscribe to Request.Response BEFORE invoking Screenshot to avoid missing fast responses.
        let req_proxy = uncached_proxy(
            &conn,
            "org.freedesktop.portal.Desktop",
            &expected_path,
            "org.freedesktop.portal.Request",
        )
        .await?;
        let mut stream = req_proxy.receive_signal("Response").await.map_err(|e| {
            RpcError::new(
                ErrorCode::BackendError,
                format!("portal signal subscribe error: {e}"),
            )
        })?;

        let proxy = uncached_proxy(
            &conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Screenshot",
        )
        .await?;

        let mut opts: HashMap<&str, Variant<'_>> = HashMap::new();
        opts.insert("handle_token", Variant::from(token.as_str()));
        opts.insert("interactive", Variant::from(false));

        let actual_path: OwnedObjectPath =
            proxy.call("Screenshot", &("", opts)).await.map_err(|e| {
                RpcError::new(
                    ErrorCode::BackendError,
                    format!("portal Screenshot call failed: {e}"),
                )
            })?;

        // Fallback if an older portal returns a non-standard request path.
        if actual_path.as_str() != expected_path {
            let alt_proxy = uncached_proxy(
                &conn,
                "org.freedesktop.portal.Desktop",
                actual_path.as_str(),
                "org.freedesktop.portal.Request",
            )
            .await?;
            stream = alt_proxy.receive_signal("Response").await.map_err(|e| {
                RpcError::new(
                    ErrorCode::BackendError,
                    format!("portal signal subscribe error: {e}"),
                )
            })?;
        }

        use futures_util::StreamExt as _;
        let sig = tokio::time::timeout(std::time::Duration::from_secs(6), stream.next())
            .await
            .map_err(|_| {
                RpcError::new(
                    ErrorCode::Timeout,
                    "timed out waiting for Wayland screenshot portal response",
                )
            })?
            .ok_or_else(|| {
                RpcError::new(
                    ErrorCode::BackendError,
                    "Wayland screenshot portal stream closed",
                )
            })?;

        let (response, results): (u32, HashMap<String, zbus::OwnedVariant>) =
            sig.body().deserialize().map_err(|e| {
                RpcError::new(
                    ErrorCode::BackendError,
                    format!("decode portal response failed: {e}"),
                )
            })?;
        if response != 0 {
            return Err(RpcError::with_next(
                ErrorCode::PermissionDenied,
                format!("Wayland screenshot portal rejected capture (code {response})"),
                "Allow non-interactive screenshot permission for desktopd in your compositor portal settings.",
            ));
        }

        let uri = results
            .get("uri")
            .and_then(|v| <&str>::try_from(v).ok())
            .ok_or_else(|| {
                RpcError::new(
                    ErrorCode::BackendError,
                    "Wayland screenshot portal returned no image URI",
                )
            })?;
        let path = decode_file_uri(uri);
        let bytes = std::fs::read(&path).map_err(|e| {
            RpcError::new(
                ErrorCode::BackendError,
                format!("failed to read portal screenshot {path}: {e}"),
            )
        })?;
        let _ = std::fs::remove_file(&path);
        Ok(bytes)
    })
}

pub fn read_wayland_clipboard() -> Result<String, RpcError> {
    let out = Command::new("wl-paste")
        .arg("--no-newline")
        .output()
        .map_err(|e| {
            RpcError::with_next(
                ErrorCode::BackendError,
                format!("wl-paste failed: {e}"),
                "Install wl-clipboard (`sudo apt install wl-clipboard`).",
            )
        })?;
    if !out.status.success() {
        return Err(RpcError::new(
            ErrorCode::BackendError,
            "wl-paste returned non-zero status (clipboard may be empty)",
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn write_wayland_clipboard(text: &str) -> Result<(), RpcError> {
    let mut child = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            RpcError::with_next(
                ErrorCode::BackendError,
                format!("wl-copy failed: {e}"),
                "Install wl-clipboard (`sudo apt install wl-clipboard`).",
            )
        })?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let status = child
        .wait()
        .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("wl-copy wait: {e}")))?;
    if !status.success() {
        return Err(RpcError::new(
            ErrorCode::BackendError,
            "wl-copy exited with non-zero status",
        ));
    }
    Ok(())
}

/// Click a point on pure Wayland via `ydotool` when XWayland is unavailable.
pub fn wayland_click_point(
    point: Point,
    button: MouseButton,
    count: u8,
) -> Result<StepExecInfo, RpcError> {
    if !command_exists("ydotool") {
        return Err(RpcError::with_next(
            ErrorCode::BackendError,
            "pure Wayland pointer injection requires ydotool (or an X11/Xvfb session)",
            "Install and start ydotoold/ydotool, use AT-SPI2 semantic refs, or run `desktopd --xvfb`.",
        ));
    }
    run_ydotool(&[
        "mousemove",
        "--absolute",
        "-x",
        &format!("{}", point.x.round() as i32),
        "-y",
        &format!("{}", point.y.round() as i32),
    ])?;
    let code = match button {
        MouseButton::Left => "0xC0",
        MouseButton::Right => "0xC1",
        MouseButton::Middle => "0xC2",
    };
    for _ in 0..count.max(1) {
        run_ydotool(&["click", code])?;
    }
    Ok(StepExecInfo {
        detail: format!(
            "ydotool clicked ({:.0},{:.0}) x{}",
            point.x,
            point.y,
            count.max(1)
        ),
        used_background: false,
    })
}

pub fn wayland_hover_point(point: Point) -> Result<StepExecInfo, RpcError> {
    if !command_exists("ydotool") {
        return Err(RpcError::with_next(
            ErrorCode::BackendError,
            "pure Wayland pointer movement requires ydotool",
            "Install ydotool/ydotoold or run under X11/Xvfb.",
        ));
    }
    run_ydotool(&[
        "mousemove",
        "--absolute",
        "-x",
        &format!("{}", point.x.round() as i32),
        "-y",
        &format!("{}", point.y.round() as i32),
    ])?;
    Ok(StepExecInfo {
        detail: format!("ydotool hovered ({:.0},{:.0})", point.x, point.y),
        used_background: false,
    })
}

pub fn wayland_scroll(
    point: Point,
    direction: ScrollDirection,
    amount: u32,
) -> Result<StepExecInfo, RpcError> {
    if !command_exists("ydotool") {
        return Err(RpcError::with_next(
            ErrorCode::BackendError,
            "pure Wayland scroll requires ydotool",
            "Install ydotool/ydotoold or run under X11/Xvfb.",
        ));
    }
    let _ = wayland_hover_point(point)?;
    let amt = amount.clamp(1, 50) as i32;
    let (dx, dy) = match direction {
        ScrollDirection::Up => (0, amt),
        ScrollDirection::Down => (0, -amt),
        ScrollDirection::Left => (-amt, 0),
        ScrollDirection::Right => (amt, 0),
    };
    run_ydotool(&[
        "mousemove",
        "--wheel",
        "-x",
        &dx.to_string(),
        "-y",
        &dy.to_string(),
    ])?;
    Ok(StepExecInfo {
        detail: format!("ydotool scrolled {:?} x{}", direction, amount),
        used_background: false,
    })
}

pub fn wayland_type_text(text: &str) -> Result<StepExecInfo, RpcError> {
    if command_exists("wtype") {
        let status = Command::new("wtype")
            .arg("--")
            .arg(text)
            .status()
            .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("wtype failed: {e}")))?;
        if status.success() {
            return Ok(StepExecInfo {
                detail: format!("wtype entered {} chars", text.chars().count()),
                used_background: false,
            });
        }
    }
    if command_exists("ydotool") {
        run_ydotool(&["type", "--", text])?;
        return Ok(StepExecInfo {
            detail: format!("ydotool typed {} chars", text.chars().count()),
            used_background: false,
        });
    }
    Err(RpcError::with_next(
        ErrorCode::BackendError,
        "pure Wayland text typing requires wtype or ydotool (or AT-SPI2 EditableText)",
        "Install wtype or ydotool, pass a ref with EditableText support, or run `desktopd --xvfb`.",
    ))
}

/// Exact XKB keysym names accepted by `wtype -k <name>`.
#[must_use]
pub fn xkb_keysym_name(key: KeyCode) -> &'static str {
    match key {
        KeyCode::A => "a",
        KeyCode::B => "b",
        KeyCode::C => "c",
        KeyCode::D => "d",
        KeyCode::E => "e",
        KeyCode::F => "f",
        KeyCode::G => "g",
        KeyCode::H => "h",
        KeyCode::I => "i",
        KeyCode::J => "j",
        KeyCode::K => "k",
        KeyCode::L => "l",
        KeyCode::M => "m",
        KeyCode::N => "n",
        KeyCode::O => "o",
        KeyCode::P => "p",
        KeyCode::Q => "q",
        KeyCode::R => "r",
        KeyCode::S => "s",
        KeyCode::T => "t",
        KeyCode::U => "u",
        KeyCode::V => "v",
        KeyCode::W => "w",
        KeyCode::X => "x",
        KeyCode::Y => "y",
        KeyCode::Z => "z",
        KeyCode::Digit0 => "0",
        KeyCode::Digit1 => "1",
        KeyCode::Digit2 => "2",
        KeyCode::Digit3 => "3",
        KeyCode::Digit4 => "4",
        KeyCode::Digit5 => "5",
        KeyCode::Digit6 => "6",
        KeyCode::Digit7 => "7",
        KeyCode::Digit8 => "8",
        KeyCode::Digit9 => "9",
        KeyCode::Minus => "minus",
        KeyCode::Equal => "equal",
        KeyCode::LeftBracket => "bracketleft",
        KeyCode::RightBracket => "bracketright",
        KeyCode::Backslash => "backslash",
        KeyCode::Semicolon => "semicolon",
        KeyCode::Quote => "apostrophe",
        KeyCode::Comma => "comma",
        KeyCode::Period => "period",
        KeyCode::Slash => "slash",
        KeyCode::Grave => "grave",
        KeyCode::Return => "Return",
        KeyCode::Tab => "Tab",
        KeyCode::Space => "space",
        KeyCode::Backspace => "BackSpace",
        KeyCode::Delete => "Delete",
        KeyCode::Escape => "Escape",
        KeyCode::Home => "Home",
        KeyCode::End => "End",
        KeyCode::PageUp => "Page_Up",
        KeyCode::PageDown => "Page_Down",
        KeyCode::Left => "Left",
        KeyCode::Right => "Right",
        KeyCode::Up => "Up",
        KeyCode::Down => "Down",
        KeyCode::F1 => "F1",
        KeyCode::F2 => "F2",
        KeyCode::F3 => "F3",
        KeyCode::F4 => "F4",
        KeyCode::F5 => "F5",
        KeyCode::F6 => "F6",
        KeyCode::F7 => "F7",
        KeyCode::F8 => "F8",
        KeyCode::F9 => "F9",
        KeyCode::F10 => "F10",
        KeyCode::F11 => "F11",
        KeyCode::F12 => "F12",
    }
}

pub fn wayland_key_combo(combo: KeyCombo, repeat: u8) -> Result<StepExecInfo, RpcError> {
    if !command_exists("wtype") {
        return Err(RpcError::with_next(
            ErrorCode::BackendError,
            "pure Wayland key combos require wtype (or X11/XWayland/Xvfb)",
            "Install wtype or run under X11/Xvfb.",
        ));
    }
    let key_name = xkb_keysym_name(combo.key);
    let mut args: Vec<&str> = Vec::new();
    if combo.modifiers.ctrl {
        args.extend(["-M", "ctrl"]);
    }
    if combo.modifiers.alt {
        args.extend(["-M", "alt"]);
    }
    if combo.modifiers.shift {
        args.extend(["-M", "shift"]);
    }
    if combo.modifiers.cmd {
        args.extend(["-M", "logo"]);
    }
    args.extend(["-k", key_name]);
    if combo.modifiers.cmd {
        args.extend(["-m", "logo"]);
    }
    if combo.modifiers.shift {
        args.extend(["-m", "shift"]);
    }
    if combo.modifiers.alt {
        args.extend(["-m", "alt"]);
    }
    if combo.modifiers.ctrl {
        args.extend(["-m", "ctrl"]);
    }

    let count = repeat.max(1);
    for _ in 0..count {
        let status = Command::new("wtype")
            .args(&args)
            .status()
            .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("wtype failed: {e}")))?;
        if !status.success() {
            return Err(RpcError::new(
                ErrorCode::BackendError,
                format!("wtype exited with non-zero status for {key_name}"),
            ));
        }
    }
    Ok(StepExecInfo {
        detail: format!("wtype pressed {key_name} x{count}"),
        used_background: false,
    })
}

fn run_ydotool(args: &[&str]) -> Result<(), RpcError> {
    let status = Command::new("ydotool")
        .args(args)
        .status()
        .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("ydotool failed: {e}")))?;
    if !status.success() {
        return Err(RpcError::with_next(
            ErrorCode::BackendError,
            format!("ydotool {:?} exited with {status}", args),
            "Ensure ydotoold is running and your user has access to /dev/uinput.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_xkb_keysym_names_and_decodes_portal_uris() {
        assert_eq!(xkb_keysym_name(KeyCode::A), "a");
        assert_eq!(xkb_keysym_name(KeyCode::Digit0), "0");
        assert_eq!(xkb_keysym_name(KeyCode::Backspace), "BackSpace");
        assert_eq!(xkb_keysym_name(KeyCode::Space), "space");
        assert_eq!(xkb_keysym_name(KeyCode::PageUp), "Page_Up");
        assert_eq!(xkb_keysym_name(KeyCode::Quote), "apostrophe");

        assert_eq!(
            portal_request_path(":1.42", "tok123"),
            "/org/freedesktop/portal/desktop/request/1_42/tok123"
        );
        assert_eq!(
            decode_file_uri("file:///tmp/Screenshot%20from%202026.png"),
            "/tmp/Screenshot from 2026.png"
        );
        assert_eq!(
            decode_file_uri("file://localhost/tmp/caf%C3%A9.png"),
            "/tmp/café.png"
        );
    }
}
