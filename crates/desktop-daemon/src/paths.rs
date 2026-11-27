use std::path::PathBuf;

/// Default local Unix domain socket path for `desktopd`.
#[must_use]
pub fn default_socket_path() -> PathBuf {
    if let Ok(custom) = std::env::var("DESKTOP_SOCKET")
        && !custom.trim().is_empty()
    {
        return PathBuf::from(custom);
    }
    #[cfg(target_os = "macos")]
    {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("agent-desktop")
            .join("desktopd.sock")
    }
    #[cfg(not(target_os = "macos"))]
    {
        dirs::runtime_dir()
            .or_else(dirs::state_dir)
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("agent-desktop")
            .join("desktopd.sock")
    }
}

/// Default state directory (audit log, transient state).
#[must_use]
pub fn default_state_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("DESKTOP_STATE_DIR")
        && !custom.trim().is_empty()
    {
        return PathBuf::from(custom);
    }
    dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("agent-desktop")
}

/// Default opt-in policy file path.
#[must_use]
pub fn default_policy_path() -> PathBuf {
    if let Ok(custom) = std::env::var("DESKTOP_POLICY")
        && !custom.trim().is_empty()
    {
        return PathBuf::from(custom);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".config")
        .join("agent-desktop")
        .join("policy.toml")
}

/// Default fleet configuration path (`~/.config/agent-desktop/fleet.toml`).
#[must_use]
pub fn default_fleet_path() -> PathBuf {
    if let Ok(custom) = std::env::var("DESKTOP_FLEET")
        && !custom.trim().is_empty()
    {
        return PathBuf::from(custom);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".config")
        .join("agent-desktop")
        .join("fleet.toml")
}
