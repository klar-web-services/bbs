use crate::{
    config::Config,
    git_sync,
    model::{Repository, RepositoryCatalog, Snapshot},
    query::QueryFingerprint,
    search::{CachedScan, ScanOptions},
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

/// Bumped when the stored body changes shape. Entries under an older schema
/// simply become unreachable and are reclaimed by `bbs cache prune`.
const CACHE_SCHEMA: u32 = 2;

/// The key covers the query, what was scanned, and the exact commits scanned -
/// and nothing about how the results were displayed. `--sort`, `--max-results`
/// and `--context` are applied when rendering, so re-asking for the same scan
/// in a different order is a hit rather than a rescan of every file.
#[derive(Serialize)]
struct CacheKey<'a> {
    schema: u32,
    query: &'a QueryFingerprint,
    scan: &'a ScanOptions,
    snapshots: Vec<(&'a str, &'a str, &'a str)>,
}

pub fn result_key(
    query: &QueryFingerprint,
    scan: &ScanOptions,
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
        scan,
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

pub fn load_result(config: &Config, key: &str) -> Result<Option<CachedScan>> {
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
    match serde_json::from_slice::<CachedScan>(&bytes) {
        Ok(scan) => {
            if let Ok(file) = fs::OpenOptions::new().write(true).open(&path) {
                let _ = file.set_modified(SystemTime::now());
            }
            Ok(Some(scan))
        }
        Err(_) => {
            let _ = fs::remove_file(path);
            Ok(None)
        }
    }
}

pub fn save_result(config: &Config, key: &str, scan: &CachedScan) -> Result<()> {
    fs::create_dir_all(config.results_dir())?;
    let target = result_path(config, key);
    let temp = tempfile::NamedTempFile::new_in(config.results_dir())?;
    {
        let mut encoder = zstd::Encoder::new(temp.as_file(), 3)?;
        encoder.write_all(&serde_json::to_vec(scan)?)?;
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
            "no cached repository catalog at {}; run `bbs repos` online to build it",
            path.display()
        )
    })?;
    // The catalog is derived data: an online run rebuilds it silently, so the
    // message has to say so rather than reading like a corrupt database.
    serde_json::from_slice(&bytes)
        .context("cached repository catalog is corrupt; run `bbs repos` online to rebuild it")
}

/// Reconstructs a catalog from the snapshots on disk.
///
/// A corrupt or missing catalog used to make `--offline` fail outright, even
/// though every repository it would have named was sitting in the cache with
/// its own metadata beside it.
pub fn rebuild_catalog_from_snapshots(config: &Config) -> Result<RepositoryCatalog> {
    let mut repositories: Vec<Repository> = snapshot_entries(config)?
        .iter()
        .filter_map(|entry| git_sync::read_meta(&entry.path))
        .map(|meta| meta.repository)
        .collect();
    repositories.sort_by(|a, b| a.full_name.cmp(&b.full_name));
    repositories.dedup_by(|a, b| a.uuid == b.uuid);
    if repositories.is_empty() {
        anyhow::bail!(
            "no repository catalog and no described snapshots to rebuild one from; run `bbs repos` online"
        );
    }
    Ok(RepositoryCatalog {
        discovered_at: Utc::now(),
        workspaces: vec![],
        repositories,
    })
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Vec<SnapshotDetail>>,
}

/// One snapshot, described. A snapshot with no metadata yet is listed with
/// what can still be read from it rather than omitted, because an unidentified
/// directory taking up disk is exactly what a user needs to see.
#[derive(Debug, serde::Serialize)]
pub struct SnapshotDetail {
    pub repository: Option<String>,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub synced_at: Option<DateTime<Utc>>,
    pub age_seconds: Option<i64>,
    pub bytes: u64,
    pub path: String,
}

pub fn status(config: &Config, verbose: bool) -> Result<CacheStatus> {
    let (snapshot_bytes, _) = tree_stats(&config.snapshots_dir())?;
    let entries = snapshot_entries(config)?;
    let (result_bytes, result_entries) = tree_stats(&config.results_dir())?;
    let details = verbose.then(|| {
        let now = Utc::now();
        entries
            .iter()
            .map(|entry| {
                let meta = git_sync::read_meta(&entry.path);
                SnapshotDetail {
                    repository: meta.as_ref().map(|m| m.repository.full_name.clone()),
                    branch: meta.as_ref().map(|m| m.branch.clone()),
                    commit: meta.as_ref().map(|m| m.commit.clone()),
                    synced_at: meta.as_ref().map(|m| m.synced_at),
                    age_seconds: meta
                        .as_ref()
                        .map(|m| now.signed_duration_since(m.synced_at).num_seconds()),
                    bytes: entry.size,
                    path: entry.path.display().to_string(),
                }
            })
            .collect()
    });
    Ok(CacheStatus {
        snapshot_bytes,
        result_bytes,
        snapshots: entries.len(),
        result_entries,
        details,
    })
}

/// Drops every snapshot of one repository, on every branch.
///
/// Clearing the whole cache to recover from one bad snapshot meant refetching
/// everything; the alternative was finding a directory named by two opaque
/// hashes. Result entries are keyed on commit SHAs and can never be served for
/// a repository with no snapshot, so they are left to the ordinary budget.
pub fn forget(config: &Config, repository: &Repository) -> Result<u64> {
    let root = config
        .snapshots_dir()
        .join(git_sync::repository_component(&repository.uuid));
    if !root.exists() {
        return Ok(0);
    }
    config.validate_cache_target(&root)?;
    let (bytes, _) = tree_stats(&root)?;
    fs::remove_dir_all(&root)?;
    Ok(bytes)
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
        for marker in [
            entry.path.with_extension("used"),
            git_sync::meta_path(&entry.path),
        ] {
            if marker.exists() {
                let _ = fs::remove_file(marker);
            }
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
        model::{Repository, SkippedFiles},
        search::{Presentation, SortMode},
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
            options: crate::query::QueryOptions::default(),
        };
        let scan = ScanOptions::default();
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
            result_key(&query, &scan, &[snapshot("one")]).unwrap(),
            result_key(&query, &scan, &[snapshot("two")]).unwrap()
        );

        // The whole point of the split: how results are displayed must not
        // change the key, or re-running a query with a different `--sort`
        // rescans every file to reach the same answer in a different order.
        let widened = ScanOptions {
            paths: vec!["src/**".into()],
            ..Default::default()
        };
        assert_ne!(
            result_key(&query, &scan, &[snapshot("one")]).unwrap(),
            result_key(&query, &widened, &[snapshot("one")]).unwrap()
        );
    }

    /// The catalog is derived data. Refusing to search offline because it is
    /// corrupt, while the snapshots it would have named sit in the cache
    /// describing themselves, is a refusal with a way out that was not taken.
    #[test]
    fn a_corrupt_catalog_is_rebuilt_from_the_snapshots_on_disk() {
        let temp = tempdir().unwrap();
        let config = Config {
            cache_dir: temp.path().join("cache"),
            config_dir: temp.path().join("config"),
            ..Default::default()
        };
        config.ensure_dirs().unwrap();
        let repository = Repository {
            uuid: "{rebuilt}".into(),
            workspace: "team".into(),
            slug: "api".into(),
            name: "API".into(),
            full_name: "team/api".into(),
            default_branch: Some("main".into()),
            clone_url: String::new(),
            web_url: "https://example.invalid/team/api".into(),
        };

        // no snapshots at all: the error must still point at the way out
        let error = rebuild_catalog_from_snapshots(&config)
            .unwrap_err()
            .to_string();
        assert!(error.contains("bbs repos"), "{error}");

        // a described snapshot is enough to rebuild from
        let checkout = git_sync::snapshot_path(&config, &repository, "main");
        fs::create_dir_all(checkout.join(".git")).unwrap();
        fs::write(
            git_sync::meta_path(&checkout),
            serde_json::to_vec(&crate::model::SnapshotMeta {
                repository: repository.clone(),
                branch: "main".into(),
                commit: "0a1b2c3d".into(),
                synced_at: Utc::now(),
            })
            .unwrap(),
        )
        .unwrap();
        let rebuilt = rebuild_catalog_from_snapshots(&config).unwrap();
        assert_eq!(rebuilt.repositories.len(), 1);
        // the web URL survives, so permalinks still work
        assert_eq!(
            rebuilt.repositories[0].web_url,
            "https://example.invalid/team/api"
        );

        // and the corruption message names the fix
        fs::write(config.catalog_path(), b"{not json").unwrap();
        let error = load_catalog(&config).unwrap_err().to_string();
        assert!(
            error.contains("run `bbs repos` online to rebuild it"),
            "{error}"
        );
    }

    /// Recovering from one bad snapshot used to mean clearing the whole cache
    /// and refetching everything.
    #[test]
    fn forget_drops_one_repository_and_leaves_the_rest() {
        let temp = tempdir().unwrap();
        let config = Config {
            cache_dir: temp.path().join("cache"),
            config_dir: temp.path().join("config"),
            ..Default::default()
        };
        config.ensure_dirs().unwrap();
        let make = |uuid: &str, slug: &str| Repository {
            uuid: uuid.into(),
            workspace: "team".into(),
            slug: slug.into(),
            name: slug.into(),
            full_name: format!("team/{slug}"),
            default_branch: Some("main".into()),
            clone_url: String::new(),
            web_url: String::new(),
        };
        let doomed = make("{doomed}", "doomed");
        let kept = make("{kept}", "kept");
        for (repository, branch) in [(&doomed, "main"), (&doomed, "release"), (&kept, "main")] {
            let checkout = git_sync::snapshot_path(&config, repository, branch);
            fs::create_dir_all(checkout.join(".git")).unwrap();
            fs::write(checkout.join("file.txt"), "x".repeat(64)).unwrap();
        }

        let freed = forget(&config, &doomed).unwrap();
        assert!(freed >= 128, "both branches should be reclaimed: {freed}");
        assert!(!git_sync::snapshot_path(&config, &doomed, "main").exists());
        assert!(!git_sync::snapshot_path(&config, &doomed, "release").exists());
        assert!(git_sync::snapshot_path(&config, &kept, "main").exists());

        // forgetting something never cached is not an error
        assert_eq!(forget(&config, &make("{absent}", "absent")).unwrap(), 0);
    }

    #[test]
    fn verbose_status_identifies_each_snapshot() {
        let temp = tempdir().unwrap();
        let config = Config {
            cache_dir: temp.path().join("cache"),
            config_dir: temp.path().join("config"),
            ..Default::default()
        };
        config.ensure_dirs().unwrap();
        let repository = Repository {
            uuid: "{listed}".into(),
            workspace: "team".into(),
            slug: "api".into(),
            name: "API".into(),
            full_name: "team/api".into(),
            default_branch: Some("main".into()),
            clone_url: String::new(),
            web_url: String::new(),
        };
        let checkout = git_sync::snapshot_path(&config, &repository, "main");
        fs::create_dir_all(checkout.join(".git")).unwrap();
        fs::write(
            git_sync::meta_path(&checkout),
            serde_json::to_vec(&crate::model::SnapshotMeta {
                repository: repository.clone(),
                branch: "main".into(),
                commit: "0a1b2c3d".into(),
                synced_at: Utc::now(),
            })
            .unwrap(),
        )
        .unwrap();

        assert!(status(&config, false).unwrap().details.is_none());
        let details = status(&config, true).unwrap().details.unwrap();
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].repository.as_deref(), Some("team/api"));
        assert_eq!(details[0].branch.as_deref(), Some("main"));
        assert_eq!(details[0].commit.as_deref(), Some("0a1b2c3d"));
        assert!(details[0].age_seconds.unwrap() >= 0);
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
        let scan = CachedScan {
            results: vec![],
            total_results: 0,
            stored_context: 6,
            stored_limit: 1000,
            complete: true,
            stored_sort: SortMode::Relevance,
            repositories_searched: 1,
            files_searched: 2,
            skipped_files: SkippedFiles::default(),
            matches_capped_files: 0,
            pattern_gave_up_files: 0,
            scan_ms: 3,
        };
        save_result(&config, "key", &scan).unwrap();
        let loaded = load_result(&config, "key").unwrap().unwrap();
        assert_eq!(loaded.files_searched, 2);
        // a complete body answers any narrower presentation
        assert!(loaded.satisfies(&Presentation {
            sort: SortMode::Path,
            max_results: 500,
            context: 2,
        }));
        // but not one asking for more context than was stored
        assert!(!loaded.satisfies(&Presentation {
            sort: SortMode::Relevance,
            max_results: 500,
            context: 20,
        }));
    }
}
