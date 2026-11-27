use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use desktop_core::{AnnotateBox, RawScreenshot, StepExecInfo};
use desktop_proto::{
    AppMatchRank, DisplayInfo, ErrorCode, FocusMode, ImageFormat, KeyCombo, Modifiers, MouseButton,
    Point, Rect, RpcError, ScreenState, ScreenshotTarget, ScrollDirection, WindowInfo,
    char_to_x11_keysym,
};
use x11rb::CURRENT_TIME;
use x11rb::connection::Connection as _;
use x11rb::protocol::composite;
use x11rb::protocol::randr::{self, Connection as RandrConnection};
use x11rb::protocol::screensaver;
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ClientMessageData, ClientMessageEvent, ConfigureWindowAux,
    ConnectionExt as _, EventMask, ImageFormat as X11ImageFormat, ImageOrder, InputFocus, MapState,
    StackMode, Visualid, Window,
};
use x11rb::protocol::xtest;
use x11rb::rust_connection::{DefaultStream, RustConnection};

use crate::image_encode::{ZPixmapFormat, encode_rgba_screenshot, zpixmap_to_rgba};
use crate::session::app_info_from_pid;

const XK_SHIFT_L: u32 = 0xffe1;
const XK_CONTROL_L: u32 = 0xffe3;
const XK_ALT_L: u32 = 0xffe9;
const XK_SUPER_L: u32 = 0xffeb;

const XTEST_KEY_PRESS: u8 = 2;
const XTEST_KEY_RELEASE: u8 = 3;
const XTEST_BUTTON_PRESS: u8 = 4;
const XTEST_BUTTON_RELEASE: u8 = 5;
const XTEST_MOTION_NOTIFY: u8 = 6;

/// Interned EWMH/ICCCM atoms cached per X11 connection.
struct Atoms {
    net_client_list_stacking: Atom,
    net_client_list: Atom,
    net_active_window: Atom,
    net_wm_name: Atom,
    utf8_string: Atom,
    net_wm_pid: Atom,
    net_wm_state: Atom,
    net_wm_state_hidden: Atom,
    net_wm_window_type: Atom,
    net_wm_window_type_desktop: Atom,
    net_wm_window_type_dock: Atom,
}

impl Atoms {
    fn intern(conn: &RustConnection) -> Result<Self, RpcError> {
        let intern_one = |name: &[u8]| -> Result<Atom, RpcError> {
            conn.intern_atom(false, name)
                .map_err(x11_err)?
                .reply()
                .map(|r| r.atom)
                .map_err(x11_reply_err)
        };
        Ok(Self {
            net_client_list_stacking: intern_one(b"_NET_CLIENT_LIST_STACKING")?,
            net_client_list: intern_one(b"_NET_CLIENT_LIST")?,
            net_active_window: intern_one(b"_NET_ACTIVE_WINDOW")?,
            net_wm_name: intern_one(b"_NET_WM_NAME")?,
            utf8_string: intern_one(b"UTF8_STRING")?,
            net_wm_pid: intern_one(b"_NET_WM_PID")?,
            net_wm_state: intern_one(b"_NET_WM_STATE")?,
            net_wm_state_hidden: intern_one(b"_NET_WM_STATE_HIDDEN")?,
            net_wm_window_type: intern_one(b"_NET_WM_WINDOW_TYPE")?,
            net_wm_window_type_desktop: intern_one(b"_NET_WM_WINDOW_TYPE_DESKTOP")?,
            net_wm_window_type_dock: intern_one(b"_NET_WM_WINDOW_TYPE_DOCK")?,
        })
    }
}

/// Pure-Rust X11 client wrapping `x11rb::rust_connection::RustConnection`.
pub struct X11Session {
    conn: RustConnection,
    screen_num: usize,
    atoms: Atoms,
}

impl X11Session {
    pub fn connect(display: &str, auth_cookie: Option<&[u8]>) -> Result<Self, RpcError> {
        let (conn, screen_num) = if let Some(cookie) = auth_cookie {
            connect_with_cookie(display, cookie)?
        } else {
            RustConnection::connect(Some(display)).map_err(|err| {
                RpcError::with_next(
                    ErrorCode::BackendError,
                    format!("failed to connect to X11 display {display:?}: {err}"),
                    "Verify DISPLAY is active or start desktopd with --xvfb for headless operation.",
                )
            })?
        };
        let atoms = Atoms::intern(&conn)?;
        Ok(Self {
            conn,
            screen_num,
            atoms,
        })
    }

    fn root(&self) -> Window {
        self.conn.setup().roots[self.screen_num].root
    }

    fn screen(&self) -> &xproto::Screen {
        &self.conn.setup().roots[self.screen_num]
    }

    fn msb_first(&self) -> bool {
        self.conn.setup().image_byte_order == ImageOrder::MSB_FIRST
    }

    fn visual_masks(&self, visual_id: Visualid) -> (u32, u32, u32) {
        for depth in &self.screen().allowed_depths {
            for vis in &depth.visuals {
                if vis.visual_id == visual_id {
                    return (vis.red_mask, vis.green_mask, vis.blue_mask);
                }
            }
        }
        (0x00ff_0000, 0x0000_ff00, 0x0000_00ff)
    }

    fn bits_per_pixel_for_depth(&self, depth: u8) -> u8 {
        self.conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|f| f.depth == depth)
            .map_or(32, |f| f.bits_per_pixel)
    }

    /// Query monitors via RandR, falling back cleanly to the root screen (e.g., `Xvfb`).
    pub fn displays(&self) -> Result<Vec<DisplayInfo>, RpcError> {
        let root = self.root();
        let primary_output = randr::get_output_primary(&self.conn, root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.output);

        if let Ok(cookie) = randr::get_screen_resources_current(&self.conn, root)
            && let Ok(res) = cookie.reply()
        {
            let mut displays = Vec::new();
            for output in res.outputs {
                let Ok(out_cookie) =
                    randr::get_output_info(&self.conn, output, res.config_timestamp)
                else {
                    continue;
                };
                let Ok(out_info) = out_cookie.reply() else {
                    continue;
                };
                if out_info.connection != RandrConnection::CONNECTED || out_info.crtc == 0 {
                    continue;
                }
                let Ok(crtc_cookie) =
                    randr::get_crtc_info(&self.conn, out_info.crtc, res.config_timestamp)
                else {
                    continue;
                };
                let Ok(crtc) = crtc_cookie.reply() else {
                    continue;
                };
                if crtc.width == 0 || crtc.height == 0 {
                    continue;
                }
                let idx = displays.len() as u32;
                let main = primary_output.map_or(idx == 0, |p| p == output);
                displays.push(DisplayInfo {
                    index: idx,
                    id: output,
                    bounds: Rect {
                        x: f64::from(crtc.x),
                        y: f64::from(crtc.y),
                        width: f64::from(crtc.width),
                        height: f64::from(crtc.height),
                    },
                    pixel_width: u32::from(crtc.width),
                    pixel_height: u32::from(crtc.height),
                    scale: 1.0,
                    main,
                });
            }
            if !displays.is_empty() {
                if !displays.iter().any(|d| d.main) {
                    displays[0].main = true;
                }
                return Ok(displays);
            }
        }

        // Fallback to X11 root screen dimensions (standard for Xvfb without RandR outputs).
        let screen = self.screen();
        let w = u32::from(screen.width_in_pixels);
        let h = u32::from(screen.height_in_pixels);
        Ok(vec![DisplayInfo {
            index: 0,
            id: root,
            bounds: Rect {
                x: 0.0,
                y: 0.0,
                width: f64::from(w),
                height: f64::from(h),
            },
            pixel_width: w,
            pixel_height: h,
            scale: 1.0,
            main: true,
        }])
    }

    /// List visible top-level windows using EWMH `_NET_CLIENT_LIST_STACKING`, with WM-less fallback.
    pub fn windows(&self, app_filter: Option<&str>) -> Result<Vec<WindowInfo>, RpcError> {
        if let Some(filter) = app_filter
            && filter.trim().is_empty()
        {
            return Ok(Vec::new());
        }
        let root = self.root();
        let displays = self.displays()?;
        let active_window = self.active_window_id();

        // Try EWMH stacking order (bottom-to-top -> reverse to front-to-back).
        let mut window_ids = self.read_window_list_prop(root, self.atoms.net_client_list_stacking);
        if window_ids.is_empty() {
            window_ids = self.read_window_list_prop(root, self.atoms.net_client_list);
        } else {
            window_ids.reverse();
        }

        // Fallback for WM-less Xvfb: direct mapped children of root.
        if window_ids.is_empty()
            && let Ok(tree) = self.conn.query_tree(root).map_err(x11_err)?.reply()
        {
            window_ids = tree.children.into_iter().rev().collect();
        }

        let mut ranked_out: Vec<(u8, WindowInfo)> = Vec::new();
        for wid in window_ids {
            let Ok(attrs) = self.conn.get_window_attributes(wid).map(|c| c.reply()) else {
                continue;
            };
            let Ok(attrs) = attrs else {
                continue;
            };
            if attrs.map_state != MapState::VIEWABLE || attrs.override_redirect {
                continue;
            }
            if self.is_ignored_window_type(wid) || self.is_hidden_state(wid) {
                continue;
            }

            let Some(bounds) = self.window_screen_bounds(wid) else {
                continue;
            };
            if bounds.width < 2.0 || bounds.height < 2.0 {
                continue;
            }

            let pid = self.read_u32_prop(wid, self.atoms.net_wm_pid).unwrap_or(0);
            let wm_class = self.read_wm_class(wid);
            let app = app_info_from_pid(
                pid,
                wm_class
                    .as_ref()
                    .map(|(inst, cls)| (inst.as_str(), cls.as_str())),
            );
            let title = self.read_window_title(wid).unwrap_or_default();

            let match_score = if let Some(filter) = app_filter {
                let trimmed = filter.trim();
                if let Some(rank) = app.match_rank(trimmed) {
                    match rank {
                        AppMatchRank::Exact => 4,
                        AppMatchRank::WordPrefix => 3,
                        AppMatchRank::Substring => 2,
                    }
                } else if !trimmed.ends_with('.')
                    && title.to_lowercase().contains(&trimmed.to_lowercase())
                {
                    1
                } else {
                    continue;
                }
            } else {
                4
            };

            let center = bounds.center();
            let display_index = displays
                .iter()
                .find(|d| d.bounds.contains(center))
                .map(|d| d.index);

            ranked_out.push((
                match_score,
                WindowInfo {
                    id: wid,
                    app,
                    title,
                    bounds,
                    layer: 0,
                    alpha: 1.0,
                    display_index,
                    focused: active_window == Some(wid),
                    minimized: false,
                    on_screen: true,
                },
            ));
        }
        if app_filter.is_some() {
            if ranked_out.iter().any(|(score, _)| *score == 4) {
                ranked_out.retain(|(score, _)| *score == 4);
            } else {
                ranked_out.sort_by(|(a, _), (b, _)| b.cmp(a));
            }
        }
        Ok(ranked_out.into_iter().map(|(_, w)| w).collect())
    }

    pub fn focused_window(&self) -> Result<Option<WindowInfo>, RpcError> {
        let wins = self.windows(None)?;
        if let Some(w) = wins.iter().find(|w| w.focused) {
            return Ok(Some(w.clone()));
        }
        Ok(wins.into_iter().next())
    }

    pub fn focus_window(&self, window_id: u32) -> Result<WindowInfo, RpcError> {
        let root = self.root();
        let data = ClientMessageData::from([1u32, CURRENT_TIME, 0, 0, 0]);
        let event = ClientMessageEvent {
            response_type: xproto::CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: window_id,
            type_: self.atoms.net_active_window,
            data,
        };
        let _ = self.conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        );
        let _ = self.conn.configure_window(
            window_id,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        );
        let _ = self
            .conn
            .set_input_focus(InputFocus::PARENT, window_id, CURRENT_TIME);
        self.conn.flush().map_err(x11_err)?;

        let wins = self.windows(None)?;
        wins.into_iter().find(|w| w.id == window_id).ok_or_else(|| {
            RpcError::with_next(
                ErrorCode::NotFound,
                format!("window w{window_id} not found on X11 display"),
                "Call list_windows to inspect currently open windows.",
            )
        })
    }

    pub fn screen_state(&self) -> ScreenState {
        let root = self.root();
        if let Ok(cookie) = screensaver::query_info(&self.conn, root)
            && let Ok(reply) = cookie.reply()
        {
            // ScreenSaver state 1 = On (blanked/locked); 0 = Off; 3 = Disabled.
            if reply.state == 1 {
                return ScreenState::DisplayAsleep;
            }
        }
        ScreenState::Unlocked
    }

    pub fn wake_display(&self) -> Result<bool, RpcError> {
        let _ = self.conn.force_screen_saver(xproto::ScreenSaver::RESET);
        self.conn.flush().map_err(x11_err)?;
        Ok(true)
    }

    /// Capture a display, background/foreground window, app, or region in memory via X11 ZPixmap.
    pub fn capture(
        &self,
        target: &ScreenshotTarget,
        format: ImageFormat,
        annotate: &[AnnotateBox],
        focus_mode: FocusMode,
    ) -> Result<RawScreenshot, RpcError> {
        let root = self.root();
        let displays = self.displays()?;

        match target {
            ScreenshotTarget::Display { index } => {
                let disp = match index {
                    Some(i) => displays.get(*i as usize).ok_or_else(|| {
                        RpcError::new(
                            ErrorCode::NotFound,
                            format!("display {i} not found ({} active)", displays.len()),
                        )
                    })?,
                    None => displays
                        .iter()
                        .find(|d| d.main)
                        .or_else(|| displays.first())
                        .ok_or_else(|| RpcError::new(ErrorCode::NotFound, "no active displays"))?,
                };
                let x = disp.bounds.x.round() as i16;
                let y = disp.bounds.y.round() as i16;
                let w = disp.pixel_width.clamp(1, u32::from(u16::MAX)) as u16;
                let h = disp.pixel_height.clamp(1, u32::from(u16::MAX)) as u16;
                let rgba = self.grab_drawable_rgba(root, x, y, w, h)?;
                encode_rgba_screenshot(
                    rgba,
                    u32::from(w),
                    u32::from(h),
                    disp.bounds,
                    format,
                    annotate,
                )
            }
            ScreenshotTarget::Region {
                x,
                y,
                width,
                height,
                ..
            } => {
                let screen = self.screen();
                let req = Rect {
                    x: *x,
                    y: *y,
                    width: *width,
                    height: *height,
                };
                let (rx, ry, rw, rh, clipped) =
                    clip_rect_to_screen(req, screen.width_in_pixels, screen.height_in_pixels)
                        .ok_or_else(|| {
                            RpcError::with_next(
                                ErrorCode::InvalidInput,
                                "requested capture region lies completely outside the X11 screen",
                                "Pass a region overlapping the active display bounds.",
                            )
                        })?;
                let rgba = self.grab_drawable_rgba(root, rx, ry, rw, rh)?;
                encode_rgba_screenshot(
                    rgba,
                    u32::from(rw),
                    u32::from(rh),
                    clipped,
                    format,
                    annotate,
                )
            }
            ScreenshotTarget::Window { window_id } => {
                self.capture_window(*window_id, format, annotate, focus_mode)
            }
            ScreenshotTarget::App { app } => {
                let wins = self.windows(Some(app))?;
                let first = wins.first().ok_or_else(|| {
                    RpcError::with_next(
                        ErrorCode::NotFound,
                        format!("no visible window found for app {app:?}"),
                        "Open the app or call list_windows to inspect available windows.",
                    )
                })?;
                self.capture_window(first.id, format, annotate, focus_mode)
            }
        }
    }

    fn capture_window(
        &self,
        window_id: u32,
        format: ImageFormat,
        annotate: &[AnnotateBox],
        focus_mode: FocusMode,
    ) -> Result<RawScreenshot, RpcError> {
        if focus_mode == FocusMode::Foreground {
            let _ = self.focus_window(window_id);
            std::thread::sleep(Duration::from_millis(40));
        }

        let bounds = self.window_screen_bounds(window_id).ok_or_else(|| {
            RpcError::with_next(
                ErrorCode::NotFound,
                format!("window w{window_id} not found or has empty geometry"),
                "Call list_windows to check active window IDs.",
            )
        })?;
        let w = bounds.width.round().clamp(1.0, f64::from(u16::MAX)) as u16;
        let h = bounds.height.round().clamp(1.0, f64::from(u16::MAX)) as u16;

        // Try direct window drawable capture first (works on occluded/background windows when
        // Composite redirection is active!). If X11 returns BadMatch (e.g. partially offscreen
        // or unredirected window), fall back cleanly to the clipped intersection on root.
        let _ = composite::query_version(&self.conn, 0, 4);
        let (rgba, final_w, final_h, final_bounds) = match self
            .grab_drawable_rgba(window_id, 0, 0, w, h)
        {
            Ok(px) => (px, u32::from(w), u32::from(h), bounds),
            Err(_) => {
                let root = self.root();
                let screen = self.screen();
                let (cx, cy, cw, ch, clipped) =
                    clip_rect_to_screen(bounds, screen.width_in_pixels, screen.height_in_pixels)
                        .ok_or_else(|| {
                            RpcError::with_next(
                                ErrorCode::NotFound,
                                format!(
                                    "window w{window_id} lies completely outside the X11 screen"
                                ),
                                "Move or focus the window onto a visible display before capturing.",
                            )
                        })?;
                let px = self.grab_drawable_rgba(root, cx, cy, cw, ch)?;
                (px, u32::from(cw), u32::from(ch), clipped)
            }
        };

        encode_rgba_screenshot(rgba, final_w, final_h, final_bounds, format, annotate)
    }

    fn grab_drawable_rgba(
        &self,
        drawable: Window,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> Result<Vec<u8>, RpcError> {
        let reply = self
            .conn
            .get_image(X11ImageFormat::Z_PIXMAP, drawable, x, y, width, height, !0)
            .map_err(x11_err)?
            .reply()
            .map_err(x11_reply_err)?;

        let bpp = self.bits_per_pixel_for_depth(reply.depth);
        let (rm, gm, bm) = self.visual_masks(reply.visual);
        zpixmap_to_rgba(
            &reply.data,
            ZPixmapFormat {
                width: u32::from(width),
                height: u32::from(height),
                bits_per_pixel: bpp,
                red_mask: rm,
                green_mask: gm,
                blue_mask: bm,
                msb_first: self.msb_first(),
            },
        )
    }

    // ---- XTest synthetic input ----

    pub fn hover_point(
        &self,
        stop_flag: &AtomicBool,
        point: Point,
    ) -> Result<StepExecInfo, RpcError> {
        check_stopped(stop_flag)?;
        self.warp_pointer(point)?;
        Ok(StepExecInfo {
            detail: format!("xtest hovered ({:.0},{:.0})", point.x, point.y),
            used_background: false,
        })
    }

    pub fn click_point(
        &self,
        stop_flag: &AtomicBool,
        point: Point,
        button: MouseButton,
        count: u8,
        mods: Modifiers,
    ) -> Result<StepExecInfo, RpcError> {
        check_stopped(stop_flag)?;
        let btn = x11_mouse_button(button);
        let mod_codes = self.resolve_modifier_keycodes(mods)?;
        self.warp_pointer(point)?;
        self.press_keycodes(&mod_codes, true)?;
        let res = (|| -> Result<(), RpcError> {
            for i in 0..count.max(1) {
                check_stopped(stop_flag)?;
                self.fake_button(btn, true)?;
                std::thread::sleep(Duration::from_millis(12));
                self.fake_button(btn, false)?;
                if i + 1 < count {
                    std::thread::sleep(Duration::from_millis(45));
                }
            }
            Ok(())
        })();
        let _ = self.press_keycodes_rev(&mod_codes, false);
        res?;
        Ok(StepExecInfo {
            detail: format!(
                "xtest clicked ({:.0},{:.0}) {:?} x{}",
                point.x,
                point.y,
                button,
                count.max(1)
            ),
            used_background: false,
        })
    }

    pub fn drag(
        &self,
        stop_flag: &AtomicBool,
        from: Point,
        to: Point,
        mods: Modifiers,
    ) -> Result<StepExecInfo, RpcError> {
        check_stopped(stop_flag)?;
        let mod_codes = self.resolve_modifier_keycodes(mods)?;
        self.warp_pointer(from)?;
        self.press_keycodes(&mod_codes, true)?;
        let mut button_down = false;
        let res = (|| -> Result<(), RpcError> {
            self.fake_button(1, true)?;
            button_down = true;
            let steps = 12usize;
            for i in 1..=steps {
                check_stopped(stop_flag)?;
                let t = i as f64 / steps as f64;
                let p = Point {
                    x: from.x + (to.x - from.x) * t,
                    y: from.y + (to.y - from.y) * t,
                };
                self.warp_pointer(p)?;
                std::thread::sleep(Duration::from_millis(8));
            }
            Ok(())
        })();
        if button_down {
            let _ = self.fake_button(1, false);
        }
        let _ = self.press_keycodes_rev(&mod_codes, false);
        res?;
        Ok(StepExecInfo {
            detail: format!(
                "xtest dragged ({:.0},{:.0}) -> ({:.0},{:.0})",
                from.x, from.y, to.x, to.y
            ),
            used_background: false,
        })
    }

    pub fn scroll(
        &self,
        stop_flag: &AtomicBool,
        point: Point,
        direction: ScrollDirection,
        amount: u32,
    ) -> Result<StepExecInfo, RpcError> {
        check_stopped(stop_flag)?;
        self.warp_pointer(point)?;
        let btn = match direction {
            ScrollDirection::Up => 4,
            ScrollDirection::Down => 5,
            ScrollDirection::Left => 6,
            ScrollDirection::Right => 7,
        };
        let clicks = amount.clamp(1, 50);
        for _ in 0..clicks {
            check_stopped(stop_flag)?;
            self.fake_button(btn, true)?;
            self.fake_button(btn, false)?;
            std::thread::sleep(Duration::from_millis(6));
        }
        Ok(StepExecInfo {
            detail: format!(
                "xtest scrolled {:?} x{} at ({:.0},{:.0})",
                direction, clicks, point.x, point.y
            ),
            used_background: false,
        })
    }

    pub fn press_key_combo(
        &self,
        stop_flag: &AtomicBool,
        combo: KeyCombo,
        repeat: u8,
    ) -> Result<StepExecInfo, RpcError> {
        check_stopped(stop_flag)?;
        let keymap = self.load_keymap()?;
        let (keycode, extra_shift) = keymap.lookup(combo.key.x11_keysym()).ok_or_else(|| {
            RpcError::new(
                ErrorCode::BackendError,
                format!("X11 keymap has no keycode for {:?}", combo.key),
            )
        })?;
        let mut mods = combo.modifiers;
        if extra_shift {
            mods.shift = true;
        }
        let mod_codes = keymap.modifier_keycodes(mods);
        self.press_keycodes(&mod_codes, true)?;
        let res = (|| -> Result<(), RpcError> {
            for i in 0..repeat.max(1) {
                check_stopped(stop_flag)?;
                self.fake_key(keycode, true)?;
                std::thread::sleep(Duration::from_millis(8));
                self.fake_key(keycode, false)?;
                if i + 1 < repeat {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            Ok(())
        })();
        let _ = self.press_keycodes_rev(&mod_codes, false);
        res?;
        Ok(StepExecInfo {
            detail: format!("xtest pressed {:?} x{}", combo.key, repeat.max(1)),
            used_background: false,
        })
    }

    /// Type ASCII/mapped characters directly via XTest key events, always releasing Shift on error.
    pub fn type_text(&self, stop_flag: &AtomicBool, text: &str) -> Result<usize, RpcError> {
        let keymap = self.load_keymap()?;
        let shift_code = keymap.lookup(XK_SHIFT_L).map(|(kc, _)| kc);
        let mut typed = 0usize;
        for ch in text.chars() {
            check_stopped(stop_flag)?;
            let Some(keysym) = char_to_x11_keysym(ch) else {
                continue;
            };
            let Some((keycode, needs_shift)) = keymap.lookup(keysym) else {
                return Err(RpcError::with_next(
                    ErrorCode::BackendError,
                    format!("character {ch:?} is not mapped in the current X11 keymap"),
                    "Install xclip or xsel so agent-desktop can paste Unicode text via clipboard.",
                ));
            };
            let shift_pressed = if needs_shift && let Some(sc) = shift_code {
                self.fake_key(sc, true)?;
                Some(sc)
            } else {
                None
            };
            let step_res = (|| -> Result<(), RpcError> {
                self.fake_key(keycode, true)?;
                self.fake_key(keycode, false)?;
                Ok(())
            })();
            if let Some(sc) = shift_pressed {
                let _ = self.fake_key(sc, false);
            }
            step_res?;
            typed += 1;
            std::thread::sleep(Duration::from_millis(4));
        }
        Ok(typed)
    }

    /// Save clipboard, write `text` via `xclip`/`xsel`, send `Ctrl+V`, and restore previous clipboard.
    pub fn paste_text_via_clipboard(
        &self,
        stop_flag: &AtomicBool,
        display: &str,
        xauth_path: Option<&Path>,
        text: &str,
    ) -> Result<(), RpcError> {
        check_stopped(stop_flag)?;
        let prev = read_x11_clipboard(display, xauth_path).ok();
        write_x11_clipboard(display, xauth_path, text)?;
        std::thread::sleep(Duration::from_millis(20));
        let paste_combo = KeyCombo {
            key: desktop_proto::KeyCode::V,
            modifiers: Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
        };
        let res = self.press_key_combo(stop_flag, paste_combo, 1);
        std::thread::sleep(Duration::from_millis(45));
        if let Some(old) = prev {
            let _ = write_x11_clipboard(display, xauth_path, &old);
        }
        res.map(|_| ())
    }

    // ---- Internal helpers ----

    fn warp_pointer(&self, point: Point) -> Result<(), RpcError> {
        let root = self.root();
        let x = point
            .x
            .round()
            .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
        let y = point
            .y
            .round()
            .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
        xtest::fake_input(
            &self.conn,
            XTEST_MOTION_NOTIFY,
            0,
            CURRENT_TIME,
            root,
            x,
            y,
            0,
        )
        .map_err(x11_err)?;
        self.conn.flush().map_err(x11_err)?;
        Ok(())
    }

    fn fake_button(&self, button: u8, press: bool) -> Result<(), RpcError> {
        let ty = if press {
            XTEST_BUTTON_PRESS
        } else {
            XTEST_BUTTON_RELEASE
        };
        xtest::fake_input(&self.conn, ty, button, CURRENT_TIME, x11rb::NONE, 0, 0, 0)
            .map_err(x11_err)?;
        self.conn.flush().map_err(x11_err)?;
        Ok(())
    }

    fn fake_key(&self, keycode: u8, press: bool) -> Result<(), RpcError> {
        let ty = if press {
            XTEST_KEY_PRESS
        } else {
            XTEST_KEY_RELEASE
        };
        xtest::fake_input(&self.conn, ty, keycode, CURRENT_TIME, x11rb::NONE, 0, 0, 0)
            .map_err(x11_err)?;
        self.conn.flush().map_err(x11_err)?;
        Ok(())
    }

    fn press_keycodes(&self, codes: &[u8], press: bool) -> Result<(), RpcError> {
        for &kc in codes {
            self.fake_key(kc, press)?;
        }
        Ok(())
    }

    fn press_keycodes_rev(&self, codes: &[u8], press: bool) -> Result<(), RpcError> {
        for &kc in codes.iter().rev() {
            self.fake_key(kc, press)?;
        }
        Ok(())
    }

    fn resolve_modifier_keycodes(&self, mods: Modifiers) -> Result<Vec<u8>, RpcError> {
        if mods.is_empty() {
            return Ok(Vec::new());
        }
        let km = self.load_keymap()?;
        Ok(km.modifier_keycodes(mods))
    }

    fn load_keymap(&self) -> Result<X11Keymap, RpcError> {
        let min_kc = self.conn.setup().min_keycode;
        let max_kc = self.conn.setup().max_keycode;
        let count = max_kc.saturating_sub(min_kc).saturating_add(1);
        let reply = self
            .conn
            .get_keyboard_mapping(min_kc, count)
            .map_err(x11_err)?
            .reply()
            .map_err(x11_reply_err)?;
        Ok(X11Keymap::from_mapping(
            min_kc,
            reply.keysyms_per_keycode,
            &reply.keysyms,
        ))
    }

    fn active_window_id(&self) -> Option<Window> {
        if let Some(wid) = self.read_u32_prop(self.root(), self.atoms.net_active_window)
            && wid != 0
        {
            return Some(wid);
        }
        self.conn
            .get_input_focus()
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.focus)
            .filter(|&w| w != 0 && w != 1 && w != self.root())
    }

    fn read_window_list_prop(&self, win: Window, prop: Atom) -> Vec<Window> {
        let Ok(cookie) = self
            .conn
            .get_property(false, win, prop, AtomEnum::WINDOW, 0, 4096)
        else {
            return Vec::new();
        };
        let Ok(reply) = cookie.reply() else {
            return Vec::new();
        };
        reply
            .value32()
            .map(|iter| iter.collect())
            .unwrap_or_default()
    }

    fn read_u32_prop(&self, win: Window, prop: Atom) -> Option<u32> {
        let reply = self
            .conn
            .get_property(false, win, prop, AtomEnum::ANY, 0, 4)
            .ok()?
            .reply()
            .ok()?;
        reply.value32()?.next()
    }

    fn read_atoms_prop(&self, win: Window, prop: Atom) -> Vec<Atom> {
        let Ok(cookie) = self
            .conn
            .get_property(false, win, prop, AtomEnum::ATOM, 0, 64)
        else {
            return Vec::new();
        };
        let Ok(reply) = cookie.reply() else {
            return Vec::new();
        };
        reply
            .value32()
            .map(|iter| iter.collect())
            .unwrap_or_default()
    }

    fn is_ignored_window_type(&self, win: Window) -> bool {
        let types = self.read_atoms_prop(win, self.atoms.net_wm_window_type);
        types.iter().any(|&a| {
            a == self.atoms.net_wm_window_type_desktop || a == self.atoms.net_wm_window_type_dock
        })
    }

    fn is_hidden_state(&self, win: Window) -> bool {
        let states = self.read_atoms_prop(win, self.atoms.net_wm_state);
        states.contains(&self.atoms.net_wm_state_hidden)
    }

    fn read_window_title(&self, win: Window) -> Option<String> {
        if let Ok(reply) = self
            .conn
            .get_property(
                false,
                win,
                self.atoms.net_wm_name,
                self.atoms.utf8_string,
                0,
                1024,
            )
            .map(|c| c.reply())
            && let Ok(reply) = reply
            && !reply.value.is_empty()
            && let Ok(s) = String::from_utf8(reply.value)
        {
            return Some(s);
        }
        let reply = self
            .conn
            .get_property(false, win, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 1024)
            .ok()?
            .reply()
            .ok()?;
        if reply.value.is_empty() {
            return None;
        }
        Some(String::from_utf8_lossy(&reply.value).into_owned())
    }

    fn read_wm_class(&self, win: Window) -> Option<(String, String)> {
        let reply = self
            .conn
            .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            .ok()?
            .reply()
            .ok()?;
        if reply.value.is_empty() {
            return None;
        }
        let mut parts = reply.value.split(|&b| b == 0);
        let inst = parts
            .next()
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .unwrap_or_default();
        let cls = parts
            .next()
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .unwrap_or_default();
        if inst.is_empty() && cls.is_empty() {
            None
        } else {
            Some((inst, cls))
        }
    }

    fn window_screen_bounds(&self, win: Window) -> Option<Rect> {
        let geom = self.conn.get_geometry(win).ok()?.reply().ok()?;
        let trans = self
            .conn
            .translate_coordinates(win, self.root(), 0, 0)
            .ok()?
            .reply()
            .ok()?;
        Some(Rect {
            x: f64::from(trans.dst_x),
            y: f64::from(trans.dst_y),
            width: f64::from(geom.width),
            height: f64::from(geom.height),
        })
    }
}

/// Connect to an X11 display using an explicit `MIT-MAGIC-COOKIE-1` without mutating process env.
fn connect_with_cookie(display: &str, cookie: &[u8]) -> Result<(RustConnection, usize), RpcError> {
    use std::os::unix::net::UnixStream;

    let trimmed = display.trim().trim_start_matches(':');
    let (dpy_str, screen_str) = trimmed
        .split_once('.')
        .map_or((trimmed, "0"), |(d, s)| (d, s));
    let dpy_num: u32 = dpy_str.parse().map_err(|_| {
        RpcError::new(
            ErrorCode::BackendError,
            format!("invalid X11 display {display:?}"),
        )
    })?;
    let screen: usize = screen_str.parse().unwrap_or(0);
    let sock_path = format!("/tmp/.X11-unix/X{dpy_num}");
    let unix = UnixStream::connect(&sock_path).map_err(|err| {
        RpcError::new(
            ErrorCode::BackendError,
            format!("failed to connect to X11 Unix socket {sock_path}: {err}"),
        )
    })?;
    let (stream, _) = DefaultStream::from_unix_stream(unix).map_err(|err| {
        RpcError::new(
            ErrorCode::BackendError,
            format!("failed to wrap X11 Unix stream {sock_path}: {err}"),
        )
    })?;
    let conn = RustConnection::connect_to_stream_with_auth_info(
        stream,
        screen,
        b"MIT-MAGIC-COOKIE-1".to_vec(),
        cookie.to_vec(),
    )
    .map_err(|err| {
        RpcError::new(
            ErrorCode::BackendError,
            format!("X11 auth handshake failed on {display:?}: {err}"),
        )
    })?;
    Ok((conn, screen))
}

/// Intersect a requested rectangle with the root X11 screen `[0, screen_w) x [0, screen_h)`.
#[must_use]
pub fn clip_rect_to_screen(
    rect: Rect,
    screen_w: u16,
    screen_h: u16,
) -> Option<(i16, i16, u16, u16, Rect)> {
    let sw = i32::from(screen_w);
    let sh = i32::from(screen_h);
    let x0 = (rect.x.round() as i32).clamp(0, sw);
    let y0 = (rect.y.round() as i32).clamp(0, sh);
    let x1 = ((rect.x + rect.width).round() as i32).clamp(0, sw);
    let y1 = ((rect.y + rect.height).round() as i32).clamp(0, sh);
    let w = x1 - x0;
    let h = y1 - y0;
    if w <= 0 || h <= 0 {
        return None;
    }
    let clipped = Rect {
        x: f64::from(x0),
        y: f64::from(y0),
        width: f64::from(w),
        height: f64::from(h),
    };
    Some((x0 as i16, y0 as i16, w as u16, h as u16, clipped))
}

/// Read X11 clipboard text using `xclip` or `xsel`.
pub fn read_x11_clipboard(display: &str, xauth_path: Option<&Path>) -> Result<String, RpcError> {
    for (bin, args) in [
        ("xclip", &["-o", "-selection", "clipboard"][..]),
        ("xsel", &["-ob"][..]),
    ] {
        let mut cmd = Command::new(bin);
        cmd.args(args).env("DISPLAY", display);
        if let Some(auth) = xauth_path {
            cmd.env("XAUTHORITY", auth);
        }
        if let Ok(out) = cmd.output()
            && out.status.success()
        {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
    }
    Err(RpcError::with_next(
        ErrorCode::BackendError,
        "unable to read X11 clipboard (neither xclip nor xsel succeeded)",
        "Install xclip (`sudo apt install xclip`) or xsel.",
    ))
}

/// Write X11 clipboard text using `xclip` or `xsel`.
pub fn write_x11_clipboard(
    display: &str,
    xauth_path: Option<&Path>,
    text: &str,
) -> Result<(), RpcError> {
    use std::io::Write as _;

    for (bin, args) in [
        ("xclip", &["-i", "-selection", "clipboard"][..]),
        ("xsel", &["-ib"][..]),
    ] {
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .env("DISPLAY", display)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(auth) = xauth_path {
            cmd.env("XAUTHORITY", auth);
        }
        if let Ok(mut child) = cmd.spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if let Ok(status) = child.wait()
                && status.success()
            {
                return Ok(());
            }
        }
    }
    Err(RpcError::with_next(
        ErrorCode::BackendError,
        "unable to write X11 clipboard (neither xclip nor xsel succeeded)",
        "Install xclip (`sudo apt install xclip`) or xsel.",
    ))
}

struct X11Keymap {
    map: HashMap<u32, (u8, bool)>,
}

impl X11Keymap {
    /// Two-pass X11 keymap index: unshifted column-0 always takes precedence over shifted column-1.
    fn from_mapping(min_kc: u8, per_keycode: u8, keysyms: &[u32]) -> Self {
        let mut map = HashMap::new();
        let stride = usize::from(per_keycode.max(1));

        // Pass 1: column 0 (unshifted).
        for (idx, chunk) in keysyms.chunks(stride).enumerate() {
            let Some(kc) = min_kc.checked_add(idx as u8) else {
                break;
            };
            if let Some(&sym0) = chunk.first()
                && sym0 != 0
            {
                map.entry(sym0).or_insert((kc, false));
            }
        }

        // Pass 2: column 1 (shifted) only when not already mapped unshifted.
        for (idx, chunk) in keysyms.chunks(stride).enumerate() {
            let Some(kc) = min_kc.checked_add(idx as u8) else {
                break;
            };
            if let Some(&sym1) = chunk.get(1)
                && sym1 != 0
            {
                map.entry(sym1).or_insert((kc, true));
            }
        }
        Self { map }
    }

    fn lookup(&self, keysym: u32) -> Option<(u8, bool)> {
        if let Some(found) = self.map.get(&keysym).copied() {
            return Some(found);
        }
        // Standard X11 core protocol rule: if a keycode only lists lowercase `[XK_a, 0]`,
        // Shift + that keycode produces uppercase `XK_A`.
        if (u32::from(b'A')..=u32::from(b'Z')).contains(&keysym) {
            let lower = keysym + 32;
            if let Some((kc, _)) = self.map.get(&lower).copied() {
                return Some((kc, true));
            }
        }
        None
    }

    fn modifier_keycodes(&self, mods: Modifiers) -> Vec<u8> {
        let mut codes = Vec::new();
        if mods.ctrl
            && let Some((kc, _)) = self.lookup(XK_CONTROL_L)
        {
            codes.push(kc);
        }
        if mods.alt
            && let Some((kc, _)) = self.lookup(XK_ALT_L)
        {
            codes.push(kc);
        }
        if mods.cmd
            && let Some((kc, _)) = self.lookup(XK_SUPER_L)
        {
            codes.push(kc);
        }
        if mods.shift
            && let Some((kc, _)) = self.lookup(XK_SHIFT_L)
        {
            codes.push(kc);
        }
        codes
    }
}

fn x11_mouse_button(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
    }
}

fn check_stopped(stop_flag: &AtomicBool) -> Result<(), RpcError> {
    if stop_flag.load(Ordering::SeqCst) {
        return Err(RpcError::with_next(
            ErrorCode::StoppedByUser,
            "action interrupted by stop signal",
            "Start a new session when ready to continue.",
        ));
    }
    Ok(())
}

fn x11_err(err: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        ErrorCode::BackendError,
        format!("X11 connection error: {err}"),
    )
}

fn x11_reply_err(err: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        ErrorCode::BackendError,
        format!("X11 protocol error: {err}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_prefers_unshifted_and_handles_implicit_uppercase() {
        // Keycode 38 -> ['a', 0] (minimal Xvfb style without explicit 'A' in col 1)
        // Keycode 39 -> ['1', '!']
        // Keycode 40 -> ['!', '@'] (unshifted '!' on later keycode should win over shifted col 1)
        let km = X11Keymap::from_mapping(
            38,
            2,
            &[
                u32::from(b'a'),
                0,
                u32::from(b'1'),
                u32::from(b'!'),
                u32::from(b'!'),
                u32::from(b'@'),
                XK_SHIFT_L,
                XK_SHIFT_L,
            ],
        );
        assert_eq!(km.lookup(u32::from(b'a')), Some((38, false)));
        assert_eq!(km.lookup(u32::from(b'A')), Some((38, true)));
        assert_eq!(km.lookup(u32::from(b'!')), Some((40, false)));
        assert_eq!(km.lookup(u32::from(b'@')), Some((40, true)));
        assert_eq!(
            km.modifier_keycodes(Modifiers {
                shift: true,
                ..Modifiers::default()
            }),
            vec![41]
        );
    }

    #[test]
    fn clips_partially_offscreen_and_negative_origin_windows_accurately() {
        // Partially below bottom edge: 800x600 at y=500 on 1280x800 -> 800x300 at (0,500)
        let (x, y, w, h, bounds) = clip_rect_to_screen(
            Rect {
                x: 0.0,
                y: 500.0,
                width: 800.0,
                height: 600.0,
            },
            1280,
            800,
        )
        .unwrap();
        assert_eq!((x, y, w, h), (0, 500, 800, 300));
        assert_eq!(bounds.height, 300.0);

        // Negative origin: 400x300 at (-100, -50) -> 300x250 at (0, 0)
        let (x, y, w, h, bounds) = clip_rect_to_screen(
            Rect {
                x: -100.0,
                y: -50.0,
                width: 400.0,
                height: 300.0,
            },
            1280,
            800,
        )
        .unwrap();
        assert_eq!((x, y, w, h), (0, 0, 300, 250));
        assert_eq!(bounds.x, 0.0);
        assert_eq!(bounds.width, 300.0);

        // Completely offscreen -> None
        assert!(
            clip_rect_to_screen(
                Rect {
                    x: 1500.0,
                    y: 0.0,
                    width: 200.0,
                    height: 200.0,
                },
                1280,
                800,
            )
            .is_none()
        );
    }
}
