use crate::{
    model::{MatchRange, ResultLine, SearchResponse, SearchResult, Snapshot},
    query::{CompiledQuery, QueryFingerprint},
};
use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchOptions {
    pub paths: Vec<String>,
    pub context: usize,
    pub max_results: usize,
    pub max_file_bytes: u64,
    pub sort: SortMode,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            paths: vec![],
            context: 2,
            max_results: 500,
            max_file_bytes: 4 * 1024 * 1024,
            sort: SortMode::Relevance,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SortMode {
    Relevance,
    Repo,
    Path,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchFingerprint {
    pub query: QueryFingerprint,
    pub options: SearchOptions,
}

pub struct SearchOutcome {
    pub response: SearchResponse,
}

pub fn run(
    query: &CompiledQuery,
    snapshots: &[Snapshot],
    options: &SearchOptions,
    cancelled: Arc<AtomicBool>,
) -> Result<SearchOutcome> {
    let started = Instant::now();
    let globset = build_globs(&options.paths)?;
    let positive = query.positive_atoms();
    let candidates = collect_candidates(snapshots, globset.as_ref(), options.max_file_bytes)?;
    let files_searched = candidates.len();
    let nested: Vec<Result<Option<SearchResult>>> = candidates
        .par_iter()
        .map(|(snapshot, path)| {
            if cancelled.load(Ordering::Relaxed) {
                return Ok(None);
            }
            search_file(query, snapshot, path, options.context, &positive)
        })
        .collect();
    let mut results = Vec::new();
    for result in nested {
        if let Some(result) = result? {
            results.push(result);
        }
    }
    match options.sort {
        SortMode::Relevance => results.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.repository.cmp(&b.repository))
                .then_with(|| a.path.cmp(&b.path))
        }),
        SortMode::Repo => results.sort_by(|a, b| {
            a.repository
                .cmp(&b.repository)
                .then_with(|| a.path.cmp(&b.path))
        }),
        SortMode::Path => results.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| a.repository.cmp(&b.repository))
        }),
    }
    let truncated = results.len() > options.max_results;
    results.truncate(options.max_results);
    Ok(SearchOutcome {
        response: SearchResponse {
            query: query.sources.clone(),
            results,
            repositories_searched: snapshots.len(),
            files_searched,
            elapsed_ms: started.elapsed().as_millis(),
            cached: false,
            truncated,
        },
    })
}

fn build_globs(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let normalized = pattern.replace('\\', "/");
        builder.add(
            GlobBuilder::new(&normalized)
                .literal_separator(true)
                .backslash_escape(false)
                .build()
                .with_context(|| format!("invalid path glob `{pattern}`"))?,
        );
    }
    Ok(Some(builder.build()?))
}

fn collect_candidates<'a>(
    snapshots: &'a [Snapshot],
    globs: Option<&GlobSet>,
    max_bytes: u64,
) -> Result<Vec<(&'a Snapshot, std::path::PathBuf)>> {
    let mut output = Vec::new();
    for snapshot in snapshots {
        for entry in WalkDir::new(&snapshot.checkout)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| entry.file_name() != ".git")
        {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.metadata()?.len() > max_bytes {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&snapshot.checkout)?
                .to_string_lossy()
                .replace('\\', "/");
            if globs.is_some_and(|set| !set.is_match(&relative)) {
                continue;
            }
            output.push((snapshot, entry.path().to_path_buf()));
        }
    }
    Ok(output)
}

fn search_file(
    query: &CompiledQuery,
    snapshot: &Snapshot,
    path: &Path,
    context: usize,
    positive: &BTreeSet<usize>,
) -> Result<Option<SearchResult>> {
    let bytes = fs::read(path)?;
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        return Ok(None);
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    let mut matches: Vec<Vec<(usize, usize)>> = Vec::with_capacity(query.atoms.len());
    for atom in &query.atoms {
        matches.push(atom.find_all(&bytes)?);
    }
    let present = matches
        .iter()
        .map(|items| !items.is_empty())
        .collect::<Vec<_>>();
    if !query.expression.evaluate(&present) {
        return Ok(None);
    }

    let line_starts = line_starts(&bytes);
    let mut wanted = BTreeSet::new();
    let mut line_ranges: HashMap<usize, Vec<MatchRange>> = HashMap::new();
    let mut match_count = 0;
    let mut first_match = usize::MAX;
    let mut first_per_atom = Vec::new();
    for atom in positive {
        if let Some((start, _)) = matches[*atom].first() {
            first_per_atom.push(*start);
        }
        for &(start, end) in &matches[*atom] {
            match_count += 1;
            first_match = first_match.min(start);
            let first_line = containing_line(&line_starts, start);
            let last_line = containing_line(&line_starts, end.saturating_sub(1));
            for line in first_line..=last_line {
                let content_start = line_starts[line];
                let content_end = line_starts.get(line + 1).copied().unwrap_or(bytes.len());
                line_ranges.entry(line).or_default().push(MatchRange {
                    start: start.max(content_start) - content_start,
                    end: end.min(content_end) - content_start,
                    atom: *atom,
                });
                let from = line.saturating_sub(context);
                let to = (line + context).min(line_starts.len().saturating_sub(1));
                wanted.extend(from..=to);
            }
        }
    }
    if match_count == 0 {
        return Ok(None);
    }
    let lines_text: Vec<&str> = text.split('\n').collect();
    let lines = wanted
        .into_iter()
        .filter_map(|index| {
            lines_text.get(index).map(|line| {
                let ranges = line_ranges.remove(&index).unwrap_or_default();
                ResultLine {
                    number: index + 1,
                    text: line.trim_end_matches('\r').to_string(),
                    is_context: ranges.is_empty(),
                    ranges,
                }
            })
        })
        .collect::<Vec<_>>();
    let relative = path
        .strip_prefix(&snapshot.checkout)?
        .to_string_lossy()
        .replace('\\', "/");
    let distinct = positive.iter().filter(|id| present[**id]).count();
    let density = match_count as f64 / lines_text.len().max(1) as f64;
    let spread = first_per_atom
        .iter()
        .max()
        .copied()
        .unwrap_or(first_match)
        .saturating_sub(first_per_atom.iter().min().copied().unwrap_or(first_match));
    let proximity =
        10.0 / (1.0 + spread as f64 / 120.0) + 1.0 / (1.0 + first_match as f64 / 4096.0);
    let path_bonus = query
        .atoms
        .iter()
        .filter(|atom| {
            atom.find_all(relative.as_bytes())
                .map(|m| !m.is_empty())
                .unwrap_or(false)
        })
        .count() as f64
        * 4.0;
    let generated_penalty = if relative.split('/').any(|part| {
        matches!(
            part.to_ascii_lowercase().as_str(),
            "vendor" | "generated" | "dist" | "build" | "node_modules"
        )
    }) {
        8.0
    } else {
        0.0
    };
    let score = distinct as f64 * 20.0
        + match_count.min(50) as f64
        + density * 10.0
        + proximity
        + path_bonus
        - generated_penalty;
    let first_line = lines
        .iter()
        .find(|line| !line.ranges.is_empty())
        .map(|line| line.number)
        .unwrap_or(1);
    let web_url = format!(
        "{}/src/{}/{}#lines-{}",
        snapshot.repository.web_url.trim_end_matches('/'),
        snapshot.commit,
        relative.replace(' ', "%20"),
        first_line
    );
    Ok(Some(SearchResult {
        repository: snapshot.repository.full_name.clone(),
        repository_name: snapshot.repository.name.clone(),
        path: relative,
        branch: snapshot.branch.clone(),
        commit: snapshot.commit.clone(),
        web_url,
        score,
        match_count,
        lines,
        stale: snapshot.stale,
    }))
}

fn line_starts(bytes: &[u8]) -> Vec<usize> {
    let mut output = vec![0];
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            output.push(index + 1);
        }
    }
    output
}

fn containing_line(starts: &[usize], offset: usize) -> usize {
    starts
        .partition_point(|start| *start <= offset)
        .saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::Repository, query::CaseMode};
    use chrono::Utc;
    use tempfile::tempdir;
    #[test]
    fn searches_boolean_expression_in_same_file() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("one.rs"), "fn alpha() {}\nlet beta = 1;\n").unwrap();
        fs::write(dir.path().join("two.rs"), "fn alpha() {}\n").unwrap();
        let snapshot = Snapshot {
            repository: Repository {
                uuid: "1".into(),
                workspace: "w".into(),
                slug: "r".into(),
                name: "r".into(),
                full_name: "w/r".into(),
                default_branch: Some("main".into()),
                clone_url: String::new(),
                web_url: "https://bitbucket.org/w/r".into(),
            },
            branch: "main".into(),
            commit: "abc".into(),
            synchronized_at: Utc::now(),
            checkout: dir.path().into(),
            stale: false,
        };
        let query =
            CompiledQuery::parse(&["alpha AND beta".into()], false, CaseMode::Sensitive).unwrap();
        let outcome = run(
            &query,
            &[snapshot],
            &SearchOptions::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(outcome.response.results.len(), 1);
        assert_eq!(outcome.response.results[0].path, "one.rs");
    }
}
