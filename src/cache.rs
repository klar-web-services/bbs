use crate::{
    config::Config,
    model::{RepositoryCatalog, SearchResponse, Snapshot},
    query::QueryFingerprint,
    search::SearchOptions,
};
use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

const CACHE_SCHEMA: u32 = 1;

#[derive(Serialize)]
struct CacheKey<'a> {
    schema: u32,
    query: &'a QueryFingerprint,
    options: &'a SearchOptions,
    snapshots: Vec<(&'a str, &'a str, &'a str)>,
}

pub fn result_key(
    query: &QueryFingerprint,
    options: &SearchOptions,
    snapshots: &[Snapshot],
) -> Result<String> {
    let mut ordered = snapshots.iter().collect::<Vec<_>>();
    ordered.sort_by(|a, b| {
        a.repository
            .uuid
            .cmp(&b.repository.uuid)
            .then_with(|| a.branch.cmp(&b.branch))
    });
    let key = CacheKey {
        schema: CACHE_SCHEMA,
        query,
        options,
        snapshots: ordered
            .into_iter()
            .map(|s| {
                (
                    s.repository.uuid.as_str(),
                    s.branch.as_str(),
                    s.commit.as_str(),
                )
            })
            .collect(),
    };
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&key)?)))
}

pub fn load_result(config: &Config, key: &str) -> Result<Option<SearchResponse>> {
    let path = result_path(config, key);
    if !path.exists() {
        return Ok(None);
    }
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let mut decoder = match zstd::Decoder::new(file) {
        Ok(decoder) => decoder,
        Err(_) => {
            let _ = fs::remove_file(path);
            return Ok(None);
        }
    };
    let mut bytes = Vec::new();
    if decoder.read_to_end(&mut bytes).is_err() {
        let _ = fs::remove_file(path);
        return Ok(None);
    }
    match serde_json::from_slice::<SearchResponse>(&bytes) {
        Ok(mut response) => {
            if let Ok(file) = fs::OpenOptions::new().write(true).open(&path) {
                let _ = file.set_modified(SystemTime::now());
            }
            response.cached = true;
            Ok(Some(response))
        }
        Err(_) => {
            let _ = fs::remove_file(path);
            Ok(None)
        }
    }
}

pub fn save_result(config: &Config, key: &str, response: &SearchResponse) -> Result<()> {
    fs::create_dir_all(config.results_dir())?;
    let target = result_path(config, key);
    let temp = tempfile::NamedTempFile::new_in(config.results_dir())?;
    {
        let mut encoder = zstd::Encoder::new(temp.as_file(), 3)?;
        encoder.write_all(&serde_json::to_vec(response)?)?;
        encoder.finish()?;
    }
    temp.persist(&target).map_err(|error| error.error)?;
    Ok(())
}

fn result_path(config: &Config, key: &str) -> PathBuf {
    config.results_dir().join(format!("{key}.json.zst"))
}

pub fn load_catalog(config: &Config) -> Result<RepositoryCatalog> {
    let path = config.catalog_path();
    let bytes = fs::read(&path).with_context(|| {
        format!(
            "no cached repository catalog at {}; run an online search first",
            path.display()
        )
    })?;
    serde_json::from_slice(&bytes).context("cached repository catalog is corrupt")
}

pub fn save_catalog(config: &Config, catalog: &RepositoryCatalog) -> Result<()> {
    fs::create_dir_all(&config.cache_dir)?;
    atomic_json(&config.catalog_path(), catalog)
}

fn atomic_json<T: Serialize>(target: &Path, value: &T) -> Result<()> {
    let parent = target.parent().context("cache target has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temp, value)?;
    temp.flush()?;
    temp.persist(target).map_err(|error| error.error)?;
    Ok(())
}

#[derive(Debug, serde::Serialize)]
pub struct CacheStatus {
    pub snapshot_bytes: u64,
    pub result_bytes: u64,
    pub snapshots: usize,
    pub result_entries: usize,
}

pub fn status(config: &Config) -> Result<CacheStatus> {
    let (snapshot_bytes, _) = tree_stats(&config.snapshots_dir())?;
    let snapshots = snapshot_entries(config)?.len();
    let (result_bytes, result_entries) = tree_stats(&config.results_dir())?;
    Ok(CacheStatus {
        snapshot_bytes,
        result_bytes,
        snapshots,
        result_entries,
    })
}

fn tree_stats(root: &Path) -> Result<(u64, usize)> {
    if !root.exists() {
        return Ok((0, 0));
    }
    let mut bytes = 0;
    let mut files = 0;
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        if entry.file_type().is_file() {
            bytes += entry.metadata()?.len();
            files += 1;
        }
    }
    Ok((bytes, files))
}

pub fn clear_results(config: &Config) -> Result<()> {
    let path = config.results_dir();
    if path.exists() {
        config.validate_cache_target(&path)?;
        fs::remove_dir_all(&path)?;
    }
    fs::create_dir_all(path)?;
    Ok(())
}

pub fn prune_results(config: &Config) -> Result<u64> {
    let budget = config.result_budget_mb * 1024 * 1024;
    let mut entries = Vec::new();
    if config.results_dir().exists() {
        for entry in fs::read_dir(config.results_dir())? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_file() {
                entries.push((
                    entry.path(),
                    metadata.len(),
                    metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                ));
            }
        }
    }
    let total: u64 = entries.iter().map(|(_, size, _)| *size).sum();
    if total <= budget {
        return Ok(0);
    }
    entries.sort_by_key(|(_, _, modified)| *modified);
    let mut current = total;
    let mut removed = 0;
    for (path, size, _) in entries {
        if current <= budget {
            break;
        }
        config.validate_cache_target(&path)?;
        fs::remove_file(path)?;
        current -= size;
        removed += size;
    }
    Ok(removed)
}

pub fn prune_snapshots(config: &Config) -> Result<u64> {
    let budget = config.snapshot_budget_gb * 1024 * 1024 * 1024;
    let mut entries = snapshot_entries(config)?;
    let total: u64 = entries.iter().map(|entry| entry.size).sum();
    if total <= budget {
        return Ok(0);
    }
    entries.sort_by_key(|entry| entry.used);
    let mut current = total;
    let mut removed = 0;
    for entry in entries {
        if current <= budget {
            break;
        }
        config.validate_cache_target(&entry.path)?;
        fs::remove_dir_all(&entry.path)?;
        let marker = entry.path.with_extension("used");
        if marker.exists() {
            let _ = fs::remove_file(marker);
        }
        current = current.saturating_sub(entry.size);
        removed += entry.size;
    }
    Ok(removed)
}

struct SnapshotEntry {
    path: PathBuf,
    size: u64,
    used: SystemTime,
}

fn snapshot_entries(config: &Config) -> Result<Vec<SnapshotEntry>> {
    let root = config.snapshots_dir();
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for repository in fs::read_dir(&root)? {
        let repository = repository?;
        if !repository.file_type()?.is_dir() {
            continue;
        }
        for branch in fs::read_dir(repository.path())? {
            let branch = branch?;
            if !branch.file_type()?.is_dir() || !branch.path().join(".git").exists() {
                continue;
            }
            let (size, _) = tree_stats(&branch.path())?;
            let marker = branch.path().with_extension("used");
            let used = fs::metadata(marker)
                .and_then(|metadata| metadata.modified())
                .or_else(|_| branch.metadata()?.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            entries.push(SnapshotEntry {
                path: branch.path(),
                size,
                used,
            });
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{Repository, SearchResponse},
        query::CaseMode,
        search::SortMode,
    };
    use chrono::Utc;
    use tempfile::tempdir;

    #[test]
    fn commit_sha_changes_the_result_key() {
        let query = QueryFingerprint {
            sources: vec!["foo".into()],
            expression: crate::query::Expr::Atom(0),
            atoms: vec![crate::query::AtomSpec {
                source: "foo".into(),
                kind: crate::query::AtomKind::Wildcard,
                flags: String::new(),
            }],
            case_mode: CaseMode::Smart,
            multiline: false,
        };
        let options = SearchOptions {
            sort: SortMode::Relevance,
            ..Default::default()
        };
        let repository = Repository {
            uuid: "1".into(),
            workspace: "w".into(),
            slug: "r".into(),
            name: "r".into(),
            full_name: "w/r".into(),
            default_branch: Some("main".into()),
            clone_url: String::new(),
            web_url: String::new(),
        };
        let snapshot = |commit: &str| Snapshot {
            repository: repository.clone(),
            branch: "main".into(),
            commit: commit.into(),
            synchronized_at: Utc::now(),
            checkout: PathBuf::new(),
            stale: false,
        };
        assert_ne!(
            result_key(&query, &options, &[snapshot("one")]).unwrap(),
            result_key(&query, &options, &[snapshot("two")]).unwrap()
        );
    }

    #[test]
    fn compressed_results_round_trip() {
        let temp = tempdir().unwrap();
        let config = Config {
            cache_dir: temp.path().into(),
            config_dir: temp.path().join("config"),
            ..Default::default()
        };
        config.ensure_dirs().unwrap();
        let response = SearchResponse {
            query: vec!["foo".into()],
            results: vec![],
            repositories_searched: 1,
            files_searched: 2,
            elapsed_ms: 3,
            cached: false,
            truncated: false,
            skipped: Vec::new(),
        };
        save_result(&config, "key", &response).unwrap();
        let loaded = load_result(&config, "key").unwrap().unwrap();
        assert!(loaded.cached);
        assert_eq!(loaded.files_searched, 2);
    }
}
