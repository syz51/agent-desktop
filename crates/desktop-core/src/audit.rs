use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct AuditEvent<'a> {
    pub time: DateTime<Utc>,
    pub event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<&'a str>,
}

/// Append-only JSONL audit log in the state directory. Never logs typed text or clipboard contents.
pub struct AuditLog {
    path: PathBuf,
    lock: Mutex<()>,
}

impl AuditLog {
    #[must_use]
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("events.jsonl"),
            lock: Mutex::new(()),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, event: AuditEvent<'_>) {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let Ok(line) = serde_json::to_string(&event) else {
            return;
        };
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}
