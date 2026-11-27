use std::fs;
use std::path::{Path, PathBuf};

use desktop_proto::{AppInfo, ErrorCode, PolicyConfig, PolicyMode, RpcError};

/// Reads and persists optional user policy in `~/.config/agent-desktop/policy.toml`.
///
/// When no policy file exists, defaults to `PolicyConfig::default()` (`mode = "open"`,
/// zero hardcoded app blocks, floating Stop overlay enabled).
pub struct PolicyStore {
    path: PathBuf,
}

impl PolicyStore {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<PolicyConfig, RpcError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PolicyConfig::default());
            }
            Err(err) => {
                return Err(RpcError::with_next(
                    ErrorCode::BackendError,
                    format!("could not read policy {}: {err}", self.path.display()),
                    "Fix file permissions or remove the broken policy.toml.",
                ));
            }
        };
        toml::from_str(&text).map_err(|err| {
            RpcError::with_next(
                ErrorCode::InvalidInput,
                format!("invalid TOML in {}: {err}", self.path.display()),
                "Fix policy.toml syntax or delete it to restore the default open policy.",
            )
        })
    }

    pub fn save(&self, config: &PolicyConfig) -> Result<(), RpcError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                RpcError::new(
                    ErrorCode::BackendError,
                    format!("could not create policy directory: {err}"),
                )
            })?;
        }
        let body = toml::to_string_pretty(config).map_err(|err| {
            RpcError::new(ErrorCode::Internal, format!("serialize policy: {err}"))
        })?;
        let tmp = self.path.with_extension("toml.tmp");
        fs::write(&tmp, body).map_err(|err| {
            RpcError::new(
                ErrorCode::BackendError,
                format!("write {}: {err}", tmp.display()),
            )
        })?;
        fs::rename(&tmp, &self.path).map_err(|err| {
            RpcError::new(
                ErrorCode::BackendError,
                format!("commit {}: {err}", self.path.display()),
            )
        })
    }
}

/// Evaluate user-configured opt-in policy against an app.
pub fn check_app_policy(policy: &PolicyConfig, app: &AppInfo) -> Result<bool, RpcError> {
    if policy
        .deny_apps
        .iter()
        .any(|rule| app.matches_policy_rule(rule))
    {
        return Err(RpcError::with_next(
            ErrorCode::PolicyDenied,
            format!("{} is in your policy.toml deny_apps list", app.name),
            "Remove it from deny_apps in policy.toml if you want agents to control it.",
        ));
    }
    let needs_confirm = policy.mode == PolicyMode::Confirm
        || policy
            .confirm_apps
            .iter()
            .any(|rule| app.matches_policy_rule(rule));
    Ok(needs_confirm)
}
