//! Rule + settings persistence as JSON at `%APPDATA%\rl-app\rules.json`.
//!
//! Everything the engine needs across restarts lives in one file: the per-exe
//! rules, the master switch, and the "other traffic" fallback limits. A
//! missing file is treated as defaults (empty rules, master on, no fallback);
//! parse errors are logged and also fall back to defaults so a corrupt file
//! never crashes the backend.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::types::Rule;
use super::OtherLimits;

/// Everything persisted across restarts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Persisted {
    /// Per-exe rules keyed by lowercase executable path.
    #[serde(default)]
    pub rules: HashMap<String, Rule>,
    /// Master switch. Defaults to on.
    #[serde(default = "default_true")]
    pub master: bool,
    /// Fallback download limit for traffic that matches no rule.
    #[serde(default)]
    pub other_down_bps: Option<u64>,
    /// Fallback upload limit for traffic that matches no rule.
    #[serde(default)]
    pub other_up_bps: Option<u64>,
}

impl Default for Persisted {
    fn default() -> Self {
        Self {
            rules: HashMap::new(),
            master: true,
            other_down_bps: None,
            other_up_bps: None,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Directory that holds our persisted state: `%APPDATA%\rl-app`.
pub fn config_dir() -> PathBuf {
    // APPDATA is always set for interactive users on Windows. If it is somehow
    // absent (unusual service contexts), fall back to the current directory so
    // we still have somewhere to write.
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("rl-app")
}

/// Full path to the rules file.
pub fn rules_path() -> PathBuf {
    config_dir().join("rules.json")
}

/// Load the persisted state. Missing file or any error => defaults.
pub fn load() -> Persisted {
    let path = rules_path();
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("no rules file at {}, starting fresh", path.display());
            return Persisted::default();
        }
        Err(e) => {
            tracing::warn!("failed to read {}: {e}", path.display());
            return Persisted::default();
        }
    };

    // A UTF-8 byte order mark from an editor must not empty the state.
    match serde_json::from_str::<Persisted>(data.trim_start_matches('\u{feff}')) {
        Ok(mut p) => {
            // Defensively normalise keys to lowercase so lookups from the engine
            // (which always lowercases) match regardless of how the file was written.
            p.rules = p
                .rules
                .into_iter()
                .map(|(k, mut v)| {
                    let key = k.to_lowercase();
                    v.exe_path = v.exe_path.to_lowercase();
                    (key, v)
                })
                .collect();
            tracing::info!(
                "loaded {} rule(s) from {} (master={})",
                p.rules.len(),
                path.display(),
                p.master
            );
            p
        }
        Err(e) => {
            tracing::warn!("failed to parse {}: {e}; starting fresh", path.display());
            Persisted::default()
        }
    }
}

/// Build the persisted state from the live shared state.
pub(crate) fn persisted_from(rules: &HashMap<String, Rule>, master: bool, other: &OtherLimits) -> Persisted {
    Persisted {
        rules: rules.clone(),
        master,
        other_down_bps: other.down_bps,
        other_up_bps: other.up_bps,
    }
}

/// Persist the given state as pretty JSON, creating the directory if needed.
pub fn save(persisted: &Persisted) -> anyhow::Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let path = rules_path();
    let json = serde_json::to_string_pretty(persisted)?;
    std::fs::write(&path, json)?;
    tracing::debug!("saved state to {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_round_trip_through_json() {
        let mut rules: HashMap<String, Rule> = HashMap::new();
        rules.insert(
            "c:\\program files\\steam\\steam.exe".to_string(),
            Rule {
                exe_path: "c:\\program files\\steam\\steam.exe".to_string(),
                down_limit: Some(2 * 1024 * 1024),
                up_limit: None,
                blocked: false,
                disabled: false,
            },
        );
        let p = Persisted {
            rules,
            master: false,
            other_down_bps: Some(1024 * 1024),
            other_up_bps: None,
        };

        let json = serde_json::to_string_pretty(&p).expect("serialize");
        let back: Persisted = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(p, back);
    }

    #[test]
    fn missing_fields_get_defaults() {
        // An old/partial file must not fail: master defaults on, limits to None.
        let back: Persisted = serde_json::from_str("{}").unwrap();
        assert!(back.master);
        assert!(back.rules.is_empty());
        assert!(back.other_down_bps.is_none());
    }

    #[test]
    fn empty_map_round_trips() {
        let p = Persisted::default();
        let json = serde_json::to_string(&p).unwrap();
        let back: Persisted = serde_json::from_str(&json).unwrap();
        assert!(back.rules.is_empty());
        assert!(back.master);
    }

    #[test]
    fn rule_defaults_are_unlimited_and_unblocked() {
        let r = Rule::default();
        assert!(r.down_limit.is_none());
        assert!(r.up_limit.is_none());
        assert!(!r.blocked);
        assert!(!r.disabled);
    }
}
