use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use desktop_proto::is_tailscale_ip;

const WHOIS_CACHE_TTL: Duration = Duration::from_secs(300);

/// Locate the local `tailscale` CLI binary on macOS or Linux.
#[must_use]
pub fn find_tailscale_bin() -> Option<PathBuf> {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let cand = dir.join("tailscale");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    for candidate in [
        "/opt/homebrew/bin/tailscale",
        "/usr/local/bin/tailscale",
        "/usr/bin/tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ] {
        let p = Path::new(candidate);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    None
}

/// Detect this machine's Tailscale IPv4 address (`100.64.0.0/10`).
///
/// Scans OS network interfaces first (`ifconfig` on macOS, `ip -4 addr` on Linux) so detection
/// works reliably inside LaunchAgents and locked sessions without talking to Tailscale's GUI XPC;
/// falls back to `tailscale ip -4`.
pub fn detect_tailscale_ipv4() -> Result<IpAddr> {
    if let Some(ip) = detect_tailscale_ipv4_from_interfaces() {
        return Ok(ip);
    }

    let bin = find_tailscale_bin()
        .context("no 100.64.0.0/10 Tailscale interface found and tailscale CLI is unavailable")?;
    let out = Command::new(&bin)
        .args(["ip", "-4"])
        .output()
        .with_context(|| format!("run {} ip -4", bin.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("tailscale ip -4 failed: {}", err.trim());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let ip_str = stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .context("tailscale ip -4 returned no address")?;
    let ip: IpAddr = ip_str
        .parse()
        .with_context(|| format!("parse tailscale ip {ip_str:?}"))?;
    if !is_tailscale_ip(ip) {
        anyhow::bail!("tailscale ip -4 returned non-Tailscale address {ip}");
    }
    Ok(ip)
}

fn detect_tailscale_ipv4_from_interfaces() -> Option<IpAddr> {
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("/sbin/ifconfig").output().ok()?;
        if !out.status.success() {
            return None;
        }
        parse_ifconfig_tailscale_ipv4(&String::from_utf8_lossy(&out.stdout))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let out = Command::new("ip")
            .args(["-4", "-o", "addr", "show"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        parse_ip_addr_tailscale_ipv4(&String::from_utf8_lossy(&out.stdout))
    }
}

/// Extract the first `100.64.0.0/10` IPv4 address from BSD/macOS `ifconfig` output.
#[must_use]
pub fn parse_ifconfig_tailscale_ipv4(output: &str) -> Option<IpAddr> {
    for line in output.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("inet ")
            && let Some(token) = rest.split_whitespace().next()
            && let Ok(ip) = token.parse::<IpAddr>()
            && is_tailscale_ip(ip)
        {
            return Some(ip);
        }
    }
    None
}

/// Extract the first `100.64.0.0/10` IPv4 address from Linux `ip -4 -o addr show` output.
#[allow(dead_code)]
#[must_use]
pub fn parse_ip_addr_tailscale_ipv4(output: &str) -> Option<IpAddr> {
    for line in output.lines() {
        let mut tokens = line.split_whitespace();
        while let Some(tok) = tokens.next() {
            if tok == "inet"
                && let Some(cidr) = tokens.next()
                && let Some(ip_str) = cidr.split('/').next()
                && let Ok(ip) = ip_str.parse::<IpAddr>()
                && is_tailscale_ip(ip)
            {
                return Some(ip);
            }
        }
    }
    None
}

/// Caches `tailscale whois` peer verification so hot-path remote RPC adds 0 ms overhead.
pub struct TailscaleWhoisCache {
    self_user_id: Mutex<Option<Option<i64>>>,
    verified: Mutex<HashMap<IpAddr, Instant>>,
}

impl Default for TailscaleWhoisCache {
    fn default() -> Self {
        Self::new()
    }
}

impl TailscaleWhoisCache {
    #[must_use]
    pub fn new() -> Self {
        Self {
            self_user_id: Mutex::new(None),
            verified: Mutex::new(HashMap::new()),
        }
    }

    /// Fast non-blocking check whether `peer_ip` is already verified in the TTL cache.
    #[must_use]
    pub fn is_verified_cached(&self, peer_ip: IpAddr) -> bool {
        if peer_ip.is_loopback() {
            return true;
        }
        if !is_tailscale_ip(peer_ip) {
            return false;
        }
        let guard = self.verified.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(&peer_ip).is_some_and(|exp| Instant::now() < *exp)
    }

    /// Pre-populate `self_user_id` off the request hot path at daemon startup so the first cold
    /// `verify_peer` call avoids spawning a second `tailscale status --json` subprocess.
    pub fn prewarm_self_user_id(&self) {
        if let Some(bin) = find_tailscale_bin() {
            let _ = self.self_user_id(&bin);
        }
    }

    /// Verify that a Tailscale peer IP is in the Tailscale CGNAT/ULA range (or loopback) and,
    /// when `tailscale whois` is reachable, belongs to our own Tailscale user.
    pub fn verify_peer(&self, peer_ip: IpAddr) -> Result<(), String> {
        if peer_ip.is_loopback() {
            return Ok(());
        }
        if !is_tailscale_ip(peer_ip) {
            return Err(format!("peer {peer_ip} is not a Tailscale IP"));
        }

        // Fast path: check 5-minute TTL cache.
        {
            let guard = self.verified.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(exp) = guard.get(&peer_ip)
                && Instant::now() < *exp
            {
                return Ok(());
            }
        }

        // Best-effort `tailscale whois` check; if the macOS GUI CLI is unreachable from a
        // LaunchAgent (`Tailscale.CLIError`), fall back cleanly to Tailscale WireGuard IP +
        // mandatory 256-bit bearer token auth.
        if let Some(bin) = find_tailscale_bin()
            && let Ok(out) = Command::new(&bin)
                .args(["whois", "--json", &peer_ip.to_string()])
                .output()
        {
            if out.status.success()
                && let Ok(whois) = serde_json::from_slice::<serde_json::Value>(&out.stdout)
            {
                let peer_user = whois
                    .pointer("/UserProfile/ID")
                    .and_then(serde_json::Value::as_i64)
                    .or_else(|| {
                        whois
                            .pointer("/Node/User")
                            .and_then(serde_json::Value::as_i64)
                    });
                if let Some(my_uid) = self.self_user_id(&bin)
                    && let Some(p_uid) = peer_user
                    && p_uid != my_uid
                {
                    return Err(format!(
                        "Tailscale peer {peer_ip} belongs to user {p_uid}, not local user {my_uid}"
                    ));
                }
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr);
                tracing::debug!(
                    %peer_ip,
                    stderr = stderr.trim(),
                    "tailscale whois unavailable in LaunchAgent context; relying on Tailscale IP + bearer token"
                );
            }
        }

        self.verified
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(peer_ip, Instant::now() + WHOIS_CACHE_TTL);
        Ok(())
    }

    fn self_user_id(&self, bin: &Path) -> Option<i64> {
        let mut guard = self.self_user_id.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = *guard {
            return cached;
        }
        let uid = Command::new(bin)
            .args(["status", "--json"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
            .and_then(|v| {
                v.pointer("/Self/UserID")
                    .and_then(serde_json::Value::as_i64)
            });
        *guard = Some(uid);
        uid
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn parses_ifconfig_and_ip_addr_tailscale_ipv4() {
        let mac_sample = "\
en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tinet 192.168.1.42 netmask 0xffffff00 broadcast 192.168.1.255
utun4: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1280
\tinet 100.101.202.5 --> 100.101.202.5 netmask 0xffffffff
";
        assert_eq!(
            parse_ifconfig_tailscale_ipv4(mac_sample),
            Some(IpAddr::V4(Ipv4Addr::new(100, 101, 202, 5)))
        );

        let linux_sample = "\
1: lo    inet 127.0.0.1/8 scope host lo
2: eth0  inet 10.0.0.5/24 brd 10.0.0.255 scope global eth0
3: tailscale0 inet 100.64.12.34/32 scope global tailscale0
";
        assert_eq!(
            parse_ip_addr_tailscale_ipv4(linux_sample),
            Some(IpAddr::V4(Ipv4Addr::new(100, 64, 12, 34)))
        );
    }

    #[test]
    fn whois_cache_fast_path_recognizes_loopback_and_rejects_uncached_peers() {
        let cache = TailscaleWhoisCache::new();
        assert!(cache.is_verified_cached(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(!cache.is_verified_cached(IpAddr::V4(Ipv4Addr::new(100, 64, 1, 2))));
        assert!(!cache.is_verified_cached(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))));
    }
}
