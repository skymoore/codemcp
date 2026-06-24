//! `codemcp setup <harness>` — wire codemcp into an agent harness.
//!
//! Backs up the harness config, moves its MCP servers verbatim into codemcp's own
//! `mcp.json`, then rewrites the harness config to launch codemcp as its single
//! MCP server. Only `opencode` is supported today.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::env::Settings;
use crate::error::Error;

/// Supported harnesses.
#[derive(Debug, Clone, Copy)]
pub enum Harness {
    Opencode,
}

impl std::str::FromStr for Harness {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "opencode" => Ok(Harness::Opencode),
            other => Err(format!(
                "unsupported harness {other:?}; supported: opencode"
            )),
        }
    }
}

/// Run setup for the given harness.
pub fn run(harness: Harness) -> Result<(), Error> {
    match harness {
        Harness::Opencode => setup_opencode(),
    }
}

fn setup_opencode() -> Result<(), Error> {
    let opencode_path = opencode_config_path()?;
    if !opencode_path.exists() {
        return Err(Error::Config(format!(
            "opencode config not found at {}",
            opencode_path.display()
        )));
    }

    // Read + parse the opencode config (preserve everything we don't touch).
    let raw = std::fs::read_to_string(&opencode_path)
        .map_err(|e| Error::Config(format!("cannot read {}: {e}", opencode_path.display())))?;
    let mut root: Value = serde_json::from_str(&raw)
        .map_err(|e| Error::Config(format!("invalid JSON in {}: {e}", opencode_path.display())))?;

    let mcp = root
        .get("mcp")
        .cloned()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));
    let server_count = mcp.as_object().map(|m| m.len()).unwrap_or(0);

    // 1) Back up the opencode config.
    let backup = backup_file(&opencode_path)?;
    println!(
        "backed up {} -> {}",
        opencode_path.display(),
        backup.display()
    );

    // 2) Move the mcp section verbatim into codemcp's mcp.json (backing up any
    //    existing one first).
    let codemcp_path = Settings::config_path_for_setup();
    if let Some(parent) = codemcp_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if codemcp_path.exists() {
        let b = backup_file(&codemcp_path)?;
        println!(
            "backed up existing {} -> {}",
            codemcp_path.display(),
            b.display()
        );
    }
    let codemcp_config = json!({ "mcp": mcp });
    write_json(&codemcp_path, &codemcp_config)?;
    println!(
        "wrote {} upstream server(s) to {}",
        server_count,
        codemcp_path.display()
    );

    // 3) Rewrite opencode's mcp section to launch codemcp as the single server,
    //    and reset the tools block to codemcp's single tool.
    let obj = root
        .as_object_mut()
        .ok_or_else(|| Error::Config("opencode config root is not an object".into()))?;
    obj.insert(
        "mcp".to_string(),
        json!({
            "codemcp": {
                "type": "local",
                "command": ["codemcp"],
                "environment": {
                    "CODEMCP_CONFIG": codemcp_path.to_string_lossy(),
                    "CODEMCP_INSTANCE_LABEL": "opencode"
                },
                "enabled": true
            }
        }),
    );
    obj.insert(
        "tools".to_string(),
        json!({
            "codemcp*": true,
            "execute_python": true
        }),
    );

    write_json(&opencode_path, &root)?;
    println!("updated {} to launch codemcp", opencode_path.display());

    if which::which("codemcp").is_err() {
        println!(
            "\nwarning: `codemcp` was not found on PATH. opencode launches it as the bare \
             command `codemcp`, so install it on PATH (e.g. `cargo install --path .` or copy \
             the binary into a PATH dir) before starting opencode."
        );
    }

    println!("\nsetup complete. restart opencode to use codemcp.");
    Ok(())
}

fn opencode_config_path() -> Result<PathBuf, Error> {
    Ok(crate::env::config_base()
        .join("opencode")
        .join("opencode.json"))
}

/// Copy `path` to a timestamped `.bak.<ts>` sibling. Returns the backup path.
fn backup_file(path: &Path) -> Result<PathBuf, Error> {
    let ts = backup_timestamp();
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".bak.{ts}"));
    let backup = path.with_file_name(name);
    std::fs::copy(path, &backup)
        .map_err(|e| Error::Config(format!("backup {} failed: {e}", path.display())))?;
    Ok(backup)
}

fn write_json(path: &Path, value: &Value) -> Result<(), Error> {
    let mut text = serde_json::to_string_pretty(value)
        .map_err(|e| Error::Config(format!("serialize {} failed: {e}", path.display())))?;
    text.push('\n');
    std::fs::write(path, text)
        .map_err(|e| Error::Config(format!("write {} failed: {e}", path.display())))?;
    Ok(())
}

/// `YYYYMMDD-HHMMSS` from the system clock, no extra deps.
fn backup_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Convert epoch seconds to a UTC calendar timestamp.
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// Days since Unix epoch -> (year, month, day), proleptic Gregorian.
/// Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
