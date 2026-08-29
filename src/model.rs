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

/// A repository that was asked for but could not contribute to this search:
/// it has no commits, lacks the requested branch, or has no cached snapshot in
/// offline mode. It is reported rather than failing the whole search, because
/// skipping it hides no results that could otherwise have been found.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkippedRepository {
    pub repository: String,
    pub branch: Option<String>,
    pub reason: String,
}

/// Files the scan walked past. `files_searched` counts candidates, so without
/// these a search that silently skipped the one file the user cared about is
/// indistinguishable from one that genuinely found nothing.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkippedFiles {
    pub too_large: usize,
    pub binary: usize,
    pub not_utf8: usize,
}

impl SkippedFiles {
    pub fn total(&self) -> usize {
        self.too_large + self.binary + self.not_utf8
    }

    /// Reasons that actually occurred, for a summary line that names them.
    pub fn reasons(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        for (count, label) in [
            (self.too_large, "too large"),
            (self.binary, "binary"),
            (self.not_utf8, "not UTF-8"),
        ] {
            if count > 0 {
                reasons.push(format!("{count} {label}"));
            }
        }
        reasons
    }
}

/// Why a search stopped short. A single boolean could not distinguish "you
/// asked for 50 results and there were more" from "PCRE2 gave up on your
/// pattern", and only the second means the results may be materially wrong.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Truncation {
    /// More files matched than `--max-results` asked for. Benign.
    pub results_capped: bool,
    /// At least one file hit the per-atom match cap.
    pub matches_capped: bool,
    /// PCRE2 abandoned a pattern in at least one file. Results may be wrong.
    pub pattern_gave_up: bool,
    pub matches_capped_files: usize,
    pub pattern_gave_up_files: usize,
}

impl Truncation {
    pub fn new(
        results_capped: bool,
        matches_capped_files: usize,
        pattern_gave_up_files: usize,
    ) -> Self {
        Self {
            results_capped,
            matches_capped: matches_capped_files > 0,
            pattern_gave_up: pattern_gave_up_files > 0,
            matches_capped_files,
            pattern_gave_up_files,
        }
    }

    pub fn any(&self) -> bool {
        self.results_capped || self.matches_capped || self.pattern_gave_up
    }

    /// Phrases for the summary line, most actionable first. `pattern_gave_up`
    /// leads because it is the one a user should act on.
    pub fn reasons(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        if self.pattern_gave_up {
            reasons.push(format!(
                "pattern too expensive in {} files",
                self.pattern_gave_up_files
            ));
        }
        if self.matches_capped {
            reasons.push(format!(
                "match cap reached in {} files",
                self.matches_capped_files
            ));
        }
        if self.results_capped {
            reasons.push("more results than --max-results".to_string());
        }
        reasons
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: Vec<String>,
    pub results: Vec<SearchResult>,
    pub repositories_searched: usize,
    pub files_searched: usize,
    /// End-to-end wall time for the whole request, synchronization included.
    pub elapsed_ms: u128,
    pub cached: bool,
    /// Derived from `truncation`. Kept because the browser interface and
    /// existing scripts read it.
    pub truncated: bool,
    #[serde(default)]
    pub truncation: Truncation,
    #[serde(default)]
    pub skipped_files: SkippedFiles,
    /// Results before `--max-results` was applied, so a zero limit can report
    /// the true count and the exit code stays honest.
    #[serde(default)]
    pub total_results: usize,
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub sync_ms: u128,
    #[serde(default)]
    pub scan_ms: u128,
    #[serde(default)]
    pub skipped: Vec<SkippedRepository>,
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
