use std::collections::BTreeMap;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::rpc::{ErrorCode, RpcError};
use crate::types::HostStatus;

pub const DEFAULT_REMOTE_PORT: u16 = 9487;
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 750;

/// Fleet configuration loaded from `~/.config/agent-desktop/fleet.toml` (private, `0600`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetConfig {
    /// Optional Tailscale/loopback remote HTTP RPC listener for `desktopd`.
    #[serde(default)]
    pub listen: Option<ListenConfig>,
    /// Named remote machines reachable over Tailscale (e.g. `mac-a`, `mac-b`, `linux-box`).
    #[serde(default)]
    pub hosts: BTreeMap<String, PeerHostConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenConfig {
    /// `"tailscale"` (auto-detect local Tailscale IPv4) or an explicit IP address.
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Inline bearer token (prefer `token_file` with `0600` permissions).
    #[serde(default)]
    pub token: Option<String>,
    /// Path to a `0600` file containing the bearer token.
    #[serde(default)]
    pub token_file: Option<String>,
    /// Verify remote Tailscale peers belong to our own tailnet via `tailscale whois`.
    #[serde(default = "default_true")]
    pub require_tailscale: bool,
    /// Refuse binding non-Tailscale / non-loopback addresses unless explicitly set true.
    #[serde(default)]
    pub allow_non_tailscale: bool,
}

impl Default for ListenConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            port: DEFAULT_REMOTE_PORT,
            token: None,
            token_file: None,
            require_tailscale: true,
            allow_non_tailscale: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerHostConfig {
    /// Target `host:port` (or bare IP/MagicDNS name, defaulting to port 9487).
    pub addr: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_file: Option<String>,
    #[serde(default)]
    pub connect_timeout_ms: Option<u64>,
}

impl PeerHostConfig {
    /// Normalized `http://host:port` base URL.
    #[must_use]
    pub fn base_url(&self) -> String {
        let trimmed = self.addr.trim().trim_end_matches('/');
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            return trimmed.to_string();
        }
        let has_port = (trimmed.contains(':') && !trimmed.starts_with('['))
            || (trimmed.starts_with('[') && trimmed.contains("]:"));
        if has_port {
            format!("http://{trimmed}")
        } else {
            format!("http://{trimmed}:{DEFAULT_REMOTE_PORT}")
        }
    }
}

/// Live reachability and status summary for one host returned by `list_hosts` / `desktop hosts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEntryStatus {
    pub alias: String,
    pub addr: String,
    pub reachable: bool,
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<HostStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl FleetConfig {
    pub fn load_from(path: &Path) -> Result<Self, RpcError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(RpcError::with_next(
                    ErrorCode::BackendError,
                    format!("could not read fleet config {}: {err}", path.display()),
                    "Check permissions on fleet.toml or run `desktop fleet init`.",
                ));
            }
        };
        toml::from_str(&text).map_err(|err| {
            RpcError::with_next(
                ErrorCode::InvalidInput,
                format!("invalid TOML in {}: {err}", path.display()),
                "Fix fleet.toml syntax.",
            )
        })
    }
}

/// Expand leading `~/` relative to `home`.
#[must_use]
pub fn expand_tilde(raw: &str, home: Option<&Path>) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(h) = home
    {
        return h.join(rest);
    }
    PathBuf::from(raw)
}

/// Load a non-empty bearer token from inline value or `token_file`.
pub fn resolve_token(
    inline: Option<&str>,
    token_file: Option<&str>,
    home: Option<&Path>,
) -> Result<String, RpcError> {
    if let Some(tok) = inline.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(tok.to_string());
    }
    if let Some(tf) = token_file.map(str::trim).filter(|s| !s.is_empty()) {
        let path = expand_tilde(tf, home);
        let raw = fs::read_to_string(&path).map_err(|err| {
            RpcError::with_next(
                ErrorCode::BackendError,
                format!("could not read token file {}: {err}", path.display()),
                "Run `desktop fleet init` or create the 0600 token file.",
            )
        })?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(RpcError::new(
                ErrorCode::InvalidInput,
                format!("token file {} is empty", path.display()),
            ));
        }
        return Ok(trimmed.to_string());
    }
    Err(RpcError::with_next(
        ErrorCode::InvalidInput,
        "remote fleet listener/peer requires `token` or `token_file`",
        "Run `desktop fleet init` to generate a 0600 bearer token.",
    ))
}

/// Returns true for Tailscale CGNAT IPv4 (`100.64.0.0/10`) or Tailscale ULA IPv6 (`fd7a:115c:a1e0::/48`).
#[must_use]
pub fn is_tailscale_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0b1100_0000) == 64
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

/// Refuse binding `desktopd` to `0.0.0.0`, `::`, LAN, or public IPs unless explicitly opted in.
pub fn validate_bind_ip(ip: IpAddr, allow_non_tailscale: bool) -> Result<(), RpcError> {
    if is_tailscale_ip(ip) || ip.is_loopback() {
        return Ok(());
    }
    if allow_non_tailscale && !ip.is_unspecified() {
        return Ok(());
    }
    Err(RpcError::with_next(
        ErrorCode::PolicyDenied,
        format!("refusing to bind desktopd remote listener to non-Tailscale address {ip}"),
        "Set bind = \"tailscale\" (recommended) or a 100.64.0.0/10 Tailscale IP in fleet.toml.",
    ))
}

fn default_bind() -> String {
    "tailscale".into()
}
fn default_port() -> u16 {
    DEFAULT_REMOTE_PORT
}
fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    #[test]
    fn validates_tailscale_cidr_and_rejects_public_or_wildcard_binds() {
        assert!(is_tailscale_ip(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_tailscale_ip(IpAddr::V4(Ipv4Addr::new(100, 99, 10, 42))));
        assert!(is_tailscale_ip(IpAddr::V4(Ipv4Addr::new(
            100, 127, 255, 254
        ))));
        assert!(!is_tailscale_ip(IpAddr::V4(Ipv4Addr::new(
            100, 63, 255, 255
        ))));
        assert!(!is_tailscale_ip(IpAddr::V4(Ipv4Addr::new(100, 128, 0, 1))));
        assert!(!is_tailscale_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))));
        assert!(is_tailscale_ip(IpAddr::V6(Ipv6Addr::new(
            0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, 1
        ))));

        assert!(validate_bind_ip(IpAddr::V4(Ipv4Addr::LOCALHOST), false).is_ok());
        assert!(validate_bind_ip(IpAddr::V4(Ipv4Addr::new(100, 100, 1, 2)), false).is_ok());
        assert!(validate_bind_ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).is_err());
        assert!(validate_bind_ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED), true).is_err());
        assert!(validate_bind_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), false).is_err());
    }

    #[test]
    fn parses_fleet_toml_and_normalizes_peer_urls() {
        let cfg: FleetConfig = toml::from_str(
            r#"
[listen]
bind = "tailscale"
port = 9487
token_file = "~/.config/agent-desktop/token"

[hosts.pro]
addr = "100.64.0.10:9487"
token_file = "~/.config/agent-desktop/token"

[hosts.air]
addr = "100.64.0.11"
"#,
        )
        .unwrap();
        assert_eq!(cfg.listen.as_ref().unwrap().port, 9487);
        assert_eq!(cfg.hosts["pro"].base_url(), "http://100.64.0.10:9487");
        assert_eq!(cfg.hosts["air"].base_url(), "http://100.64.0.11:9487");
    }
}
