use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use desktop_proto::{ErrorCode, RpcError};
use rand::RngExt as _;

use crate::session::command_exists;

const XAUTH_FAMILY_WILD: u16 = 0xffff;
const MIT_MAGIC_COOKIE_1: &[u8] = b"MIT-MAGIC-COOKIE-1";

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

/// Build an Xauthority (`MIT-MAGIC-COOKIE-1`, `FamilyWild`) record in wire format.
#[must_use]
pub fn encode_xauthority_record(display_num: u32, cookie: &[u8; 16]) -> Vec<u8> {
    let dpy_str = display_num.to_string();
    let mut out =
        Vec::with_capacity(2 + 2 + 2 + dpy_str.len() + 2 + MIT_MAGIC_COOKIE_1.len() + 2 + 16);
    out.extend_from_slice(&XAUTH_FAMILY_WILD.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // empty address for FamilyWild
    out.extend_from_slice(&(dpy_str.len() as u16).to_be_bytes());
    out.extend_from_slice(dpy_str.as_bytes());
    out.extend_from_slice(&(MIT_MAGIC_COOKIE_1.len() as u16).to_be_bytes());
    out.extend_from_slice(MIT_MAGIC_COOKIE_1);
    out.extend_from_slice(&(cookie.len() as u16).to_be_bytes());
    out.extend_from_slice(cookie);
    out
}

/// Build command-line arguments for `Xvfb` with mandatory `-auth` cookie protection (never `-ac`).
#[must_use]
pub fn xvfb_args(display_num: u32, cfg: &XvfbConfig, xauth_path: &Path) -> Vec<String> {
    vec![
        format!(":{display_num}"),
        "-screen".to_string(),
        "0".to_string(),
        format!("{}x{}x{}", cfg.width, cfg.height, cfg.depth),
        "-nolisten".to_string(),
        "tcp".to_string(),
        "-auth".to_string(),
        xauth_path.to_string_lossy().into_owned(),
    ]
}

fn write_private_xauthority(display_num: u32, cookie: &[u8; 16]) -> Result<PathBuf, RpcError> {
    use std::io::Write as _;

    let base_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let path = base_dir.join(format!(
        "agent-desktop-xvfb-{}-{}.xauth",
        std::process::id(),
        display_num
    ));
    let _ = std::fs::remove_file(&path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| {
            RpcError::new(
                ErrorCode::BackendError,
                format!(
                    "failed to create 0600 Xauthority file {}: {e}",
                    path.display()
                ),
            )
        })?;
    let record = encode_xauthority_record(display_num, cookie);
    file.write_all(&record).map_err(|e| {
        RpcError::new(
            ErrorCode::BackendError,
            format!("failed to write Xauthority file {}: {e}", path.display()),
        )
    })?;
    Ok(path)
}

/// RAII handle owning a private `Xvfb` server (with `0600` MIT-MAGIC-COOKIE-1 auth) and optional WM.
pub struct ManagedXvfb {
    display: String,
    cookie: [u8; 16],
    xauth_path: PathBuf,
    xvfb: Child,
    wm: Option<Child>,
}

impl ManagedXvfb {
    /// Launch a private authenticated `Xvfb` server and wait until its Unix socket is ready.
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
        let cookie: [u8; 16] = rand::rng().random();
        let xauth_path = write_private_xauthority(display_num, &cookie)?;
        let args = xvfb_args(display_num, cfg, &xauth_path);

        let mut xvfb = Command::new("Xvfb")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| {
                let _ = std::fs::remove_file(&xauth_path);
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
                let _ = std::fs::remove_file(&xauth_path);
                return Err(RpcError::with_next(
                    ErrorCode::BackendError,
                    format!("Xvfb {display_str} exited immediately with status {status}"),
                    "Check if another display server owns the socket or try a different display.",
                ));
            }
            if Instant::now() >= deadline {
                let _ = xvfb.kill();
                let _ = xvfb.wait();
                let _ = std::fs::remove_file(&xauth_path);
                return Err(RpcError::new(
                    ErrorCode::Timeout,
                    format!("timed out waiting for Xvfb socket {sock_path}"),
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        let wm = if cfg.start_wm {
            spawn_optional_wm(&display_str, &xauth_path)
        } else {
            None
        };

        tracing::info!(
            x11_display = %display_str,
            width = cfg.width,
            height = cfg.height,
            wm = wm.is_some(),
            "started managed authenticated Xvfb session"
        );
        Ok(Self {
            display: display_str,
            cookie,
            xauth_path,
            xvfb,
            wm,
        })
    }

    #[must_use]
    pub fn display(&self) -> &str {
        &self.display
    }

    #[must_use]
    pub fn cookie(&self) -> &[u8; 16] {
        &self.cookie
    }

    #[must_use]
    pub fn xauth_path(&self) -> &Path {
        &self.xauth_path
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
        let _ = std::fs::remove_file(&self.xauth_path);
    }
}

fn spawn_optional_wm(display: &str, xauth_path: &Path) -> Option<Child> {
    for candidate in ["openbox", "fluxbox", "metacity"] {
        if command_exists(candidate) {
            match Command::new(candidate)
                .env("DISPLAY", display)
                .env("XAUTHORITY", xauth_path)
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
    fn builds_authenticated_xvfb_args_and_xauth_record() {
        let auth = Path::new("/tmp/test.xauth");
        let args = xvfb_args(
            105,
            &XvfbConfig {
                width: 1280,
                height: 800,
                depth: 24,
                start_wm: false,
                display_hint: Some(105),
            },
            auth,
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
                "-auth",
                "/tmp/test.xauth"
            ]
        );
        assert!(!args.contains(&"-ac".to_string()));

        let cookie = [0xABu8; 16];
        let rec = encode_xauthority_record(105, &cookie);
        // FamilyWild (0xffff) + addr_len(0) + "105" + "MIT-MAGIC-COOKIE-1" + 16-byte cookie
        assert_eq!(&rec[0..4], &[0xff, 0xff, 0x00, 0x00]);
        assert_eq!(&rec[4..9], &[0x00, 0x03, b'1', b'0', b'5']);
        assert_eq!(&rec[rec.len() - 16..], &cookie);
    }
}
