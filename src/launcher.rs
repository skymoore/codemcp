//! Identify the application that launched this gateway, so concurrent instances
//! (e.g. one per harness) are human-distinguishable in `codemcp info`/`list`.
//!
//! Two signals, in priority order:
//!   1. `CODEMCP_INSTANCE_LABEL` — an explicit, friendly name set by the harness
//!      (e.g. `setup` writes `opencode`). Always wins when present.
//!   2. The parent process name (auto-detected via `sysinfo`), e.g. `lmstudio`.
//!
//! Captured once at startup; the parent may exit later, so we don't rely on it
//! being queryable for the lifetime of the gateway.

use serde::Serialize;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Where the launcher name came from.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LauncherSource {
    /// From `CODEMCP_INSTANCE_LABEL`.
    Label,
    /// Auto-detected parent process name.
    Parent,
    /// Could not determine.
    Unknown,
}

/// Identity of the launching application.
#[derive(Debug, Clone, Serialize)]
pub struct Launcher {
    /// Best-effort friendly name (label or parent process name).
    pub name: String,
    /// Parent process id, if known.
    pub parent_pid: Option<u32>,
    /// How `name` was derived.
    pub source: LauncherSource,
}

impl Launcher {
    /// Detect the launcher once. Reads `CODEMCP_INSTANCE_LABEL` and, regardless,
    /// records the parent pid/name when discoverable.
    pub fn detect() -> Self {
        let label = std::env::var("CODEMCP_INSTANCE_LABEL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let (parent_pid, parent_name) = parent_info();

        match (label, parent_name) {
            (Some(label), _) => Launcher {
                name: label,
                parent_pid,
                source: LauncherSource::Label,
            },
            (None, Some(pname)) => Launcher {
                name: pname,
                parent_pid,
                source: LauncherSource::Parent,
            },
            (None, None) => Launcher {
                name: "unknown".to_string(),
                parent_pid,
                source: LauncherSource::Unknown,
            },
        }
    }
}

/// Best-effort `(parent_pid, parent_process_name)` for the current process.
fn parent_info() -> (Option<u32>, Option<String>) {
    let mut sys = System::new();
    // Refresh just process metadata; this is enough to walk parent links.
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());

    let me = match sysinfo::get_current_pid() {
        Ok(pid) => pid,
        Err(_) => return (None, None),
    };

    let parent_pid: Option<Pid> = sys.process(me).and_then(|p| p.parent());
    let Some(ppid) = parent_pid else {
        return (None, None);
    };

    let name = sys
        .process(ppid)
        .map(|p| p.name().to_string_lossy().to_string());

    (Some(ppid.as_u32()), name)
}
