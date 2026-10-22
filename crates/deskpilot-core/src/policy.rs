//! Safety policy: process deny list, dry-run, strategy ceiling and audit log.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::WindowInfo;

/// Interaction strategies, from least to most invasive. The engine always
/// tries them in this order and stops at the first verified success.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
    clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Accessibility actions (UIA patterns / AX actions). No focus change.
    Accessibility,
    /// Native window messages for classic controls. No focus change.
    Messages,
    /// Real keyboard input with focus navigation verified through accessibility.
    /// Takes the foreground from the user while it runs.
    Keyboard,
    /// Real mouse input at the element's on-screen position. Takes the
    /// foreground and moves the pointer while it runs.
    Mouse,
}

impl Strategy {
    pub const ALL: [Strategy; 4] = [
        Strategy::Accessibility,
        Strategy::Messages,
        Strategy::Keyboard,
        Strategy::Mouse,
    ];

    pub fn takes_focus(self) -> bool {
        matches!(self, Strategy::Keyboard | Strategy::Mouse)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Process names (without `.exe`, case-insensitive) that must never receive
    /// actions, input or window operations. Reading is still allowed.
    pub deny_processes: Vec<String>,
    /// Highest strategy the engine may escalate to unless a call lowers it.
    pub max_strategy: Option<Strategy>,
    /// Record every mutating call as JSON lines in the audit log.
    pub audit: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub deny_processes: Vec<String>,
    pub max_strategy: Strategy,
    pub dry_run: bool,
    pub audit_path: Option<PathBuf>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            deny_processes: Vec::new(),
            max_strategy: Strategy::Mouse,
            dry_run: false,
            audit_path: None,
        }
    }
}

impl Policy {
    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("deskpilot").join("config.json"))
    }

    pub fn default_audit_path() -> Option<PathBuf> {
        dirs::data_local_dir().map(|d| d.join("deskpilot").join("audit.jsonl"))
    }

    /// Loads the user config file (if any); command-line values are applied by the caller.
    pub fn load(path: Option<&PathBuf>) -> Result<Self> {
        let path = path.cloned().or_else(Self::config_path);
        let config = match path {
            Some(p) if p.exists() => {
                let text = std::fs::read_to_string(&p)
                    .map_err(|e| Error::InvalidArgument(format!("{}: {e}", p.display())))?;
                serde_json::from_str::<Config>(&text)
                    .map_err(|e| Error::InvalidArgument(format!("{}: {e}", p.display())))?
            }
            _ => Config::default(),
        };
        Ok(Self {
            deny_processes: config.deny_processes,
            max_strategy: config.max_strategy.unwrap_or(Strategy::Mouse),
            dry_run: false,
            audit_path: if config.audit.unwrap_or(true) {
                Self::default_audit_path()
            } else {
                None
            },
        })
    }

    pub fn check_window(&self, window: &WindowInfo) -> Result<()> {
        for denied in &self.deny_processes {
            let denied = denied.trim_end_matches(".exe");
            if window
                .process_names()
                .any(|p| p.eq_ignore_ascii_case(denied))
            {
                return Err(Error::PolicyDenied(format!(
                    "process '{denied}' is in the deny list (window {} \"{}\")",
                    window.id, window.title
                )));
            }
        }
        Ok(())
    }

    pub fn audit(&self, tool: &str, args: &serde_json::Value, outcome: &str) {
        let Some(path) = &self.audit_path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let line = serde_json::json!({ "ts_ms": ts, "pid": std::process::id(), "tool": tool, "args": args, "outcome": outcome, "dry_run": self.dry_run });
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }
}
