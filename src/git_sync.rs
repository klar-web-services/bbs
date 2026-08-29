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

/// The result of preparing one repository for a search. A repository that
/// simply has nothing to offer on the requested branch is `Unavailable` rather
/// than an error, so one empty or differently-branched repository cannot fail a
/// search across all the others.
#[derive(Debug)]
pub enum Sync {
    /// Boxed because a `Snapshot` carries the whole `Repository` record, which
    /// would otherwise make every `Unavailable` cost as much as a ready one.
    Ready(Box<Snapshot>),
    Unavailable(String),
}

impl Sync {
    /// Unwraps a snapshot that the caller knows must be present. Test-only
    /// convenience; production code matches on the variants.
    #[cfg(test)]
    pub fn snapshot(self) -> Snapshot {
        match self {
            Sync::Ready(snapshot) => *snapshot,
            Sync::Unavailable(reason) => panic!("expected a snapshot, got: {reason}"),
        }
    }
}

/// libgit2 reports a missing branch, and an entirely empty remote, as a
/// not-found reference. Anything else (auth, transport, corruption) is a real
/// failure and must still stop the search.
fn missing_reference(error: &git2::Error) -> bool {
    error.code() == git2::ErrorCode::NotFound
}

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

/// Clones `repository` into `checkout`. `Ok(Some(..))` means the repository
/// has nothing to offer on this branch; `Ok(None)` means the clone succeeded.
fn clone_snapshot(
    config: &Config,
    repository: &Repository,
    branch: &str,
    token: &str,
    checkout: &Path,
) -> Result<Option<Sync>> {
    fs::create_dir_all(checkout.parent().context("invalid snapshot path")?)?;
    let mut builder = RepoBuilder::new();
    builder.branch(branch);
    let shallow =
        repository.clone_url.starts_with("http://") || repository.clone_url.starts_with("https://");
    builder.fetch_options(fetch_options(token, shallow));
    if let Err(error) = builder.clone(&repository.clone_url, checkout) {
        if checkout.exists() {
            config.validate_cache_target(checkout)?;
            let _ = fs::remove_dir_all(checkout);
        }
        if missing_reference(&error) {
            return Ok(Some(Sync::Unavailable(format!(
                "no branch `{branch}`; the repository may be empty"
            ))));
        }
        bail!(
            "failed to clone {} branch {}: {error}",
            repository.full_name,
            branch
        );
    }
    Ok(None)
}

pub fn synchronize(
    config: &Config,
    repository: &Repository,
    branch: &str,
    token: &str,
) -> Result<Sync> {
    validate_branch(branch)?;
    let checkout = snapshot_path(config, repository, branch);
    let _lock = lock_snapshot(&checkout)?;
    if !checkout.exists()
        && let Some(unavailable) = clone_snapshot(config, repository, branch, token, &checkout)?
    {
        return Ok(unavailable);
    }

    // A snapshot directory that is no longer a usable Git repository - an
    // interrupted clone, a truncated write, a half-deleted cache - is
    // discarded and fetched again. It is derived data behind an opaque pair of
    // hashes, so leaving the user to find and delete it by hand would wedge
    // every later search.
    let repo = match GitRepository::open(&checkout) {
        Ok(repo) => repo,
        Err(_) => {
            config.validate_cache_target(&checkout)?;
            fs::remove_dir_all(&checkout).with_context(|| {
                format!(
                    "cannot discard the damaged snapshot for {} at {}",
                    repository.full_name,
                    checkout.display()
                )
            })?;
            if let Some(unavailable) = clone_snapshot(config, repository, branch, token, &checkout)?
            {
                return Ok(unavailable);
            }
            GitRepository::open(&checkout)
                .with_context(|| format!("invalid cached checkout for {}", repository.full_name))?
        }
    };
    {
        let mut remote = repo.find_remote("origin")?;
        let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
        let shallow = repository.clone_url.starts_with("http://")
            || repository.clone_url.starts_with("https://");
        let mut options = fetch_options(token, shallow);
        if let Err(error) = remote.fetch(&[&refspec], Some(&mut options), None) {
            if missing_reference(&error) {
                return Ok(Sync::Unavailable(format!(
                    "no branch `{branch}` on the remote"
                )));
            }
            return Err(anyhow::Error::new(error).context(format!(
                "failed to update {} branch {}",
                repository.full_name, branch
            )));
        }
    }
    let reference = match repo.find_reference(&format!("refs/remotes/origin/{branch}")) {
        Ok(reference) => reference,
        Err(error) if missing_reference(&error) => {
            return Ok(Sync::Unavailable(format!(
                "no branch `{branch}` after fetch"
            )));
        }
        Err(error) => return Err(error.into()),
    };
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

    Ok(Sync::Ready(Box::new(Snapshot {
        repository: repository.clone(),
        branch: branch.into(),
        commit: commit.to_string(),
        synchronized_at: Utc::now(),
        checkout,
        stale: false,
    })))
}

pub fn load_offline(config: &Config, repository: &Repository, branch: &str) -> Result<Sync> {
    validate_branch(branch)?;
    let checkout = snapshot_path(config, repository, branch);
    let _lock = lock_snapshot(&checkout)?;
    let repo = match GitRepository::open(&checkout) {
        Ok(repo) => repo,
        Err(_) => {
            return Ok(Sync::Unavailable(format!(
                "no cached snapshot for branch `{branch}`; run an online search first"
            )));
        }
    };
    let commit = match repo.head().and_then(|head| head.peel_to_commit()) {
        Ok(commit) => commit.id().to_string(),
        Err(_) => {
            return Ok(Sync::Unavailable(format!(
                "cached snapshot for branch `{branch}` has no commit"
            )));
        }
    };
    let synchronized_at = fs::metadata(checkout.join(".git/FETCH_HEAD"))
        .and_then(|m| m.modified())
        .map(chrono::DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now());
    mark_used(&checkout);
    Ok(Sync::Ready(Box::new(Snapshot {
        repository: repository.clone(),
        branch: branch.into(),
        commit,
        synchronized_at,
        checkout,
        stale: true,
    })))
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
        let snapshot = synchronize(&config, &repository, "main", "unused")
            .unwrap()
            .snapshot();
        assert_eq!(snapshot.commit, first.to_string());
        assert!(snapshot.checkout.join("source.rs").exists());
        let second = commit(&remote, "fn second() {}\n");
        let updated = synchronize(&config, &repository, "main", "unused")
            .unwrap()
            .snapshot();
        assert_eq!(updated.commit, second.to_string());
        assert_eq!(
            fs::read_to_string(updated.checkout.join("source.rs"))
                .unwrap()
                .replace("\r\n", "\n"),
            "fn second() {}\n"
        );
        let offline = load_offline(&config, &repository, "main")
            .unwrap()
            .snapshot();
        assert!(offline.stale);
        assert_eq!(offline.commit, second.to_string());
    }

    /// An empty remote, and a branch that does not exist, must be reported as
    /// unavailable rather than as an error: one such repository in a workspace
    /// used to fail every search across all the others.
    #[test]
    fn empty_remote_and_missing_branch_are_unavailable_not_errors() {
        let temp = tempdir().unwrap();
        let remote_path = temp.path().join("remote");
        GitRepository::init(&remote_path).unwrap();
        let config = Config {
            cache_dir: temp.path().join("cache"),
            config_dir: temp.path().join("config"),
            ..Default::default()
        };
        config.ensure_dirs().unwrap();
        let repository = Repository {
            uuid: "{empty}".into(),
            workspace: "test".into(),
            slug: "empty".into(),
            name: "empty".into(),
            full_name: "test/empty".into(),
            default_branch: Some("main".into()),
            clone_url: remote_path.to_string_lossy().into_owned(),
            web_url: "https://example.invalid/test/empty".into(),
        };
        assert!(matches!(
            synchronize(&config, &repository, "main", "unused").unwrap(),
            Sync::Unavailable(_)
        ));

        let populated = GitRepository::open(&remote_path).unwrap();
        commit(&populated, "fn only_on_main() {}\n");
        populated.set_head("refs/heads/main").unwrap();
        assert!(matches!(
            synchronize(&config, &repository, "main", "unused").unwrap(),
            Sync::Ready(_)
        ));
        assert!(matches!(
            synchronize(&config, &repository, "release/2.x", "unused").unwrap(),
            Sync::Unavailable(_)
        ));
    }

    /// A snapshot whose `.git` is gone must be re-cloned rather than reported
    /// as a permanently broken checkout.
    #[test]
    fn a_damaged_snapshot_is_discarded_and_refetched() {
        let temp = tempdir().unwrap();
        let remote_path = temp.path().join("remote");
        let remote = GitRepository::init(&remote_path).unwrap();
        commit(&remote, "fn wanted() {}\n");
        remote.set_head("refs/heads/main").unwrap();
        let config = Config {
            cache_dir: temp.path().join("cache"),
            config_dir: temp.path().join("config"),
            ..Default::default()
        };
        config.ensure_dirs().unwrap();
        let repository = Repository {
            uuid: "{damaged}".into(),
            workspace: "test".into(),
            slug: "damaged".into(),
            name: "damaged".into(),
            full_name: "test/damaged".into(),
            default_branch: Some("main".into()),
            clone_url: remote_path.to_string_lossy().into_owned(),
            web_url: "https://example.invalid/test/damaged".into(),
        };
        let first = synchronize(&config, &repository, "main", "unused")
            .unwrap()
            .snapshot();
        assert!(first.checkout.join("source.rs").exists());

        fs::remove_dir_all(first.checkout.join(".git")).unwrap();
        let healed = synchronize(&config, &repository, "main", "unused")
            .unwrap()
            .snapshot();
        assert_eq!(healed.commit, first.commit);
        assert!(healed.checkout.join("source.rs").exists());
    }

    /// Offline mode must also degrade per repository: a repository that was
    /// never synced is skipped, not fatal.
    #[test]
    fn offline_without_a_snapshot_is_unavailable() {
        let temp = tempdir().unwrap();
        let config = Config {
            cache_dir: temp.path().join("cache"),
            config_dir: temp.path().join("config"),
            ..Default::default()
        };
        config.ensure_dirs().unwrap();
        let repository = Repository {
            uuid: "{never-synced}".into(),
            workspace: "test".into(),
            slug: "never".into(),
            name: "never".into(),
            full_name: "test/never".into(),
            default_branch: Some("main".into()),
            clone_url: String::new(),
            web_url: String::new(),
        };
        assert!(matches!(
            load_offline(&config, &repository, "main").unwrap(),
            Sync::Unavailable(_)
        ));
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
