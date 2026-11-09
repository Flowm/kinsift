//! Configuration: `~/.config/kinsift/config.toml` merged over built-in defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The built-in defaults, in the same format as the user config.
pub const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub roots: Vec<RootConfig>,
    pub providers: ProvidersConfig,
    pub views: Vec<View>,
    pub keep_rules: Vec<KeepRuleConfig>,
    pub trash: TrashConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProvidersConfig {
    /// Per domain, provider names in priority order (highest first).
    pub priority: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootConfig {
    pub path: PathBuf,
    /// `auto` (default), `trash`, `quarantine` or `permanent`.
    #[serde(default)]
    pub delete: Option<String>,
    /// Quarantine directory; must be on the same filesystem as the root.
    #[serde(default)]
    pub quarantine: Option<PathBuf>,
}

/// A saved way of looking at the index.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Group-by fields, outermost first. `key:year`, `key:month` and
    /// `key:day` bucket time fields.
    pub group_by: Vec<String>,
    #[serde(default)]
    pub filter: Option<String>,
    /// Show only groups at the last level with more than one file.
    #[serde(default)]
    pub duplicates_only: bool,
    /// Keep-rule offered for bulk cleanup in this view.
    #[serde(default)]
    pub keep_rule: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct KeepRuleConfig {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// `keep-best` or `drop-dominated`.
    pub mode: String,
    /// Ranking terms, best first: `field desc`, `field asc`,
    /// `field in [a, b]`, `field is null`, `field is not null`.
    pub rank: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TrashConfig {
    /// Remind when a root's quarantine grows beyond this size, e.g. `"50GiB"`.
    pub remind_size: String,
    /// Remind when the oldest quarantine batch is older than this many days.
    pub remind_age_days: u32,
}

impl Default for TrashConfig {
    fn default() -> Self {
        TrashConfig { remind_size: "50GiB".into(), remind_age_days: 30 }
    }
}

impl Config {
    pub fn defaults() -> Config {
        toml::from_str(DEFAULT_CONFIG).expect("built-in default config is valid")
    }

    /// Load the user config at `path` (if it exists) on top of the defaults.
    pub fn load(path: Option<&Path>) -> Result<Config> {
        let mut config = Config::defaults();
        let Some(path) = path else { return Ok(config) };
        if !path.exists() {
            return Ok(config);
        }
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let user: Config = toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        config.merge(user);
        Ok(config)
    }

    fn merge(&mut self, user: Config) {
        self.roots = user.roots;
        for (domain, order) in user.providers.priority {
            self.providers.priority.insert(domain, order);
        }
        for view in user.views {
            match self.views.iter_mut().find(|v| v.name == view.name) {
                Some(existing) => *existing = view,
                None => self.views.push(view),
            }
        }
        for rule in user.keep_rules {
            match self.keep_rules.iter_mut().find(|r| r.name == rule.name) {
                Some(existing) => *existing = rule,
                None => self.keep_rules.push(rule),
            }
        }
        self.trash = user.trash;
    }

    pub fn view(&self, name: &str) -> Option<&View> {
        self.views.iter().find(|v| v.name == name)
    }

    pub fn keep_rule(&self, name: &str) -> Option<&KeepRuleConfig> {
        self.keep_rules.iter().find(|r| r.name == name)
    }

    pub fn root_config(&self, root: &Path) -> Option<&RootConfig> {
        self.roots.iter().find(|r| r.path == root || r.path.canonicalize().ok().as_deref() == Some(root))
    }
}

/// Default config file location.
pub fn default_config_path() -> Option<PathBuf> {
    std::env::var_os("KINSIFT_CONFIG")
        .map(PathBuf::from)
        .or_else(|| dirs::config_dir().map(|d| d.join("kinsift").join("config.toml")))
}

/// Default index location.
pub fn default_index_path() -> Option<PathBuf> {
    std::env::var_os("KINSIFT_INDEX")
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir().map(|d| d.join("kinsift").join("index.db")))
}

/// Parse sizes like `50GiB`, `1.5 GB`, `700M` (bare letters are binary).
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num.parse().ok()?;
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "k" | "kib" => 1024.0,
        "m" | "mib" => 1024f64.powi(2),
        "g" | "gib" => 1024f64.powi(3),
        "t" | "tib" => 1024f64.powi(4),
        _ => return None,
    };
    Some((num * mult) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let c = Config::defaults();
        assert!(c.view("folders").is_some());
        assert!(c.view("series").is_some());
        assert!(c.keep_rule("lower-resolution").is_some());
    }

    #[test]
    fn user_views_override_by_name() {
        let mut c = Config::defaults();
        let user: Config = toml::from_str(
            r#"
            [[views]]
            name = "series"
            group_by = ["media.title"]
            [[views]]
            name = "mine"
            group_by = ["fs.ext"]
            [providers.priority]
            video = ["ffprobe"]
            "#,
        )
        .unwrap();
        c.merge(user);
        assert_eq!(c.view("series").unwrap().group_by, ["media.title"]);
        assert!(c.view("mine").is_some());
        assert_eq!(c.providers.priority["video"], ["ffprobe"]);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("50GiB"), Some(50 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1.5 GB"), Some(1_500_000_000));
        assert_eq!(parse_size("700M"), Some(700 * 1024 * 1024));
        assert_eq!(parse_size("x"), None);
    }
}
