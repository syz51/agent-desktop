use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use desktop_proto::{AppInfo, ErrorCode, PolicyConfig, PolicyMode, RpcError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PolicyFileStamp {
    Missing,
    Present(SystemTime, u64),
    Uncacheable,
}

/// Reads and persists optional user policy in `~/.config/agent-desktop/policy.toml`.
///
/// Caches the parsed policy keyed by `(mtime, byte_len)` so hot-path RPCs avoid re-reading and
/// re-parsing TOML from disk while immediately picking up manual edits or `save()` updates.
/// When no policy file exists, defaults to `PolicyConfig::default()` (`mode = "open"`,
/// zero hardcoded app blocks, floating Stop overlay enabled).
pub struct PolicyStore {
    path: PathBuf,
    cache: Mutex<Option<(PolicyFileStamp, PolicyConfig)>>,
}

impl PolicyStore {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            cache: Mutex::new(None),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn current_stamp(&self) -> Result<PolicyFileStamp, RpcError> {
        match fs::metadata(&self.path) {
            Ok(meta) => match meta.modified().ok() {
                Some(mtime) => Ok(PolicyFileStamp::Present(mtime, meta.len())),
                None => Ok(PolicyFileStamp::Uncacheable),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(PolicyFileStamp::Missing),
            Err(err) => Err(RpcError::with_next(
                ErrorCode::BackendError,
                format!("could not stat policy {}: {err}", self.path.display()),
                "Fix file permissions or remove the broken policy.toml.",
            )),
        }
    }

    pub fn load(&self) -> Result<PolicyConfig, RpcError> {
        let stamp = self.current_stamp()?;
        if stamp != PolicyFileStamp::Uncacheable {
            let guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((cached_stamp, cfg)) = guard.as_ref()
                && *cached_stamp == stamp
            {
                return Ok(cfg.clone());
            }
        }
        let cfg = match stamp {
            PolicyFileStamp::Missing => PolicyConfig::default(),
            PolicyFileStamp::Present(_, _) | PolicyFileStamp::Uncacheable => {
                let text = match fs::read_to_string(&self.path) {
                    Ok(text) => text,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        let default_cfg = PolicyConfig::default();
                        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some((PolicyFileStamp::Missing, default_cfg.clone()));
                        return Ok(default_cfg);
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
                })?
            }
        };
        if stamp != PolicyFileStamp::Uncacheable {
            *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((stamp, cfg.clone()));
        }
        Ok(cfg)
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
        })?;
        let stamp = self
            .current_stamp()
            .ok()
            .filter(|s| *s != PolicyFileStamp::Uncacheable);
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = stamp.map(|s| (s, config.clone()));
        Ok(())
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

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn policy_store_caches_and_reloads_on_save_edit_and_delete() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("policy.toml");
        let store = PolicyStore::new(path.clone());

        // Missing file caches default PolicyConfig cleanly.
        let cfg0 = store.load().unwrap();
        assert_eq!(cfg0.mode, PolicyMode::Open);
        assert!(cfg0.deny_apps.is_empty());
        assert_eq!(store.load().unwrap().mode, PolicyMode::Open);

        // save() updates disk and cache immediately.
        let saved = PolicyConfig {
            mode: PolicyMode::Confirm,
            deny_apps: vec!["1Password".into()],
            ..PolicyConfig::default()
        };
        store.save(&saved).unwrap();
        let cfg1 = store.load().unwrap();
        assert_eq!(cfg1.mode, PolicyMode::Confirm);
        assert_eq!(cfg1.deny_apps, vec!["1Password".to_string()]);

        // External file edit with changed length invalidates cache immediately.
        fs::write(
            &path,
            "mode = \"open\"\ndeny_apps = [\"SecretApp\", \"VaultApp\"]\n",
        )
        .unwrap();
        let cfg2 = store.load().unwrap();
        assert_eq!(cfg2.mode, PolicyMode::Open);
        assert_eq!(
            cfg2.deny_apps,
            vec!["SecretApp".to_string(), "VaultApp".to_string()]
        );

        // Deleting policy.toml reverts cleanly to defaults.
        fs::remove_file(&path).unwrap();
        let cfg3 = store.load().unwrap();
        assert_eq!(cfg3.mode, PolicyMode::Open);
        assert!(cfg3.deny_apps.is_empty());
    }
}
