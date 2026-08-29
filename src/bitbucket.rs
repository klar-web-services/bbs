use crate::model::{Repository, RepositoryCatalog, Workspace};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use reqwest::{Client, StatusCode};
use serde::Deserialize;

#[derive(Clone)]
pub struct BitbucketClient {
    http: Client,
    api_base: String,
    token: String,
}

#[derive(Debug, Deserialize)]
struct Page<T> {
    values: Vec<T>,
    next: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceAccess {
    workspace: WorkspaceDto,
}

#[derive(Debug, Deserialize)]
struct WorkspaceDto {
    uuid: String,
    slug: String,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct RepositoryDto {
    uuid: String,
    slug: String,
    name: String,
    full_name: String,
    mainbranch: Option<BranchDto>,
    links: RepositoryLinks,
    workspace: Option<RepositoryWorkspaceDto>,
}

#[derive(Debug, Deserialize)]
struct RepositoryWorkspaceDto {
    slug: String,
}

#[derive(Debug, Deserialize)]
struct BranchDto {
    name: String,
}

#[derive(Debug, Deserialize)]
struct RepositoryLinks {
    #[serde(default)]
    clone: Vec<Link>,
    html: Option<Link>,
}

#[derive(Debug, Deserialize)]
struct Link {
    href: String,
    name: Option<String>,
}

impl BitbucketClient {
    pub fn new(api_base: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let http = Client::builder()
            .user_agent(concat!("bbs/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            api_base: api_base.into().trim_end_matches('/').into(),
            token: token.into(),
        })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let mut attempt = 0u32;
        let response = loop {
            let response = self
                .http
                .get(url)
                .bearer_auth(&self.token)
                .send()
                .await
                .with_context(|| format!("request to Bitbucket failed: {url}"))?;
            let retryable = response.status() == StatusCode::TOO_MANY_REQUESTS
                || response.status().is_server_error();
            if !retryable || attempt >= 3 {
                break response;
            }
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            let seconds = retry_after.unwrap_or(1u64 << attempt).min(15);
            tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
            attempt += 1;
        };
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            bail!("Bitbucket rejected the credential; run `bbs login` again");
        }
        if status == StatusCode::FORBIDDEN {
            bail!(
                "Bitbucket denied access; ensure the token has read:workspace:bitbucket and read:repository:bitbucket scopes"
            );
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            bail!("Bitbucket rate limit reached; wait before retrying");
        }
        if !status.is_success() {
            let message = response.text().await.unwrap_or_default();
            let compact = message.chars().take(300).collect::<String>();
            bail!("Bitbucket returned {status}: {compact}");
        }
        response
            .json()
            .await
            .context("Bitbucket returned malformed JSON")
    }

    pub async fn discover(&self) -> Result<RepositoryCatalog> {
        let mut workspaces = Vec::new();
        let mut next = Some(format!(
            "{}/user/workspaces?pagelen=100&fields=values.workspace.uuid,values.workspace.slug,values.workspace.name,next",
            self.api_base
        ));
        while let Some(url) = next.take() {
            let page: Page<WorkspaceAccess> = self.get(&url).await?;
            workspaces.extend(page.values.into_iter().map(|item| Workspace {
                uuid: item.workspace.uuid,
                slug: item.workspace.slug,
                name: item.workspace.name,
            }));
            next = page.next;
        }

        let mut repositories = Vec::new();
        for workspace in &workspaces {
            let mut next = Some(format!(
                "{}/repositories/{}?role=member&pagelen=100&fields=values.uuid,values.slug,values.name,values.full_name,values.mainbranch.name,values.workspace.slug,values.links.clone,values.links.html,next",
                self.api_base,
                urlencoding::encode(&workspace.slug)
            ));
            while let Some(url) = next.take() {
                let page: Page<RepositoryDto> = self.get(&url).await?;
                for repo in page.values {
                    let clone_url = repo
                        .links
                        .clone
                        .iter()
                        .find(|link| link.name.as_deref() == Some("https"))
                        .or_else(|| repo.links.clone.first())
                        .map(|link| link.href.clone())
                        .context("Bitbucket repository has no clone URL")?;
                    let web_url = repo
                        .links
                        .html
                        .map(|link| link.href)
                        .unwrap_or_else(|| format!("https://bitbucket.org/{}", repo.full_name));
                    repositories.push(Repository {
                        uuid: repo.uuid,
                        workspace: repo
                            .workspace
                            .map(|w| w.slug)
                            .unwrap_or_else(|| workspace.slug.clone()),
                        slug: repo.slug,
                        name: repo.name,
                        full_name: repo.full_name,
                        default_branch: repo.mainbranch.map(|branch| branch.name),
                        clone_url,
                        web_url,
                    });
                }
                next = page.next;
            }
        }
        repositories.sort_by(|a, b| a.full_name.cmp(&b.full_name));
        repositories.dedup_by(|a, b| a.uuid == b.uuid);
        Ok(RepositoryCatalog {
            discovered_at: Utc::now(),
            workspaces,
            repositories,
        })
    }
}

/// Whether a requested name is a pattern rather than an exact name.
fn is_pattern(name: &str) -> bool {
    name.contains('*') || name.contains('?') || name.contains('[')
}

fn matches_pattern(repository: &Repository, pattern: &str) -> bool {
    let glob = match globset::GlobBuilder::new(pattern)
        .case_insensitive(true)
        .build()
    {
        Ok(glob) => glob.compile_matcher(),
        Err(_) => return false,
    };
    glob.is_match(&repository.slug) || glob.is_match(&repository.full_name)
}

/// Edit distance with an early exit, used only to suggest a correction. A
/// mistyped scope used to be reported with no suggestion even though the whole
/// catalog was already in memory and the right answer was one edit away.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, ach) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, bch) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ach != bch);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

/// The closest accessible name to `name`, if one is close enough to be worth
/// naming. The threshold scales with length so a short slug cannot be
/// "corrected" to something unrelated.
pub fn did_you_mean(catalog: &RepositoryCatalog, name: &str) -> Option<String> {
    let budget = (name.chars().count() / 4).max(2);
    let lowered = name.to_ascii_lowercase();
    catalog
        .repositories
        .iter()
        .flat_map(|repository| [&repository.slug, &repository.full_name])
        .map(|candidate| {
            (
                edit_distance(&lowered, &candidate.to_ascii_lowercase()),
                candidate.clone(),
            )
        })
        .filter(|(distance, _)| *distance <= budget)
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.len().cmp(&b.1.len())))
        .map(|(_, candidate)| candidate)
}

pub fn resolve_repositories(
    catalog: &RepositoryCatalog,
    requested: &[String],
) -> Result<Vec<Repository>> {
    // An empty entry is a trailing comma, not a repository. Naming it as
    // inaccessible reported the wrong problem.
    let requested: Vec<&String> = requested
        .iter()
        .filter(|name| !name.trim().is_empty())
        .collect();
    if requested.is_empty() {
        return Ok(catalog.repositories.clone());
    }
    let mut resolved = Vec::new();
    for name in requested {
        // A pattern selects breadth on purpose, so the ambiguity check that
        // protects an exact short name does not apply to it.
        if is_pattern(name) {
            let matched: Vec<_> = catalog
                .repositories
                .iter()
                .filter(|repository| matches_pattern(repository, name))
                .cloned()
                .collect();
            if matched.is_empty() {
                bail!("no repository matches `{name}`");
            }
            resolved.extend(matched);
            continue;
        }
        let mut matches: Vec<_> = catalog
            .repositories
            .iter()
            .filter(|repo| {
                repo.full_name.eq_ignore_ascii_case(name)
                    || repo.uuid == *name
                    || repo.slug.eq_ignore_ascii_case(name)
            })
            .cloned()
            .collect();
        if matches.is_empty() {
            match did_you_mean(catalog, name) {
                Some(suggestion) => {
                    bail!("repository `{name}` is not accessible; did you mean `{suggestion}`?")
                }
                None => bail!("repository `{name}` is not accessible"),
            }
        }
        if matches.len() > 1 && !name.contains('/') {
            let choices = matches
                .iter()
                .map(|repo| repo.full_name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("repository `{name}` is ambiguous; use one of: {choices}");
        }
        resolved.push(matches.remove(0));
    }
    resolved.sort_by(|a, b| a.full_name.cmp(&b.full_name));
    resolved.dedup_by(|a, b| a.uuid == b.uuid);
    Ok(resolved)
}

/// Filters a listing. A pattern is matched as a glob, anything else as a
/// case-insensitive substring, because `bbs repos api` is a search rather than
/// a lookup and seventy unfiltered lines are not a listing anyone reads.
pub fn filter_repositories<'a>(
    repositories: &'a [Repository],
    filter: &str,
) -> Vec<&'a Repository> {
    let lowered = filter.to_ascii_lowercase();
    repositories
        .iter()
        .filter(|repository| {
            if is_pattern(filter) {
                matches_pattern(repository, filter)
            } else {
                repository.full_name.to_ascii_lowercase().contains(&lowered)
                    || repository.slug.to_ascii_lowercase().contains(&lowered)
                    || repository.name.to_ascii_lowercase().contains(&lowered)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(workspace: &str, slug: &str) -> Repository {
        Repository {
            uuid: format!("{{{workspace}-{slug}}}"),
            workspace: workspace.into(),
            slug: slug.into(),
            name: slug.into(),
            full_name: format!("{workspace}/{slug}"),
            default_branch: Some("main".into()),
            clone_url: String::new(),
            web_url: String::new(),
        }
    }

    #[test]
    fn short_names_must_be_unique() {
        let catalog = RepositoryCatalog {
            discovered_at: Utc::now(),
            workspaces: vec![],
            repositories: vec![repo("one", "api"), repo("two", "api")],
        };
        assert!(
            resolve_repositories(&catalog, &["api".into()])
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        assert_eq!(
            resolve_repositories(&catalog, &["one/api".into()]).unwrap()[0].workspace,
            "one"
        );
    }

    /// A mistyped scope has the whole catalog in memory and the right answer
    /// one edit away, so it should say so.
    #[test]
    fn a_mistyped_repository_is_offered_the_closest_name() {
        let catalog = RepositoryCatalog {
            discovered_at: Utc::now(),
            workspaces: vec![],
            repositories: vec![
                repo("team", "api-gateway"),
                repo("team", "edge-router"),
                repo("team", "edge-proxy"),
            ],
        };
        let error = resolve_repositories(&catalog, &["api-gatewy".into()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("did you mean `api-gateway`"), "{error}");

        // nothing close enough keeps the plain message rather than guessing
        let error = resolve_repositories(&catalog, &["totally-unrelated".into()])
            .unwrap_err()
            .to_string();
        assert!(!error.contains("did you mean"), "{error}");
    }

    /// `--repos 'edge-*'` selects breadth on purpose, so the ambiguity check
    /// that guards a bare short name must not apply to it.
    #[test]
    fn a_pattern_selects_every_repository_it_matches() {
        let catalog = RepositoryCatalog {
            discovered_at: Utc::now(),
            workspaces: vec![],
            repositories: vec![
                repo("team", "api-gateway"),
                repo("team", "edge-router"),
                repo("team", "edge-proxy"),
            ],
        };
        let resolved = resolve_repositories(&catalog, &["edge-*".into()]).unwrap();
        assert_eq!(
            resolved.iter().map(|r| r.slug.as_str()).collect::<Vec<_>>(),
            ["edge-proxy", "edge-router"]
        );
        assert!(
            resolve_repositories(&catalog, &["nothing-*".into()])
                .unwrap_err()
                .to_string()
                .contains("no repository matches")
        );
    }

    /// `--repos api,` reported ``repository `` is not accessible``, which
    /// names the wrong problem.
    #[test]
    fn empty_entries_are_ignored_rather_than_reported_as_missing() {
        let catalog = RepositoryCatalog {
            discovered_at: Utc::now(),
            workspaces: vec![],
            repositories: vec![repo("team", "api"), repo("team", "web")],
        };
        let resolved = resolve_repositories(&catalog, &["api".into(), "".into()]).unwrap();
        assert_eq!(resolved.len(), 1);
        // all-empty means no scope was really given, which is every repository
        assert_eq!(
            resolve_repositories(&catalog, &["".into(), "  ".into()])
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn a_listing_filter_matches_substrings_and_patterns() {
        let repositories = vec![
            repo("team", "api-gateway"),
            repo("team", "edge-router"),
            repo("other", "api"),
        ];
        assert_eq!(filter_repositories(&repositories, "api").len(), 2);
        assert_eq!(filter_repositories(&repositories, "EDGE").len(), 1);
        assert_eq!(filter_repositories(&repositories, "edge-*").len(), 1);
        assert_eq!(filter_repositories(&repositories, "other/*").len(), 1);
        assert!(filter_repositories(&repositories, "zzz").is_empty());
    }

    #[test]
    fn decodes_discovery_payloads() {
        let workspaces: Page<WorkspaceAccess> = serde_json::from_value(serde_json::json!({
            "values": [{"workspace": {"uuid": "{workspace}", "slug": "team", "name": "Team"}}]
        }))
        .unwrap();
        let repositories: Page<RepositoryDto> = serde_json::from_value(serde_json::json!({
            "values": [{
                "uuid": "{repo}", "slug": "api", "name": "API", "full_name": "team/api",
                "mainbranch": {"name": "main"}, "workspace": {"slug":"team"},
                "links": {"clone": [{"name":"https", "href":"https://bitbucket.org/team/api.git"}], "html":{"href":"https://bitbucket.org/team/api"}}
            }]
        })).unwrap();
        assert_eq!(workspaces.values[0].workspace.slug, "team");
        assert_eq!(repositories.values[0].full_name, "team/api");
        assert_eq!(
            repositories.values[0]
                .mainbranch
                .as_ref()
                .map(|branch| branch.name.as_str()),
            Some("main")
        );
    }
}
