use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub api_base: String,
    pub default_port: u16,
    pub sync_concurrency: usize,
    pub search_threads: usize,
    /// Files larger than this are not searched. May be written as a byte
    /// count or as a size with a unit -- `"32M"` -- and `0` or `"none"`
    /// searches every file whatever its size. `--max-file-size` overrides it
    /// for a single search.
    #[serde(deserialize_with = "crate::size::deserialize_size")]
    pub max_file_bytes: u64,
    pub max_results: usize,
    pub context_lines: usize,
    /// Context lines kept in the result cache, so a later `--context` narrower
    /// than this is served by trimming rather than by rescanning.
    pub cache_context_lines: usize,
    /// Results kept in the result cache, independent of `--max-results`. A
    /// scan whose matches all fit here can be re-displayed in any sort order
    /// without touching a file.
    pub cache_max_results: usize,
    pub snapshot_budget_gb: u64,
    pub result_budget_mb: u64,
    /// Install an available update automatically when a command runs.
    /// Opt-in: replacing a user's binary is not a thing to do by surprise.
    pub auto_update: bool,
    #[serde(skip)]
    pub config_dir: PathBuf,
    #[serde(skip)]
    pub cache_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        let dirs = ProjectDirs::from("dev", "bbs", "better-bitbucket-search");
        let (config_dir, cache_dir) = match dirs {
            Some(d) => (d.config_dir().to_path_buf(), d.cache_dir().to_path_buf()),
            None => (PathBuf::from(".bbs/config"), PathBuf::from(".bbs/cache")),
        };
        Self {
            api_base: "https://api.bitbucket.org/2.0".into(),
            default_port: 7337,
            sync_concurrency: 6,
            search_threads: std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(4),
            max_file_bytes: 10 * 1024 * 1024,
            max_results: 500,
            context_lines: 2,
            cache_context_lines: 6,
            cache_max_results: 2000,
            snapshot_budget_gb: 20,
            result_budget_mb: 1024,
            auto_update: false,
            config_dir,
            cache_dir,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let defaults = Self::default();
        let path = defaults.config_path();
        if !path.exists() {
            return Ok(defaults);
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut config: Config = toml::from_str(&text)
            .with_context(|| format!("invalid configuration in {}", path.display()))?;
        config.config_dir = defaults.config_dir;
        config.cache_dir = defaults.cache_dir;
        Ok(config)
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.config_dir)?;
        fs::create_dir_all(self.snapshots_dir())?;
        fs::create_dir_all(self.results_dir())?;
        Ok(())
    }

    pub fn config_path(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub fn catalog_path(&self) -> PathBuf {
        self.cache_dir.join("catalog.json")
    }
    pub fn snapshots_dir(&self) -> PathBuf {
        self.cache_dir.join("snapshots")
    }
    pub fn results_dir(&self) -> PathBuf {
        self.cache_dir.join("results")
    }

    /// Writes just this key, leaving the rest of the file as the user wrote
    /// it. Serializing the whole struct instead would bake every current
    /// default into the file, freezing values the user never chose and
    /// silently opting them out of future default changes.
    pub fn set_auto_update(&self, enabled: bool) -> Result<()> {
        let path = self.config_path();
        let mut document: toml::Table = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("invalid configuration in {}", path.display()))?,
            Err(_) => toml::Table::new(),
        };
        document.insert("auto_update".into(), toml::Value::Boolean(enabled));
        fs::create_dir_all(&self.config_dir)?;
        fs::write(&path, toml::to_string_pretty(&document)?)
            .with_context(|| format!("cannot write {}", path.display()))?;
        Ok(())
    }

    pub fn validate_cache_target(&self, path: &Path) -> Result<()> {
        let root = self
            .cache_dir
            .canonicalize()
            .unwrap_or_else(|_| self.cache_dir.clone());
        let candidate = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        anyhow::ensure!(
            candidate.starts_with(root),
            "refusing to operate outside the bbs cache root"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn config_in(dir: &Path) -> Config {
        Config {
            config_dir: dir.to_path_buf(),
            ..Default::default()
        }
    }

    #[test]
    fn auto_update_is_off_unless_asked_for() {
        assert!(!Config::default().auto_update);
    }

    /// The one limit people actually reach for should not have to be spelled
    /// in bytes, and the file must keep accepting the byte counts already
    /// written in it.
    #[test]
    fn the_file_size_limit_may_be_written_either_way() {
        let count: Config = toml::from_str("max_file_bytes = 8388608").unwrap();
        assert_eq!(count.max_file_bytes, 8 * 1024 * 1024);
        let phrase: Config = toml::from_str(r#"max_file_bytes = "32M""#).unwrap();
        assert_eq!(phrase.max_file_bytes, 32 * 1024 * 1024);
        let unlimited: Config = toml::from_str(r#"max_file_bytes = "none""#).unwrap();
        assert_eq!(unlimited.max_file_bytes, u64::MAX);
        assert!(toml::from_str::<Config>(r#"max_file_bytes = "wat""#).is_err());
        assert_eq!(Config::default().max_file_bytes, 10 * 1024 * 1024);
    }

    /// Writing the preference must not rewrite the file wholesale: a user's
    /// existing keys survive, and defaults they never set are not frozen in.
    #[test]
    fn setting_the_preference_preserves_other_keys() {
        let dir = tempdir().unwrap();
        let config = config_in(dir.path());
        std::fs::write(config.config_path(), "default_port = 9000\n").unwrap();

        config.set_auto_update(true).unwrap();

        let text = std::fs::read_to_string(config.config_path()).unwrap();
        assert!(text.contains("default_port = 9000"), "{text}");
        assert!(text.contains("auto_update = true"), "{text}");
        assert!(
            !text.contains("snapshot_budget_gb"),
            "defaults leaked: {text}"
        );
    }

    #[test]
    fn the_preference_round_trips_and_can_be_turned_off() {
        let dir = tempdir().unwrap();
        let config = config_in(dir.path());

        // Note: `assert!(x)`, not `assert_eq!(x, true)` —
        // `clippy::bool_assert_comparison` is warn-by-default and this repo
        // gates CI on `clippy -D warnings`.
        config.set_auto_update(true).unwrap();
        let text = std::fs::read_to_string(config.config_path()).unwrap();
        assert!(toml::from_str::<Config>(&text).unwrap().auto_update);

        config.set_auto_update(false).unwrap();
        let text = std::fs::read_to_string(config.config_path()).unwrap();
        assert!(!toml::from_str::<Config>(&text).unwrap().auto_update);
    }

    #[test]
    fn the_preference_can_be_written_without_an_existing_file() {
        let dir = tempdir().unwrap();
        let config = config_in(&dir.path().join("nested"));

        config.set_auto_update(true).unwrap();

        assert!(config.config_path().exists());
    }
}
