use crate::{
    auth,
    bitbucket::{self, BitbucketClient},
    cache,
    config::Config,
    git_sync,
    model::{RepositoryCatalog, SearchEvent, SearchResponse, SkippedRepository, Snapshot},
    query::{CaseMode, CompiledQuery},
    search::{self, SearchOptions, SortMode},
};
use anyhow::{Context, Result, bail};
use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, atomic::AtomicBool};

pub type Progress = Arc<dyn Fn(SearchEvent) + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchRequest {
    pub queries: Vec<String>,
    pub repositories: Vec<String>,
    pub paths: Vec<String>,
    pub branch: Option<String>,
    pub regex: bool,
    pub case_mode: CaseMode,
    pub multiline: bool,
    pub offline: bool,
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
            branch: None,
            regex: false,
            case_mode: CaseMode::Smart,
            multiline: false,
            offline: false,
            context: None,
            max_results: None,
            sort: SortMode::Relevance,
            no_cache: false,
        }
    }
}

#[derive(Clone)]
pub struct BbsApp {
    pub config: Config,
}

impl BbsApp {
    pub fn new(config: Config) -> Result<Self> {
        config.ensure_dirs()?;
        Ok(Self { config })
    }

    pub async fn catalog(&self, offline: bool) -> Result<RepositoryCatalog> {
        if offline {
            return cache::load_catalog(&self.config);
        }
        let client = BitbucketClient::new(&self.config.api_base, auth::token()?)?;
        let catalog = client.discover().await?;
        cache::save_catalog(&self.config, &catalog)?;
        Ok(catalog)
    }

    pub async fn validate_login(&self, token: &str) -> Result<String> {
        let client = BitbucketClient::new(&self.config.api_base, token)?;
        let catalog = client.discover().await?;
        let summary = format!("{} accessible repositories", catalog.repositories.len());
        cache::save_catalog(&self.config, &catalog)?;
        Ok(summary)
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
            request.regex,
            request.case_mode,
            request.multiline,
        )?;
        let lock_config = self.config.clone();
        let _search_lock =
            tokio::task::spawn_blocking(move || git_sync::lock_searches(&lock_config))
                .await
                .context("search lock task failed")??;
        (progress)(SearchEvent::Progress {
            phase: "discovery".into(),
            message: "Discovering accessible repositories".into(),
            current: 0,
            total: 0,
        });
        let catalog = self.catalog(request.offline).await?;
        let repositories = bitbucket::resolve_repositories(&catalog, &request.repositories)?;
        if repositories.is_empty() {
            bail!("no accessible repositories were found");
        }
        let token = if request.offline {
            None
        } else {
            Some(auth::token()?)
        };
        let total = repositories.len();
        let config = self.config.clone();
        let branch_override = request.branch.clone();
        let progress_sync = progress.clone();
        let concurrency = self.config.sync_concurrency.max(1);
        let mut completed = 0usize;
        let jobs = stream::iter(repositories.into_iter().map(|repository| {
            let config = config.clone();
            let token = token.clone();
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
                let outcome = tokio::task::spawn_blocking({
                    let branch = branch.clone();
                    move || match token {
                        Some(token) => git_sync::synchronize(&config, &repository, &branch, &token),
                        None => git_sync::load_offline(&config, &repository, &branch),
                    }
                })
                .await
                .context("snapshot task failed")??;
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
        if snapshots.is_empty() {
            let detail = skipped
                .iter()
                .map(|s| format!("{} ({})", s.repository, s.reason))
                .collect::<Vec<_>>()
                .join(", ");
            bail!("no repository could be searched: {detail}");
        }
        skipped.sort_by(|a, b| a.repository.cmp(&b.repository));
        snapshots.sort_by(|a, b| a.repository.full_name.cmp(&b.repository.full_name));
        // Fetching seventy repositories and scanning them are very different
        // costs, and one `elapsed_ms` could not tell them apart.
        let sync_ms = started.elapsed().as_millis();
        let options = SearchOptions {
            paths: request.paths,
            context: request.context.unwrap_or(self.config.context_lines),
            max_results: request.max_results.unwrap_or(self.config.max_results),
            max_file_bytes: self.config.max_file_bytes,
            sort: request.sort,
        };
        let key = cache::result_key(&query.normalized(), &options, &snapshots)?;
        if !request.no_cache
            && let Some(mut response) = cache::load_result(&self.config, &key)?
        {
            response.skipped = skipped;
            response.offline = request.offline;
            response.sync_ms = sync_ms;
            response.scan_ms = 0;
            response.elapsed_ms = started.elapsed().as_millis();
            // Freshness describes this run, not the run that populated the
            // cache. The cached body is keyed on exact commits so its content
            // is right either way, but an offline hit must not inherit the
            // "verified against the remote" label from an earlier online
            // search, nor the reverse.
            restamp_freshness(&mut response, &snapshots);
            (progress)(SearchEvent::Done {
                response: response.clone(),
            });
            return Ok(response);
        }
        (progress)(SearchEvent::Progress {
            phase: "search".into(),
            message: "Scanning synchronized snapshots".into(),
            current: 0,
            total: snapshots.len(),
        });
        let query = Arc::new(query);
        let options_for_search = options.clone();
        let cancelled_for_search = cancelled.clone();
        let response = tokio::task::spawn_blocking(move || {
            search::run(
                &query,
                &snapshots,
                &options_for_search,
                cancelled_for_search,
            )
        })
        .await
        .context("search task failed")??
        .response;
        let mut response = response;
        response.skipped = skipped;
        response.offline = request.offline;
        response.sync_ms = sync_ms;
        response.elapsed_ms = started.elapsed().as_millis();
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            bail!("search cancelled");
        }
        if !request.no_cache {
            cache::save_result(&self.config, &key, &response)?;
        }
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
