//! Layered configuration (figment), matching the house pattern (warden,
//! claude-proxy). Sources, later wins:
//!   1. built-in defaults
//!   2. /etc/beadsd/config.toml            (system-wide, optional)
//!   3. ~/.config/beadsd/config.toml       (shared user defaults, optional)
//!   4. --config <file>                    (per-instance, e.g. mu.toml — optional)
//!   5. BEADSD_* environment variables
//!   6. explicit CLI flags                 (applied by the caller)
//!
//! Per-instance values (`db`, the specific `listen` ip:port, `repo`) live in
//! the per-instance TOML passed with --config; shared knobs (br_bin, commit
//! interval, mcp/health paths, git identity) come from the layered defaults.

use std::path::{Path, PathBuf};

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/home/tcovert".into())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Path to the central project DB this instance owns. Required (no default).
    pub db: Option<PathBuf>,
    /// Address to bind, e.g. "0.0.0.0:7771". Network-reachable (trusted network).
    pub listen: String,
    /// Path to the br binary.
    pub br_bin: String,
    /// Central git repo root. When set (and commit_interval_secs > 0) beadsd
    /// debounce-commits this project's issues.jsonl for audit/backup.
    pub repo: Option<PathBuf>,
    /// Seconds between debounced snapshot commits (0 disables the committer).
    pub commit_interval_secs: u64,
    /// HTTP path the MCP service is mounted at.
    pub mcp_path: String,
    /// HTTP path for the health check.
    pub health_path: String,
    /// Identity used for snapshot commits (no PII; the central repo may be public).
    pub git_author_name: String,
    pub git_author_email: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            db: None,
            listen: "0.0.0.0:7777".into(),
            br_bin: "br".into(),
            repo: None,
            commit_interval_secs: 30,
            mcp_path: "/mcp".into(),
            health_path: "/health".into(),
            git_author_name: "beadsd".into(),
            git_author_email: "beadsd@localhost".into(),
        }
    }
}

impl Config {
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let h = home();
        let mut fig = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file("/etc/beadsd/config.toml"))
            .merge(Toml::file(format!("{h}/.config/beadsd/config.toml")));
        if let Some(p) = explicit {
            fig = fig.merge(Toml::file(p));
        }
        let cfg = fig.merge(Env::prefixed("BEADSD_")).extract()?;
        Ok(cfg)
    }
}
