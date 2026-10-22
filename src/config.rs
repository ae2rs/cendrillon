use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

/// User settings, read from `~/.config/cendrillon/config.toml`. Every field is optional.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Log what would be deleted instead of deleting it.
    pub dry_run: bool,
    /// Delete an output base when no Bazel command has used it for this long.
    #[serde(with = "humantime_serde")]
    pub output_base_idle: Duration,
    /// Evict disk-cache and repository-cache entries untouched for this long.
    #[serde(with = "humantime_serde")]
    pub cache_entry_idle: Duration,
    /// Delete an install base no output base points at once it is this old.
    #[serde(with = "humantime_serde")]
    pub install_base_idle: Duration,
    /// Delete a Cargo build unit Cargo has neither compiled nor read for this long, unless it
    /// is the newest generation of a top-level unit or one of those needs it.
    #[serde(with = "humantime_serde")]
    pub unit_idle: Duration,
    /// Delete a whole Cargo target directory when Cargo has used nothing in it for this long.
    #[serde(with = "humantime_serde")]
    pub target_idle: Duration,
    /// Below this share of free disk space, evict the least recently used items early.
    pub pressure_free_percent: u8,
    /// Under pressure, evict until this share of the disk is free.
    pub target_free_percent: u8,
    /// Nothing used more recently than this is evicted under pressure.
    #[serde(with = "humantime_serde")]
    pub recent_use_floor: Duration,
    /// How often the daemon checks for orphaned output bases and disk pressure.
    #[serde(with = "humantime_serde")]
    pub tick_interval: Duration,
    /// How often the daemon applies the idle deadlines.
    #[serde(with = "humantime_serde")]
    pub sweep_interval: Duration,
    /// Minimum time between two pressure evictions, so space the system frees late
    /// (for example after thinning local snapshots) is not reclaimed twice.
    #[serde(with = "humantime_serde")]
    pub pressure_cooldown: Duration,
    /// Find caches and targets anywhere in the home directory: walk it once, then follow its
    /// changes on macOS, or walk it again every few hours on Linux.
    pub scan_home: bool,
    /// Extra directories to search for Bazel caches, besides the defaults and bazelrc files.
    pub extra_roots: Vec<PathBuf>,
}

const HOUR: Duration = Duration::from_secs(3600);

impl Default for Config {
    fn default() -> Self {
        Self {
            dry_run: false,
            output_base_idle: 24 * HOUR,
            cache_entry_idle: 72 * HOUR,
            install_base_idle: 72 * HOUR,
            unit_idle: 24 * HOUR,
            target_idle: 7 * 24 * HOUR,
            pressure_free_percent: 15,
            target_free_percent: 25,
            recent_use_floor: 2 * HOUR,
            tick_interval: Duration::from_secs(60),
            sweep_interval: HOUR,
            pressure_cooldown: Duration::from_secs(600),
            scan_home: true,
            extra_roots: Vec::new(),
        }
    }
}

impl Config {
    /// Loads the config at `path`, or the defaults when the file does not exist.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Self::parse(&text).with_context(|| format!("invalid config {}", path.display()))
    }

    fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        ensure!(
            config.pressure_free_percent <= config.target_free_percent
                && config.target_free_percent < 100,
            "need pressure_free_percent <= target_free_percent < 100"
        );
        let intervals = [
            ("tick_interval", config.tick_interval),
            ("sweep_interval", config.sweep_interval),
            ("pressure_cooldown", config.pressure_cooldown),
            ("target_idle", config.target_idle),
        ];
        for (name, interval) in intervals {
            ensure!(!interval.is_zero(), "{name} must be positive");
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn overrides_merge_with_defaults() {
        let config = Config::parse(
            r#"
            dry_run = true
            output_base_idle = "3d"
            unit_idle = "30m"
            pressure_free_percent = 20
            extra_roots = ["/Volumes/build/bazel"]
            "#,
        )
        .unwrap();
        assert!(config.dry_run);
        assert_eq!(config.output_base_idle, 72 * HOUR);
        assert_eq!(config.pressure_free_percent, 20);
        assert_eq!(config.extra_roots, [PathBuf::from("/Volumes/build/bazel")]);
        assert_eq!(config.cache_entry_idle, 72 * HOUR);
        assert_eq!(config.unit_idle, HOUR / 2);
        assert_eq!(config.target_idle, 7 * 24 * HOUR);
    }

    #[test]
    fn rejects_unknown_keys_and_inverted_thresholds() {
        assert!(Config::parse("output_base_ttl = \"1h\"").is_err());
        assert!(Config::parse("pressure_free_percent = 30").is_err());
        assert!(Config::parse("pressure_cooldown = \"0s\"").is_err());
        assert!(Config::parse("target_idle = \"0s\"").is_err());
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&dir.path().join("config.toml")).unwrap();
        assert_eq!(config, Config::default());
    }
}
