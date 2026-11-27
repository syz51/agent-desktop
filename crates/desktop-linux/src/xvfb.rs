use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use desktop_proto::{ErrorCode, RpcError};

use crate::session::command_exists;

/// Configuration for a daemon-managed private headless `Xvfb` display.
#[derive(Debug, Clone)]
pub struct XvfbConfig {
    pub width: u32,
    pub height: u32,
    pub depth: u8,
    /// Start a lightweight window manager (`openbox`/`fluxbox`) if installed.
    pub start_wm: bool,
    /// Preferred display number (e.g. `99` -> `:99`).
    pub display_hint: Option<u32>,
}

impl Default for XvfbConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            depth: 24,
            start_wm: true,
            display_hint: None,
        }
    }
}

/// Parse a `"WIDTHxHEIGHT"` display geometry string (e.g. `"1920x1080"` or `"1280x800"`).
pub fn parse_xvfb_size(spec: &str) -> Result<(u32, u32), RpcError> {
    let trimmed = spec.trim();
    let Some((w_str, h_str)) = trimmed.split_once(['x', 'X']) else {
        return Err(RpcError::with_next(
            ErrorCode::InvalidInput,
            format!("invalid xvfb size {spec:?}"),
            "Pass WIDTHxHEIGHT such as 1920x1080 or 1280x800.",
        ));
    };
    let w: u32 = w_str.parse().map_err(|_| {
        RpcError::with_next(
            ErrorCode::InvalidInput,
            format!("invalid xvfb width in {spec:?}"),
            "Pass WIDTHxHEIGHT such as 1920x1080.",
        )
    })?;
    let h: u32 = h_str.parse().map_err(|_| {
        RpcError::with_next(
            ErrorCode::InvalidInput,
            format!("invalid xvfb height in {spec:?}"),
            "Pass WIDTHxHEIGHT such as 1920x1080.",
        )
    })?;
    if !(320..=7680).contains(&w) || !(240..=4320).contains(&h) {
        return Err(RpcError::with_next(
            ErrorCode::InvalidInput,
            format!("xvfb size {w}x{h} out of supported range (320x240..7680x4320)"),
            "Use a standard desktop resolution such as 1920x1080 or 1280x800.",
        ));
    }
    Ok((w, h))
}

/// Find an unused X11 display number in `99..200` (or starting from `hint`).
#[must_use]
pub fn pick_free_display(hint: Option<u32>) -> u32 {
    let start = hint.unwrap_or(99);
    for n in start..start.saturating_add(100) {
        let sock = format!("/tmp/.X11-unix/X{n}");
        let lock = format!("/tmp/.X{n}-lock");
        if !Path::new(&sock).exists() && !Path::new(&lock).exists() {
            return n;
        }
    }
    start
}

/// Build command-line arguments for `Xvfb`.
#[must_use]
pub fn xvfb_args(display_num: u32, cfg: &XvfbConfig) -> Vec<String> {
    vec![
        format!(":{display_num}"),
        "-screen".to_string(),
        "0".to_string(),
        format!("{}x{}x{}", cfg.width, cfg.height, cfg.depth),
        "-nolisten".to_string(),
        "tcp".to_string(),
        "-ac".to_string(),
    ]
}

/// RAII handle owning a private `Xvfb` server (and optional lightweight WM).
pub struct ManagedXvfb {
    display: String,
    xvfb: Child,
    wm: Option<Child>,
}

impl ManagedXvfb {
    /// Launch a private `Xvfb` server and wait until its Unix socket is ready.
    pub fn start(cfg: &XvfbConfig) -> Result<Self, RpcError> {
        if !command_exists("Xvfb") {
            return Err(RpcError::with_next(
                ErrorCode::BackendError,
                "Xvfb executable not found on PATH",
                "Install Xvfb (e.g. `sudo apt install xvfb` on Debian/Ubuntu or `sudo dnf install xorg-x11-server-Xvfb` on Fedora).",
            ));
        }

        let display_num = pick_free_display(cfg.display_hint);
        let display_str = format!(":{display_num}");
        let args = xvfb_args(display_num, cfg);

        let mut xvfb = Command::new("Xvfb")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| {
                RpcError::with_next(
                    ErrorCode::BackendError,
                    format!("failed to spawn Xvfb {display_str}: {err}"),
                    "Ensure Xvfb is installed and /tmp/.X11-unix is writable.",
                )
            })?;

        let sock_path = format!("/tmp/.X11-unix/X{display_num}");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if Path::new(&sock_path).exists() {
                break;
            }
            if let Ok(Some(status)) = xvfb.try_wait() {
                return Err(RpcError::with_next(
                    ErrorCode::BackendError,
                    format!("Xvfb {display_str} exited immediately with status {status}"),
                    "Check if another display server owns the socket or try a different display.",
                ));
            }
            if Instant::now() >= deadline {
                let _ = xvfb.kill();
                let _ = xvfb.wait();
                return Err(RpcError::new(
                    ErrorCode::Timeout,
                    format!("timed out waiting for Xvfb socket {sock_path}"),
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        let wm = if cfg.start_wm {
            spawn_optional_wm(&display_str)
        } else {
            None
        };

        tracing::info!(
            x11_display = %display_str,
            width = cfg.width,
            height = cfg.height,
            wm = wm.is_some(),
            "started managed Xvfb session"
        );
        Ok(Self {
            display: display_str,
            xvfb,
            wm,
        })
    }

    #[must_use]
    pub fn display(&self) -> &str {
        &self.display
    }
}

impl Drop for ManagedXvfb {
    fn drop(&mut self) {
        if let Some(wm) = &mut self.wm {
            let _ = wm.kill();
            let _ = wm.wait();
        }
        let _ = self.xvfb.kill();
        let _ = self.xvfb.wait();
    }
}

fn spawn_optional_wm(display: &str) -> Option<Child> {
    for candidate in ["openbox", "fluxbox", "metacity"] {
        if command_exists(candidate) {
            match Command::new(candidate)
                .env("DISPLAY", display)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    // Brief settle so EWMH root atoms register before first window query.
                    std::thread::sleep(Duration::from_millis(80));
                    return Some(child);
                }
                Err(err) => {
                    tracing::warn!(wm = candidate, %err, "failed to launch optional WM in Xvfb");
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_and_rejects_invalid_xvfb_sizes() {
        assert_eq!(parse_xvfb_size("1920x1080").unwrap(), (1920, 1080));
        assert_eq!(parse_xvfb_size("1280X800").unwrap(), (1280, 800));
        assert!(parse_xvfb_size("1920").is_err());
        assert!(parse_xvfb_size("100x100").is_err());
        assert!(parse_xvfb_size("abcxdef").is_err());
    }

    #[test]
    fn builds_expected_xvfb_args() {
        let args = xvfb_args(
            105,
            &XvfbConfig {
                width: 1280,
                height: 800,
                depth: 24,
                start_wm: false,
                display_hint: Some(105),
            },
        );
        assert_eq!(
            args,
            vec![
                ":105",
                "-screen",
                "0",
                "1280x800x24",
                "-nolisten",
                "tcp",
                "-ac"
            ]
        );
    }
}
