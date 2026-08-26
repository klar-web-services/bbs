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
    pub max_file_bytes: u64,
    pub max_results: usize,
    pub context_lines: usize,
    pub snapshot_budget_gb: u64,
    pub result_budget_mb: u64,
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
            max_file_bytes: 4 * 1024 * 1024,
            max_results: 500,
            context_lines: 2,
            snapshot_budget_gb: 20,
            result_budget_mb: 1024,
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
