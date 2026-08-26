use crate::{
    config::Config,
    model::{Repository, Snapshot},
};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use fs2::FileExt;
use git2::{
    Cred, FetchOptions, RemoteCallbacks, Repository as GitRepository,
    build::{CheckoutBuilder, RepoBuilder},
};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

pub struct SnapshotLock {
    _file: File,
}

pub struct SearchLock {
    _file: File,
}

pub fn lock_searches(config: &Config) -> Result<SearchLock> {
    fs::create_dir_all(&config.cache_dir)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config.cache_dir.join("search.lock"))?;
    file.lock_exclusive()?;
    Ok(SearchLock { _file: file })
}

fn validate_branch(branch: &str) -> Result<()> {
    anyhow::ensure!(
        !branch.is_empty() && git2::Reference::is_valid_name(&format!("refs/heads/{branch}")),
        "invalid Git branch name `{branch}`"
    );
    Ok(())
}

fn mark_used(path: &Path) {
    let marker = path.with_extension("used");
    if let Ok(file) = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(marker)
    {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
}

fn component_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex::encode(&digest[..12])
}

pub fn snapshot_path(config: &Config, repository: &Repository, branch: &str) -> PathBuf {
    config
        .snapshots_dir()
        .join(component_hash(&repository.uuid))
        .join(component_hash(branch))
}

fn lock_snapshot(path: &Path) -> Result<SnapshotLock> {
    let parent = path.parent().context("snapshot path has no parent")?;
    fs::create_dir_all(parent)?;
    let lock_path = parent.join(format!(
        "{}.lock",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    file.lock_exclusive()?;
    Ok(SnapshotLock { _file: file })
}

fn callbacks<'a>(token: &'a str) -> RemoteCallbacks<'a> {
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(move |_url, _username, _allowed| {
        Cred::userpass_plaintext("x-bitbucket-api-token-auth", token)
    });
    callbacks
}

fn fetch_options(token: &str, shallow: bool) -> FetchOptions<'_> {
    let mut options = FetchOptions::new();
    if shallow {
        options.depth(1);
    }
    options.remote_callbacks(callbacks(token));
    options
}

pub fn synchronize(
    config: &Config,
    repository: &Repository,
    branch: &str,
    token: &str,
) -> Result<Snapshot> {
    validate_branch(branch)?;
    let checkout = snapshot_path(config, repository, branch);
    let _lock = lock_snapshot(&checkout)?;
    if !checkout.exists() {
        fs::create_dir_all(checkout.parent().context("invalid snapshot path")?)?;
        let mut builder = RepoBuilder::new();
        builder.branch(branch);
        let shallow = repository.clone_url.starts_with("http://")
            || repository.clone_url.starts_with("https://");
        builder.fetch_options(fetch_options(token, shallow));
        if let Err(error) = builder.clone(&repository.clone_url, &checkout) {
            if checkout.exists() {
                config.validate_cache_target(&checkout)?;
                let _ = fs::remove_dir_all(&checkout);
            }
            bail!(
                "failed to clone {} branch {}: {error}",
                repository.full_name,
                branch
            );
        }
    }

    let repo = GitRepository::open(&checkout)
        .with_context(|| format!("invalid cached checkout for {}", repository.full_name))?;
    {
        let mut remote = repo.find_remote("origin")?;
        let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
        let shallow = repository.clone_url.starts_with("http://")
            || repository.clone_url.starts_with("https://");
        let mut options = fetch_options(token, shallow);
        remote
            .fetch(&[&refspec], Some(&mut options), None)
            .with_context(|| {
                format!(
                    "failed to update {} branch {}",
                    repository.full_name, branch
                )
            })?;
    }
    let reference = repo.find_reference(&format!("refs/remotes/origin/{branch}"))?;
    let commit = reference
        .target()
        .context("fetched branch does not point to a commit")?;
    let object = repo.find_object(commit, None)?;
    repo.checkout_tree(
        &object,
        Some(CheckoutBuilder::new().force().remove_untracked(true)),
    )?;
    repo.set_head_detached(commit)?;
    mark_used(&checkout);

    Ok(Snapshot {
        repository: repository.clone(),
        branch: branch.into(),
        commit: commit.to_string(),
        synchronized_at: Utc::now(),
        checkout,
        stale: false,
    })
}

pub fn load_offline(config: &Config, repository: &Repository, branch: &str) -> Result<Snapshot> {
    validate_branch(branch)?;
    let checkout = snapshot_path(config, repository, branch);
    let _lock = lock_snapshot(&checkout)?;
    let repo = GitRepository::open(&checkout).with_context(|| {
        format!(
            "no cached snapshot for {} branch {}; run an online search first",
            repository.full_name, branch
        )
    })?;
    let commit = repo.head()?.peel_to_commit()?.id().to_string();
    let synchronized_at = fs::metadata(checkout.join(".git/FETCH_HEAD"))
        .and_then(|m| m.modified())
        .map(chrono::DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now());
    mark_used(&checkout);
    Ok(Snapshot {
        repository: repository.clone(),
        branch: branch.into(),
        commit,
        synchronized_at,
        checkout,
        stale: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{IndexAddOption, Signature};
    use tempfile::tempdir;

    fn commit(repo: &GitRepository, text: &str) -> git2::Oid {
        fs::write(repo.workdir().unwrap().join("source.rs"), text).unwrap();
        let mut index = repo.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let signature = Signature::now("bbs test", "bbs@example.invalid").unwrap();
        let parents = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .and_then(|id| repo.find_commit(id).ok())
            .into_iter()
            .collect::<Vec<_>>();
        repo.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "test commit",
            &tree,
            &parents.iter().collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn clone_fetch_and_offline_snapshot_track_commits() {
        let temp = tempdir().unwrap();
        let remote_path = temp.path().join("remote");
        let remote = GitRepository::init(&remote_path).unwrap();
        let first = commit(&remote, "fn first() {}\n");
        remote.set_head("refs/heads/main").unwrap();
        let config = Config {
            cache_dir: temp.path().join("cache"),
            config_dir: temp.path().join("config"),
            ..Default::default()
        };
        config.ensure_dirs().unwrap();
        let repository = Repository {
            uuid: "{fixture}".into(),
            workspace: "test".into(),
            slug: "fixture".into(),
            name: "fixture".into(),
            full_name: "test/fixture".into(),
            default_branch: Some("main".into()),
            clone_url: remote_path.to_string_lossy().into_owned(),
            web_url: "https://example.invalid/test/fixture".into(),
        };
        let snapshot = synchronize(&config, &repository, "main", "unused").unwrap();
        assert_eq!(snapshot.commit, first.to_string());
        assert!(snapshot.checkout.join("source.rs").exists());
        let second = commit(&remote, "fn second() {}\n");
        let updated = synchronize(&config, &repository, "main", "unused").unwrap();
        assert_eq!(updated.commit, second.to_string());
        assert_eq!(
            fs::read_to_string(updated.checkout.join("source.rs"))
                .unwrap()
                .replace("\r\n", "\n"),
            "fn second() {}\n"
        );
        let offline = load_offline(&config, &repository, "main").unwrap();
        assert!(offline.stale);
        assert_eq!(offline.commit, second.to_string());
    }

    #[test]
    fn invalid_branch_is_rejected_before_filesystem_use() {
        let config = Config::default();
        let repository = Repository {
            uuid: "1".into(),
            workspace: "w".into(),
            slug: "r".into(),
            name: "r".into(),
            full_name: "w/r".into(),
            default_branch: None,
            clone_url: String::new(),
            web_url: String::new(),
        };
        assert!(
            load_offline(&config, &repository, "../bad:ref")
                .unwrap_err()
                .to_string()
                .contains("invalid Git branch")
        );
    }
}
