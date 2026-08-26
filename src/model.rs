use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Workspace {
    pub uuid: String,
    pub slug: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Repository {
    pub uuid: String,
    pub workspace: String,
    pub slug: String,
    pub name: String,
    pub full_name: String,
    pub default_branch: Option<String>,
    pub clone_url: String,
    pub web_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryCatalog {
    pub discovered_at: DateTime<Utc>,
    pub workspaces: Vec<Workspace>,
    pub repositories: Vec<Repository>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub repository: Repository,
    pub branch: String,
    pub commit: String,
    pub synchronized_at: DateTime<Utc>,
    #[serde(skip)]
    pub checkout: PathBuf,
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MatchRange {
    pub start: usize,
    pub end: usize,
    pub atom: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResultLine {
    pub number: usize,
    pub text: String,
    pub ranges: Vec<MatchRange>,
    pub is_context: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub repository: String,
    pub repository_name: String,
    pub path: String,
    pub branch: String,
    pub commit: String,
    pub web_url: String,
    pub score: f64,
    pub match_count: usize,
    pub lines: Vec<ResultLine>,
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: Vec<String>,
    pub results: Vec<SearchResult>,
    pub repositories_searched: usize,
    pub files_searched: usize,
    pub elapsed_ms: u128,
    pub cached: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SearchEvent {
    Progress {
        phase: String,
        message: String,
        current: usize,
        total: usize,
    },
    Result {
        result: SearchResult,
    },
    Warning {
        message: String,
    },
    Error {
        message: String,
    },
    Done {
        response: SearchResponse,
    },
}
