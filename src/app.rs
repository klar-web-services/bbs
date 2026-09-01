use crate::{
    auth::{self, Credentials},
    bitbucket::{self, BitbucketClient},
    cache,
    config::Config,
    git_sync,
    model::{
        Repository, RepositoryCatalog, SearchEvent, SearchResponse, SkippedRepository, Snapshot,
    },
    query::{CaseMode, CompiledQuery, QueryOptions},
    search::{self, Presentation, ScanOptions as SearchOptions, SortMode},
};
use anyhow::{Context, Result, bail};
use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, atomic::AtomicBool};

pub type Progress = Arc<dyn Fn(SearchEvent) + Send + Sync>;

/// An error and its causes on the one line a skipped repository is given.
fn one_line(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(": ")
}

/// The single error to raise when not one repository could be prepared.
///
/// Every failure is collected rather than raised, so a problem that is really
/// account-wide -- an expired credential, no network -- would otherwise report
/// one repository per line.
fn nothing_prepared(action: &str, skipped: &[SkippedRepository]) -> anyhow::Error {
    const NAMED: usize = 5;
    let mut detail = skipped
        .iter()
        .take(NAMED)
        .map(|s| format!("{} ({})", s.repository, s.reason))
        .collect::<Vec<_>>()
        .join(", ");
    if let Some(rest) = skipped.len().checked_sub(NAMED).filter(|rest| *rest > 0) {
        detail.push_str(&format!(", and {rest} more"));
    }
    anyhow::anyhow!("no repository could be {action}: {detail}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchRequest {
    pub queries: Vec<String>,
    pub repositories: Vec<String>,
    pub paths: Vec<String>,
    /// Path globs to remove from the search. `--path '!glob'` lands here too.
    pub exclude_paths: Vec<String>,
    /// Shorthand for excluding the directories the ranking already demotes.
    pub no_vendor: bool,
    pub branch: Option<String>,
    pub regex: bool,
    pub case_mode: CaseMode,
    pub multiline: bool,
    /// Require a word boundary either side of every term.
    pub word: bool,
    pub offline: bool,
    /// Reuse any snapshot fetched within this many seconds instead of
    /// fetching it again. `None` always fetches.
    pub max_age_seconds: Option<u64>,
    pub context: Option<usize>,
    pub max_results: Option<usize>,
    pub sort: SortMode,
    pub no_cache: bool,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            queries: vec![],
            repositories: vec![],
            paths: vec![],
            exclude_paths: vec![],
            no_vendor: false,
            branch: None,
            regex: false,
            case_mode: CaseMode::Smart,
            multiline: false,
            word: false,
            offline: false,
            max_age_seconds: None,
            context: None,
            max_results: None,
            sort: SortMode::Relevance,
            no_cache: false,
        }
    }
}

/// A `bbs warmup`: everything a search does before it looks at a file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WarmupRequest {
    /// Repositories, patterns included. Empty warms every accessible one.
    pub repositories: Vec<String>,
    /// Warm this branch rather than each repository's default branch.
    pub branch: Option<String>,
    /// Leave alone any snapshot fetched within this many seconds, so a
    /// repeated warmup pays only for what has gone stale.
    pub max_age_seconds: Option<u64>,
    /// Repositories to fetch at once. `None` uses `sync_concurrency`.
    pub concurrency: Option<usize>,
}

/// What one warmup did. The counts are what a scheduled warmup needs to tell
/// a slow network from a shrinking workspace.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WarmupReport {
    /// Repositories selected, before any of them failed.
    pub repositories: usize,
    /// Snapshots ready on disk afterwards: `fetched` plus `reused`.
    pub warmed: usize,
    /// Snapshots this run cloned or fetched from the remote.
    pub fetched: usize,
    /// Snapshots already inside the freshness window, left untouched.
    pub reused: usize,
    pub skipped: Vec<SkippedRepository>,
    /// Bytes the snapshot cache occupies afterwards.
    pub snapshot_bytes: u64,
    pub elapsed_ms: u128,
}

#[derive(Clone)]
pub struct BbsApp {
    pub config: Config,
    /// Overrides the credential store.
    ///
    /// Without it there was no seam for testing the online path at all:
    /// discovery already honours `config.api_base`, but the token could only
    /// come from the process environment or the OS keyring, both of which are
    /// global and cannot be set safely from a parallel test. An "online" test
    /// therefore hit the real network.
    credentials: Option<Credentials>,
    /// `--env-token`: present `BB_TOKEN` ahead of the saved credential.
    prefer_env_token: bool,
}

impl BbsApp {
    pub fn new(config: Config) -> Result<Self> {
        config.ensure_dirs()?;
        Ok(Self {
            config,
            credentials: None,
            prefer_env_token: false,
        })
    }

    /// Presents `BB_TOKEN` before the saved credential, for `--env-token`.
    /// The saved credential stays on as a fallback, so the flag reorders the
    /// two rather than discarding one.
    pub fn preferring_env_token(mut self, prefer: bool) -> Self {
        self.prefer_env_token = prefer;
        self
    }

    /// Builds an application that authenticates with `token` rather than
    /// consulting the environment or the credential store.
    pub fn with_token(config: Config, token: impl Into<String>) -> Result<Self> {
        config.ensure_dirs()?;
        Ok(Self {
            config,
            credentials: Some(Credentials::supplied(token)),
            prefer_env_token: false,
        })
    }

    fn credentials(&self) -> Result<Credentials> {
        match &self.credentials {
            Some(credentials) => Ok(credentials.clone()),
            None => auth::credentials(self.prefer_env_token),
        }
    }

    /// Discovers the accessible repositories, or reuses a recent discovery.
    ///
    /// `max_age` covers the catalog as well as the snapshots: back-to-back
    /// queries otherwise pay for a full workspace and repository walk of the
    /// Bitbucket API every time, which on a seventy-repository account is a
    /// second of latency before any file is looked at.
    pub async fn catalog(
        &self,
        offline: bool,
        max_age: Option<std::time::Duration>,
    ) -> Result<RepositoryCatalog> {
        if let Some(max_age) = max_age
            && !offline
            && let Ok(cached) = cache::load_catalog(&self.config)
            && chrono::Utc::now()
                .signed_duration_since(cached.discovered_at)
                .to_std()
                .is_ok_and(|age| age <= max_age)
        {
            return Ok(cached);
        }
        if offline {
            // The catalog is a cache. When it is missing or corrupt, the
            // snapshots on disk describe themselves well enough to rebuild
            // one, which beats refusing to search a cache that is right there.
            return match cache::load_catalog(&self.config) {
                Ok(catalog) => Ok(catalog),
                Err(error) => {
                    cache::rebuild_catalog_from_snapshots(&self.config).map_err(|_| error)
                }
            };
        }
        let client = BitbucketClient::new(&self.config.api_base, self.credentials()?)?;
        let catalog = client.discover().await?;
        cache::save_catalog(&self.config, &catalog)?;
        Ok(catalog)
    }

    pub async fn validate_login(&self, token: &str) -> Result<String> {
        let client = BitbucketClient::new(&self.config.api_base, Credentials::supplied(token))?;
        let catalog = client.discover().await?;
        let summary = format!("{} accessible repositories", catalog.repositories.len());
        cache::save_catalog(&self.config, &catalog)?;
        Ok(summary)
    }

    /// Clones or fetches every repository in parallel and reports both what
    /// came back and what could not.
    ///
    /// Shared by `search` and `warmup` so warming cannot drift from what a
    /// search consumes: whatever this leaves on disk is exactly what the next
    /// search finds there. `credentials` of `None` is the offline path, which
    /// reads snapshots already present rather than contacting any remote.
    async fn prepare_snapshots(
        &self,
        repositories: Vec<Repository>,
        branch_override: Option<String>,
        credentials: Option<Credentials>,
        max_age: Option<std::time::Duration>,
        concurrency: usize,
        progress: &Progress,
    ) -> Result<(Vec<Snapshot>, Vec<SkippedRepository>)> {
        let total = repositories.len();
        let config = self.config.clone();
        let progress_sync = progress.clone();
        let concurrency = concurrency.max(1);
        let mut completed = 0usize;
        let jobs = stream::iter(repositories.into_iter().map(|repository| {
            let config = config.clone();
            let credentials = credentials.clone();
            let branch_override = branch_override.clone();
            async move {
                let full_name = repository.full_name.clone();
                let Some(branch) = branch_override.or(repository.default_branch.clone()) else {
                    return Ok((
                        full_name,
                        None,
                        git_sync::Sync::Unavailable("no default branch".into()),
                    ));
                };
                let prepared = tokio::task::spawn_blocking({
                    let branch = branch.clone();
                    move || match credentials {
                        Some(credentials) => git_sync::synchronize(
                            &config,
                            &repository,
                            &branch,
                            &credentials,
                            max_age,
                        ),
                        None => git_sync::load_offline(&config, &repository, &branch),
                    }
                })
                .await
                .context("snapshot task failed")?;
                // A repository that cannot be prepared is named in `skipped`
                // rather than raised. `git_sync` already demotes an empty
                // repository and a missing branch, but a revoked permission, a
                // remote that no longer answers, or an unusable clone URL would
                // otherwise still fail the run for every other repository.
                let outcome =
                    prepared.unwrap_or_else(|error| git_sync::Sync::Unavailable(one_line(&error)));
                anyhow::Ok((full_name, Some(branch), outcome))
            }
        }))
        .buffer_unordered(concurrency);
        tokio::pin!(jobs);
        let mut snapshots: Vec<Snapshot> = Vec::new();
        let mut skipped: Vec<SkippedRepository> = Vec::new();
        while let Some(outcome) = jobs.next().await {
            let (full_name, branch, outcome) = outcome?;
            match outcome {
                git_sync::Sync::Ready(snapshot) => snapshots.push(*snapshot),
                git_sync::Sync::Unavailable(reason) => {
                    (progress_sync)(SearchEvent::Warning {
                        message: format!("skipped {full_name}: {reason}"),
                    });
                    skipped.push(SkippedRepository {
                        repository: full_name,
                        branch,
                        reason,
                    });
                }
            }
            completed += 1;
            (progress_sync)(SearchEvent::Progress {
                phase: "sync".into(),
                message: format!("Synchronized {completed} of {total} repositories"),
                current: completed,
                total,
            });
        }
        skipped.sort_by(|a, b| a.repository.cmp(&b.repository));
        Ok((snapshots, skipped))
    }

    /// Pays a search's setup cost up front.
    ///
    /// The first query against a large workspace spends nearly all its time
    /// discovering repositories and cloning them, which makes an otherwise
    /// fast tool feel slow exactly once per machine -- and again after every
    /// prune. Warming does that half on its own schedule, so the search that
    /// follows starts at the scan. There is no separate index to build: what a
    /// warmed cache saves a later search is precisely its `sync_ms`.
    pub async fn warmup(&self, request: WarmupRequest, progress: Progress) -> Result<WarmupReport> {
        let started = std::time::Instant::now();
        // Warming writes the same snapshots a search reads, so it takes the
        // same lock: a warmup and a search running at once would otherwise
        // fetch the same repository twice.
        let lock_config = self.config.clone();
        let _search_lock =
            tokio::task::spawn_blocking(move || git_sync::lock_searches(&lock_config))
                .await
                .context("search lock task failed")??;
        let max_age = request.max_age_seconds.map(std::time::Duration::from_secs);
        (progress)(SearchEvent::Progress {
            phase: "discovery".into(),
            message: "Discovering accessible repositories".into(),
            current: 0,
            total: 0,
        });
        // Discovery is always refetched, even under `--max-age`: warming from
        // a stale catalog would silently leave out every repository created
        // since, which is the one thing a warmup exists to avoid. The window
        // applies to the fetches, which are where the time goes.
        let catalog = self.catalog(false, None).await?;
        let repositories = bitbucket::resolve_repositories(&catalog, &request.repositories)?;
        if repositories.is_empty() {
            bail!("no accessible repositories were found");
        }
        let selected = repositories.len();
        // Taken before the fetches, so a snapshot still stamped earlier than
        // this is one the freshness window let us leave alone. It is the only
        // honest way to say what the run actually cost.
        let cutoff = chrono::Utc::now();
        let (snapshots, skipped) = self
            .prepare_snapshots(
                repositories,
                request.branch.clone(),
                Some(self.credentials()?),
                max_age,
                request.concurrency.unwrap_or(self.config.sync_concurrency),
                &progress,
            )
            .await?;
        if snapshots.is_empty() {
            return Err(nothing_prepared("warmed", &skipped));
        }
        let reused = snapshots
            .iter()
            .filter(|snapshot| snapshot.synchronized_at < cutoff)
            .count();
        let report = WarmupReport {
            repositories: selected,
            warmed: snapshots.len(),
            fetched: snapshots.len() - reused,
            reused,
            skipped,
            snapshot_bytes: cache::status(&self.config, false)?.snapshot_bytes,
            elapsed_ms: started.elapsed().as_millis(),
        };
        Ok(report)
    }

    pub async fn search(
        &self,
        request: SearchRequest,
        progress: Progress,
        cancelled: Arc<AtomicBool>,
    ) -> Result<SearchResponse> {
        let started = std::time::Instant::now();
        let query = CompiledQuery::parse(
            &request.queries,
            QueryOptions {
                regex: request.regex,
                case_mode: request.case_mode,
                multiline: request.multiline,
                word: request.word,
            },
        )?;
        let query_sources = query.sources.clone();
        let lock_config = self.config.clone();
        let _search_lock =
            tokio::task::spawn_blocking(move || git_sync::lock_searches(&lock_config))
                .await
                .context("search lock task failed")??;
        // A freshness window, rather than the all-or-nothing choice between
        // fetching everything and pretending to be offline.
        let max_age = request.max_age_seconds.map(std::time::Duration::from_secs);
        (progress)(SearchEvent::Progress {
            phase: "discovery".into(),
            message: "Discovering accessible repositories".into(),
            current: 0,
            total: 0,
        });
        let catalog = self.catalog(request.offline, max_age).await?;
        let repositories = bitbucket::resolve_repositories(&catalog, &request.repositories)?;
        if repositories.is_empty() {
            bail!("no accessible repositories were found");
        }
        let credentials = if request.offline {
            None
        } else {
            Some(self.credentials()?)
        };
        let (mut snapshots, skipped) = self
            .prepare_snapshots(
                repositories,
                request.branch.clone(),
                credentials,
                max_age,
                self.config.sync_concurrency,
                &progress,
            )
            .await?;
        if snapshots.is_empty() {
            return Err(nothing_prepared("searched", &skipped));
        }
        snapshots.sort_by(|a, b| a.repository.full_name.cmp(&b.repository.full_name));
        // Fetching seventy repositories and scanning them are very different
        // costs, and one `elapsed_ms` could not tell them apart.
        let sync_ms = started.elapsed().as_millis();
        let scan_options = SearchOptions {
            paths: request.paths,
            exclude_paths: request.exclude_paths,
            no_vendor: request.no_vendor,
            max_file_bytes: self.config.max_file_bytes,
        };
        let presentation = Presentation {
            sort: request.sort,
            max_results: request.max_results.unwrap_or(self.config.max_results),
            context: request.context.unwrap_or(self.config.context_lines),
        };
        // The scan is stored at least as wide as the configured cache window,
        // so a later, narrower request for context or results is answered by
        // trimming rather than by walking every file again.
        let stored = Presentation {
            sort: presentation.sort,
            max_results: presentation.max_results.max(self.config.cache_max_results),
            context: presentation.context.max(self.config.cache_context_lines),
        };
        let key = cache::result_key(&query.normalized(), &scan_options, &snapshots)?;
        let cached = if request.no_cache {
            None
        } else {
            cache::load_result(&self.config, &key)?
        };
        let (scan, cached_hit) = match cached {
            Some(scan) if scan.satisfies(&presentation) => (scan, true),
            _ => {
                (progress)(SearchEvent::Progress {
                    phase: "search".into(),
                    message: "Scanning synchronized snapshots".into(),
                    current: 0,
                    total: snapshots.len(),
                });
                let query = Arc::new(query);
                let scan_for_search = scan_options.clone();
                let stored_for_search = stored;
                let cancelled_for_search = cancelled.clone();
                let snapshots_for_search = snapshots.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    search::run(
                        &query,
                        &snapshots_for_search,
                        &scan_for_search,
                        &stored_for_search,
                        cancelled_for_search,
                    )
                })
                .await
                .context("search task failed")??;
                // A path filter that removed every candidate is nearly always a
                // typo, and silence about it reads as "no matches" rather than
                // "that glob selected nothing".
                if let Some(warning) = outcome.filter.empty_result_warning(&outcome.filter_counts) {
                    (progress)(SearchEvent::Warning { message: warning });
                }
                if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                    bail!("search cancelled");
                }
                if !request.no_cache {
                    cache::save_result(&self.config, &key, &outcome.scan)?;
                }
                (outcome.scan, false)
            }
        };
        let mut response = search::present(&scan, &presentation, &query_sources);
        response.skipped = skipped;
        response.offline = request.offline;
        response.cached = cached_hit;
        response.sync_ms = sync_ms;
        response.scan_ms = if cached_hit { 0 } else { scan.scan_ms };
        response.elapsed_ms = started.elapsed().as_millis();
        // Freshness describes this run, not the run that populated the cache.
        // The cached body is keyed on exact commits so its content is right
        // either way, but an offline hit must not inherit the "verified
        // against the remote" label from an earlier online search, nor the
        // reverse.
        restamp_freshness(&mut response, &snapshots);
        for result in &response.results {
            (progress)(SearchEvent::Result {
                result: result.clone(),
            });
        }
        (progress)(SearchEvent::Done {
            response: response.clone(),
        });
        Ok(response)
    }
}

/// Re-labels cached results with the freshness of the snapshots actually used
/// by this search.
fn restamp_freshness(response: &mut SearchResponse, snapshots: &[Snapshot]) {
    let freshness: std::collections::HashMap<(&str, &str), bool> = snapshots
        .iter()
        .map(|s| {
            (
                (s.repository.full_name.as_str(), s.branch.as_str()),
                s.stale,
            )
        })
        .collect();
    for result in &mut response.results {
        if let Some(stale) = freshness.get(&(result.repository.as_str(), result.branch.as_str())) {
            result.stale = *stale;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Repository, RepositoryCatalog};
    use chrono::Utc;
    use git2::{IndexAddOption, Repository as GitRepository, Signature};
    use std::{fs, sync::atomic::AtomicBool};
    use tempfile::tempdir;

    #[tokio::test]
    async fn offline_search_runs_end_to_end_and_then_hits_cache() {
        let temp = tempdir().unwrap();
        let config = Config {
            cache_dir: temp.path().join("cache"),
            config_dir: temp.path().join("config"),
            ..Default::default()
        };
        config.ensure_dirs().unwrap();
        let repository = Repository {
            uuid: "{repo}".into(),
            workspace: "team".into(),
            slug: "api".into(),
            name: "API".into(),
            full_name: "team/api".into(),
            default_branch: Some("main".into()),
            clone_url: "https://example.invalid/team/api.git".into(),
            web_url: "https://example.invalid/team/api".into(),
        };
        let checkout = git_sync::snapshot_path(&config, &repository, "main");
        fs::create_dir_all(checkout.parent().unwrap()).unwrap();
        let git = GitRepository::init(&checkout).unwrap();
        fs::write(checkout.join("service.rs"), "fn wanted_symbol() {}\n").unwrap();
        let mut index = git.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = git.find_tree(tree_id).unwrap();
        let signature = Signature::now("bbs", "bbs@example.invalid").unwrap();
        git.commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();
        cache::save_catalog(
            &config,
            &RepositoryCatalog {
                discovered_at: Utc::now(),
                workspaces: vec![],
                repositories: vec![repository],
            },
        )
        .unwrap();
        let app = BbsApp::new(config).unwrap();
        let request = SearchRequest {
            queries: vec!["wanted_*".into()],
            offline: true,
            ..Default::default()
        };
        let progress: Progress = Arc::new(|_| {});
        let first = app
            .search(
                request.clone(),
                progress.clone(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!(first.results.len(), 1);
        assert!(!first.cached);
        let second = app
            .search(request, progress, Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert!(second.cached);
        assert_eq!(second.results[0].path, "service.rs");
    }

    /// The whole online path -- discovery, catalog write, a real clone, the
    /// scan, and the result cache -- against a stub API and a local Git
    /// remote. There was previously no seam for this at all: an "online" test
    /// silently reached the real network and took nearly two minutes.
    #[tokio::test]
    async fn the_online_path_runs_end_to_end_against_a_stub_api() {
        use axum::{Router, routing::get};

        let temp = tempdir().unwrap();

        // a real Git remote on disk, so `synchronize` genuinely clones
        let remote = temp.path().join("remote");
        let git = GitRepository::init(&remote).unwrap();
        fs::write(remote.join("service.rs"), "fn wanted_symbol() {}\n").unwrap();
        let mut index = git.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = git.find_tree(tree_id).unwrap();
        let signature = Signature::now("bbs", "bbs@example.invalid").unwrap();
        git.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "fixture",
            &tree,
            &[],
        )
        .unwrap();
        git.set_head("refs/heads/main").unwrap();

        // a stub of the two endpoints discovery walks
        let clone_url = remote.to_string_lossy().into_owned();
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = seen.clone();
        let router = Router::new()
            .route(
                "/user/workspaces",
                get(move || {
                    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    async {
                        axum::Json(serde_json::json!({
                            "values": [{"workspace": {
                                "uuid": "{workspace}", "slug": "team", "name": "Team"
                            }}]
                        }))
                    }
                }),
            )
            .route(
                "/repositories/{workspace}",
                get(move || {
                    let clone_url = clone_url.clone();
                    async move {
                        axum::Json(serde_json::json!({
                            "values": [{
                                "uuid": "{repo}", "slug": "api", "name": "API",
                                "full_name": "team/api",
                                "mainbranch": {"name": "main"},
                                "workspace": {"slug": "team"},
                                "links": {
                                    "clone": [{"name": "https", "href": clone_url}],
                                    "html": {"href": "https://example.invalid/team/api"}
                                }
                            }]
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let config = Config {
            cache_dir: temp.path().join("cache"),
            config_dir: temp.path().join("config"),
            api_base: api_base.clone(),
            ..Default::default()
        };
        config.ensure_dirs().unwrap();
        let app = BbsApp::with_token(config.clone(), "stub-token").unwrap();
        let progress: Progress = Arc::new(|_| {});

        let first = app
            .search(
                SearchRequest {
                    queries: vec!["wanted_symbol".into()],
                    ..Default::default()
                },
                progress.clone(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!(first.results.len(), 1);
        assert_eq!(first.results[0].path, "service.rs");
        assert_eq!(first.results[0].repository, "team/api");
        // an online result is not stale, and links at the commit it scanned
        assert!(!first.results[0].stale);
        assert!(!first.offline);
        assert!(!first.cached);
        assert!(
            first.results[0]
                .web_url
                .starts_with("https://example.invalid/team/api/src/")
        );
        // discovery was written to the catalog cache on the way through
        assert_eq!(
            cache::load_catalog(&config).unwrap().repositories[0].full_name,
            "team/api"
        );

        // and the second run reuses the stored scan
        let second = app
            .search(
                SearchRequest {
                    queries: vec!["wanted_symbol".into()],
                    ..Default::default()
                },
                progress,
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert!(second.cached);
        assert_eq!(second.results.len(), 1);
        assert_eq!(seen.load(std::sync::atomic::Ordering::Relaxed), 2);

        server.abort();
    }

    /// Warming must leave behind exactly what a search consumes, so the
    /// assertion is not "a report was produced" but "an offline search now
    /// finds the code without ever having run online".
    #[tokio::test]
    async fn warmup_clones_the_workspace_and_a_repeat_inside_the_window_reuses_it() {
        use axum::{Router, routing::get};

        let temp = tempdir().unwrap();

        // a real Git remote on disk, so the warmup genuinely clones
        let remote = temp.path().join("remote");
        let git = GitRepository::init(&remote).unwrap();
        fs::write(remote.join("service.rs"), "fn wanted_symbol() {}\n").unwrap();
        let mut index = git.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = git.find_tree(tree_id).unwrap();
        let signature = Signature::now("bbs", "bbs@example.invalid").unwrap();
        git.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "fixture",
            &tree,
            &[],
        )
        .unwrap();
        git.set_head("refs/heads/main").unwrap();

        // two repositories: one clonable, one that cannot possibly be
        let clone_url = remote.to_string_lossy().into_owned();
        let router = Router::new()
            .route(
                "/user/workspaces",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "values": [{"workspace": {
                            "uuid": "{workspace}", "slug": "team", "name": "Team"
                        }}]
                    }))
                }),
            )
            .route(
                "/repositories/{workspace}",
                get(move || {
                    let clone_url = clone_url.clone();
                    async move {
                        axum::Json(serde_json::json!({
                            "values": [
                                {
                                    "uuid": "{repo}", "slug": "api", "name": "API",
                                    "full_name": "team/api",
                                    "mainbranch": {"name": "main"},
                                    "workspace": {"slug": "team"},
                                    "links": {
                                        "clone": [{"name": "https", "href": clone_url}],
                                        "html": {"href": "https://example.invalid/team/api"}
                                    }
                                },
                                {
                                    "uuid": "{broken}", "slug": "broken", "name": "Broken",
                                    "full_name": "team/broken",
                                    "mainbranch": {"name": "main"},
                                    "workspace": {"slug": "team"},
                                    "links": {
                                        "clone": [{"name": "https",
                                                   "href": "nosuchproto://example.invalid/b.git"}],
                                        "html": {"href": "https://example.invalid/team/broken"}
                                    }
                                }
                            ]
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let config = Config {
            cache_dir: temp.path().join("cache"),
            config_dir: temp.path().join("config"),
            api_base,
            ..Default::default()
        };
        config.ensure_dirs().unwrap();
        let app = BbsApp::with_token(config.clone(), "stub-token").unwrap();
        let warnings = Arc::new(std::sync::Mutex::new(Vec::new()));
        let collected = warnings.clone();
        let progress: Progress = Arc::new(move |event| {
            if let SearchEvent::Warning { message } = event {
                collected.lock().unwrap().push(message);
            }
        });

        let first = app
            .warmup(WarmupRequest::default(), progress.clone())
            .await
            .unwrap();
        assert_eq!(first.repositories, 2);
        assert_eq!(first.warmed, 1);
        assert_eq!(first.fetched, 1);
        assert_eq!(first.reused, 0);
        assert!(first.snapshot_bytes > 0);
        // one repository that cannot be cloned does not fail the warmup, and
        // is named while it happens rather than only in the report
        assert_eq!(first.skipped.len(), 1, "{:?}", first.skipped);
        assert_eq!(first.skipped[0].repository, "team/broken");
        assert!(
            warnings
                .lock()
                .unwrap()
                .iter()
                .any(|message| message.contains("team/broken")),
        );

        // the point of the whole exercise: the next search finds the code
        // without touching the network
        let searched = app
            .search(
                SearchRequest {
                    queries: vec!["wanted_symbol".into()],
                    offline: true,
                    ..Default::default()
                },
                progress.clone(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!(searched.results.len(), 1);
        assert_eq!(searched.results[0].repository, "team/api");

        // and a scheduled repeat inside the window refetches nothing
        let second = app
            .warmup(
                WarmupRequest {
                    max_age_seconds: Some(3600),
                    ..Default::default()
                },
                progress,
            )
            .await
            .unwrap();
        assert_eq!(second.warmed, 1);
        assert_eq!(second.reused, 1, "a fresh snapshot must not be refetched");
        assert_eq!(second.fetched, 0);

        server.abort();
    }

    /// `--max-age` covers discovery as well as the fetches. Without it, an
    /// online repeat query pays for a full workspace and repository walk of
    /// the Bitbucket API before a single file is looked at.
    #[tokio::test]
    async fn a_recent_catalog_is_reused_inside_the_max_age_window() {
        let temp = tempdir().unwrap();
        let config = Config {
            cache_dir: temp.path().join("cache"),
            config_dir: temp.path().join("config"),
            // any request that escaped the window would fail against this
            api_base: "http://127.0.0.1:1/unused".into(),
            ..Default::default()
        };
        config.ensure_dirs().unwrap();
        cache::save_catalog(
            &config,
            &RepositoryCatalog {
                discovered_at: Utc::now(),
                workspaces: vec![],
                repositories: vec![Repository {
                    uuid: "{repo}".into(),
                    workspace: "team".into(),
                    slug: "api".into(),
                    name: "API".into(),
                    full_name: "team/api".into(),
                    default_branch: Some("main".into()),
                    clone_url: String::new(),
                    web_url: String::new(),
                }],
            },
        )
        .unwrap();
        let app = BbsApp::new(config).unwrap();

        // online, but inside the window: served from the cache, no network
        let catalog = app
            .catalog(false, Some(std::time::Duration::from_secs(3600)))
            .await
            .unwrap();
        assert_eq!(catalog.repositories.len(), 1);

        // outside it, the network is contacted -- and here, fails
        assert!(
            app.catalog(false, Some(std::time::Duration::from_secs(0)))
                .await
                .is_err()
        );
    }

    /// One repository that cannot be prepared must not fail the search for
    /// all the others. An empty repository is already reported as
    /// unavailable, but every other clone failure -- a revoked permission, a
    /// remote that no longer answers, an unusable clone URL -- still aborted
    /// the whole workspace-wide search from inside the sync loop.
    #[tokio::test]
    async fn one_unclonable_repository_is_skipped_rather_than_failing_the_search() {
        let temp = tempdir().unwrap();
        let remote = temp.path().join("remote");
        let git = GitRepository::init(&remote).unwrap();
        fs::write(remote.join("service.rs"), "fn wanted_symbol() {}\n").unwrap();
        let mut index = git.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = git.find_tree(tree_id).unwrap();
        let signature = Signature::now("bbs", "bbs@example.invalid").unwrap();
        git.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "fixture",
            &tree,
            &[],
        )
        .unwrap();
        git.set_head("refs/heads/main").unwrap();

        let config = Config {
            cache_dir: temp.path().join("cache"),
            config_dir: temp.path().join("config"),
            // any escape to the network would fail against this
            api_base: "http://127.0.0.1:1/unused".into(),
            ..Default::default()
        };
        config.ensure_dirs().unwrap();
        cache::save_catalog(
            &config,
            &RepositoryCatalog {
                discovered_at: Utc::now(),
                workspaces: vec![],
                repositories: vec![
                    Repository {
                        uuid: "{good}".into(),
                        workspace: "team".into(),
                        slug: "api".into(),
                        name: "API".into(),
                        full_name: "team/api".into(),
                        default_branch: Some("main".into()),
                        clone_url: remote.to_string_lossy().into_owned(),
                        web_url: "https://example.invalid/team/api".into(),
                    },
                    Repository {
                        uuid: "{broken}".into(),
                        workspace: "team".into(),
                        slug: "broken".into(),
                        name: "Broken".into(),
                        full_name: "team/broken".into(),
                        default_branch: Some("main".into()),
                        clone_url: "nosuchproto://example.invalid/team/broken.git".into(),
                        web_url: "https://example.invalid/team/broken".into(),
                    },
                ],
            },
        )
        .unwrap();

        let app = BbsApp::with_token(config, "stub-token").unwrap();
        let warnings = Arc::new(std::sync::Mutex::new(Vec::new()));
        let collected = warnings.clone();
        let progress: Progress = Arc::new(move |event| {
            if let SearchEvent::Warning { message } = event {
                collected.lock().unwrap().push(message);
            }
        });
        let response = app
            .search(
                SearchRequest {
                    queries: vec!["wanted_symbol".into()],
                    // reuse the catalog above rather than reaching for the API
                    max_age_seconds: Some(3600),
                    ..Default::default()
                },
                progress,
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .expect("one unclonable repository must not fail the search");

        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].repository, "team/api");
        assert_eq!(response.skipped.len(), 1, "{:?}", response.skipped);
        assert_eq!(response.skipped[0].repository, "team/broken");
        assert_eq!(response.skipped[0].branch.as_deref(), Some("main"));
        assert!(!response.skipped[0].reason.is_empty());
        // and the skip is reported while it happens, not only in the summary
        let warnings = warnings.lock().unwrap();
        assert!(
            warnings
                .iter()
                .any(|message| message.contains("team/broken")),
            "{warnings:?}"
        );
    }

    /// Presentation must not be part of the cache key. Re-running the same
    /// query with a different `--sort`, a smaller `--context` or a smaller
    /// `--max-results` used to rescan every file to reach the same answer in a
    /// different shape.
    #[tokio::test]
    async fn changing_only_the_presentation_is_a_cache_hit() {
        let temp = tempdir().unwrap();
        let config = Config {
            cache_dir: temp.path().join("cache"),
            config_dir: temp.path().join("config"),
            ..Default::default()
        };
        config.ensure_dirs().unwrap();
        let repository = Repository {
            uuid: "{repo}".into(),
            workspace: "team".into(),
            slug: "api".into(),
            name: "API".into(),
            full_name: "team/api".into(),
            default_branch: Some("main".into()),
            clone_url: "https://example.invalid/team/api.git".into(),
            web_url: "https://example.invalid/team/api".into(),
        };
        let checkout = git_sync::snapshot_path(&config, &repository, "main");
        fs::create_dir_all(checkout.parent().unwrap()).unwrap();
        let git = GitRepository::init(&checkout).unwrap();
        for name in ["b.rs", "a.rs"] {
            fs::write(checkout.join(name), "one\ntwo\nwanted_symbol\nfour\nfive\n").unwrap();
        }
        let mut index = git.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = git.find_tree(tree_id).unwrap();
        let signature = Signature::now("bbs", "bbs@example.invalid").unwrap();
        git.commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();
        cache::save_catalog(
            &config,
            &RepositoryCatalog {
                discovered_at: Utc::now(),
                workspaces: vec![],
                repositories: vec![repository],
            },
        )
        .unwrap();
        let app = BbsApp::new(config).unwrap();
        let progress: Progress = Arc::new(|_| {});
        let run = async |request: SearchRequest| {
            app.search(request, progress.clone(), Arc::new(AtomicBool::new(false)))
                .await
                .unwrap()
        };
        let base = SearchRequest {
            queries: vec!["wanted_symbol".into()],
            offline: true,
            ..Default::default()
        };

        let first = run(base.clone()).await;
        assert!(!first.cached);
        assert_eq!(first.results.len(), 2);

        // a different sort order
        let sorted = run(SearchRequest {
            sort: SortMode::Path,
            ..base.clone()
        })
        .await;
        assert!(sorted.cached, "changing --sort must not rescan");
        assert_eq!(sorted.results[0].path, "a.rs");

        // a narrower context, correctly narrowed
        let narrow = run(SearchRequest {
            context: Some(1),
            ..base.clone()
        })
        .await;
        assert!(narrow.cached, "changing --context must not rescan");
        assert_eq!(
            narrow.results[0]
                .lines
                .iter()
                .map(|line| line.number)
                .collect::<Vec<_>>(),
            vec![2, 3, 4]
        );

        // a smaller limit, with the true total still reported
        let limited = run(SearchRequest {
            max_results: Some(1),
            ..base.clone()
        })
        .await;
        assert!(limited.cached, "changing --max-results must not rescan");
        assert_eq!(limited.results.len(), 1);
        assert_eq!(limited.total_results, 2);
        assert!(limited.truncation.results_capped);
    }

    /// A cache hit must report the freshness of *this* search. An offline hit
    /// on an entry written by an online search used to claim the results had
    /// been verified against the remote, and vice versa.
    #[test]
    fn cached_results_are_relabelled_with_this_run_s_freshness() {
        let repository = Repository {
            uuid: "{repo}".into(),
            workspace: "team".into(),
            slug: "api".into(),
            name: "API".into(),
            full_name: "team/api".into(),
            default_branch: Some("main".into()),
            clone_url: String::new(),
            web_url: "https://example.invalid/team/api".into(),
        };
        let snapshot = |stale: bool| Snapshot {
            repository: repository.clone(),
            branch: "main".into(),
            commit: "deadbeef".into(),
            synchronized_at: Utc::now(),
            checkout: std::path::PathBuf::new(),
            stale,
        };
        let response = |stale: bool| SearchResponse {
            query: vec!["wanted".into()],
            results: vec![crate::model::SearchResult {
                repository: "team/api".into(),
                repository_name: "API".into(),
                path: "service.rs".into(),
                branch: "main".into(),
                commit: "deadbeef".into(),
                web_url: String::new(),
                score: 1.0,
                match_count: 1,
                lines: vec![],
                stale,
            }],
            repositories_searched: 1,
            files_searched: 1,
            cached: true,
            ..Default::default()
        };

        // an offline run reusing a body written online must read as stale
        let mut cached = response(false);
        restamp_freshness(&mut cached, &[snapshot(true)]);
        assert!(cached.results[0].stale);

        // and an online run reusing a body written offline must not
        let mut cached = response(true);
        restamp_freshness(&mut cached, &[snapshot(false)]);
        assert!(!cached.results[0].stale);

        // a repository absent from this run's snapshots is left untouched
        let mut cached = response(true);
        restamp_freshness(&mut cached, &[]);
        assert!(cached.results[0].stale);
    }
}
