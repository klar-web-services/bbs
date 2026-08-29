use crate::{
    model::{
        MatchRange, ResultLine, SearchResponse, SearchResult, SkippedFiles, Snapshot, Truncation,
    },
    paths::{FilterCounts, PathFilter, VENDOR_DIRECTORIES, Verdict},
    query::{CompiledQuery, QueryFingerprint},
};
use anyhow::Result;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Instant,
};
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchOptions {
    pub paths: Vec<String>,
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    #[serde(default)]
    pub no_vendor: bool,
    pub context: usize,
    pub max_results: usize,
    pub max_file_bytes: u64,
    pub sort: SortMode,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            paths: vec![],
            exclude_paths: vec![],
            no_vendor: false,
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
    /// What the path filter did, so a filter that removed everything can be
    /// reported rather than looking like an empty result set.
    pub filter_counts: FilterCounts,
    pub filter: PathFilter,
}

/// Tallies kept while scanning. Files dropped for size, binary content or
/// encoding used to leave no trace at all, so a query that skipped the one file
/// it should have matched looked exactly like a query that found nothing.
#[derive(Default)]
struct ScanCounters {
    too_large: AtomicUsize,
    binary: AtomicUsize,
    not_utf8: AtomicUsize,
    /// Counted once per file, not once per atom.
    matches_capped_files: AtomicUsize,
    pattern_gave_up_files: AtomicUsize,
}

impl ScanCounters {
    fn bump(counter: &AtomicUsize) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn skipped_files(&self) -> SkippedFiles {
        SkippedFiles {
            too_large: self.too_large.load(Ordering::Relaxed),
            binary: self.binary.load(Ordering::Relaxed),
            not_utf8: self.not_utf8.load(Ordering::Relaxed),
        }
    }
}

pub fn run(
    query: &CompiledQuery,
    snapshots: &[Snapshot],
    options: &SearchOptions,
    cancelled: Arc<AtomicBool>,
) -> Result<SearchOutcome> {
    let started = Instant::now();
    let filter = PathFilter::new(&options.paths, &options.exclude_paths, options.no_vendor)?;
    let positive = query.positive_atoms();
    let counters = ScanCounters::default();
    let (candidates, filter_counts) =
        collect_candidates(snapshots, &filter, options.max_file_bytes, &counters)?;
    let files_searched = candidates.len();
    let nested: Vec<Result<Option<SearchResult>>> = candidates
        .par_iter()
        .map(|(snapshot, path)| {
            if cancelled.load(Ordering::Relaxed) {
                return Ok(None);
            }
            search_file(query, snapshot, path, options.context, &positive, &counters)
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
    let total_results = results.len();
    let truncation = Truncation::new(
        total_results > options.max_results,
        counters.matches_capped_files.load(Ordering::Relaxed),
        counters.pattern_gave_up_files.load(Ordering::Relaxed),
    );
    results.truncate(options.max_results);
    let elapsed_ms = started.elapsed().as_millis();
    Ok(SearchOutcome {
        filter_counts,
        filter,
        response: SearchResponse {
            query: query.sources.clone(),
            results,
            repositories_searched: snapshots.len(),
            files_searched,
            elapsed_ms,
            cached: false,
            truncated: truncation.any(),
            truncation,
            skipped_files: counters.skipped_files(),
            total_results,
            offline: false,
            sync_ms: 0,
            scan_ms: elapsed_ms,
            skipped: Vec::new(),
        },
    })
}

fn collect_candidates<'a>(
    snapshots: &'a [Snapshot],
    filter: &PathFilter,
    max_bytes: u64,
    counters: &ScanCounters,
) -> Result<(Vec<(&'a Snapshot, std::path::PathBuf)>, FilterCounts)> {
    let mut output = Vec::new();
    let mut counts = FilterCounts::default();
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
                ScanCounters::bump(&counters.too_large);
                continue;
            }
            counts.considered += 1;
            let relative = entry
                .path()
                .strip_prefix(&snapshot.checkout)?
                .to_string_lossy()
                .replace('\\', "/");
            match filter.verdict(&relative) {
                Verdict::Selected => counts.selected += 1,
                Verdict::DroppedByInclude => {
                    counts.dropped_by_include += 1;
                    continue;
                }
                Verdict::DroppedByExclude => {
                    counts.dropped_by_exclude += 1;
                    continue;
                }
            }
            output.push((snapshot, entry.path().to_path_buf()));
        }
    }
    Ok((output, counts))
}

fn search_file(
    query: &CompiledQuery,
    snapshot: &Snapshot,
    path: &Path,
    context: usize,
    positive: &BTreeSet<usize>,
    counters: &ScanCounters,
) -> Result<Option<SearchResult>> {
    let bytes = fs::read(path)?;
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        ScanCounters::bump(&counters.binary);
        return Ok(None);
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => {
            ScanCounters::bump(&counters.not_utf8);
            return Ok(None);
        }
    };
    let mut matches: Vec<Vec<(usize, usize)>> = Vec::with_capacity(query.atoms.len());
    let mut capped_here = false;
    let mut gave_up_here = false;
    for atom in &query.atoms {
        let found = atom.find_all(&bytes);
        capped_here |= found.capped;
        gave_up_here |= found.gave_up;
        matches.push(found.spans);
    }
    if capped_here {
        ScanCounters::bump(&counters.matches_capped_files);
    }
    if gave_up_here {
        ScanCounters::bump(&counters.pattern_gave_up_files);
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
                // Clamp to the text actually rendered for this line, which is
                // the line without its terminator. A match that crosses a line
                // break otherwise reports an end one or two bytes past the end
                // of `ResultLine::text`, and every consumer that slices by
                // those offsets is handed an out-of-range index.
                let content_end = line_text_end(&bytes, &line_starts, line);
                let range_start = start.max(content_start) - content_start;
                let range_end = end.min(content_end).saturating_sub(content_start);
                line_ranges.entry(line).or_default().push(MatchRange {
                    start: range_start,
                    end: range_end.max(range_start),
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
        .filter(|atom| !atom.find_all(relative.as_bytes()).spans.is_empty())
        .count() as f64
        * 4.0;
    let generated_penalty = if relative
        .split('/')
        .any(|part| VENDOR_DIRECTORIES.contains(&part.to_ascii_lowercase().as_str()))
    {
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
        encode_path(&relative),
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

/// Percent-encodes a repository-relative path for use in a permalink, one
/// segment at a time so the separators survive. Escaping only spaces left `#`
/// to start a URL fragment and `%` to be re-decoded, both of which point the
/// link at a different file - or at no file at all.
fn encode_path(relative: &str) -> String {
    relative
        .split('/')
        .map(|segment| urlencoding::encode(segment).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// End offset of the text `ResultLine` will carry for `line`: the line without
/// its `\n`, and without a preceding `\r` on CRLF input.
fn line_text_end(bytes: &[u8], starts: &[usize], line: usize) -> usize {
    let start = starts[line];
    let mut end = starts.get(line + 1).copied().unwrap_or(bytes.len());
    if end > start && bytes.get(end - 1) == Some(&b'\n') {
        end -= 1;
    }
    if end > start && bytes.get(end - 1) == Some(&b'\r') {
        end -= 1;
    }
    end
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
    fn snapshot_of(dir: &Path) -> Snapshot {
        Snapshot {
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
            checkout: dir.into(),
            stale: false,
        }
    }

    /// Every reported range must index inside the line text it belongs to.
    /// Matches that cross a line break, and matches that touch the `\r` of a
    /// CRLF pair, used to report an end past the end of that text.
    #[test]
    fn ranges_stay_inside_the_line_text_they_belong_to() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("crlf.txt"), "alpha beta\r\ngamma\r\n").unwrap();
        fs::write(dir.path().join("lf.txt"), "alpha\nbeta\n").unwrap();
        fs::write(dir.path().join("bare.txt"), "alpha\nbeta").unwrap();
        let snapshot = snapshot_of(dir.path());
        for source in ["/alpha[\\s\\S]*beta/", "/beta\\s/", "/alpha.*/s"] {
            let query =
                CompiledQuery::parse(&[source.into()], false, CaseMode::Sensitive, false).unwrap();
            let outcome = run(
                &query,
                std::slice::from_ref(&snapshot),
                &SearchOptions::default(),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
            for result in &outcome.response.results {
                for line in &result.lines {
                    for range in &line.ranges {
                        assert!(
                            range.start <= range.end && range.end <= line.text.len(),
                            "{source} on {} line {}: {}..{} outside 0..{} ({:?})",
                            result.path,
                            line.number,
                            range.start,
                            range.end,
                            line.text.len(),
                            line.text
                        );
                        assert!(
                            line.text.is_char_boundary(range.start)
                                && line.text.is_char_boundary(range.end),
                            "{source}: range is not on a character boundary"
                        );
                    }
                }
            }
        }
    }

    /// A permalink must survive characters that are structural in a URL.
    #[test]
    fn permalink_paths_are_percent_encoded_per_segment() {
        assert_eq!(encode_path("src/main.rs"), "src/main.rs");
        assert_eq!(encode_path("a dir/a file.txt"), "a%20dir/a%20file.txt");
        assert_eq!(encode_path("hash#name.txt"), "hash%23name.txt");
        assert_eq!(encode_path("percent%20name.txt"), "percent%2520name.txt");
        assert_eq!(encode_path("q?uery.txt"), "q%3Fuery.txt");
        assert_eq!(
            encode_path("deep/nested/path/file-1_2.3.txt"),
            "deep/nested/path/file-1_2.3.txt"
        );
        assert!(!encode_path("日本語.txt").contains('日'));
    }

    /// A file dropped for size, binary content or encoding used to leave no
    /// trace, so a query that skipped the very file it should have matched was
    /// indistinguishable from one that found nothing.
    #[test]
    fn files_skipped_by_the_scan_are_counted_by_reason() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("wanted.txt"), "needle here\n").unwrap();
        fs::write(dir.path().join("huge.txt"), "needle ".repeat(4096)).unwrap();
        fs::write(dir.path().join("binary.bin"), b"needle\x00\x01\x02").unwrap();
        fs::write(dir.path().join("latin1.txt"), b"needle \xff\xfe rest").unwrap();
        let snapshot = snapshot_of(dir.path());
        let query =
            CompiledQuery::parse(&["needle".into()], false, CaseMode::Sensitive, false).unwrap();
        let outcome = run(
            &query,
            std::slice::from_ref(&snapshot),
            &SearchOptions {
                max_file_bytes: 1024,
                ..Default::default()
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();

        assert_eq!(outcome.response.results.len(), 1);
        assert_eq!(outcome.response.results[0].path, "wanted.txt");
        assert_eq!(
            outcome.response.skipped_files,
            crate::model::SkippedFiles {
                too_large: 1,
                binary: 1,
                not_utf8: 1,
            }
        );
        // the candidate count must exclude the oversized file it never opened
        assert_eq!(outcome.response.files_searched, 3);
        assert_eq!(outcome.response.total_results, 1);
    }

    /// The three ways a search can stop short mean different things, and only
    /// one of them says the results may be wrong.
    #[test]
    fn truncation_distinguishes_a_results_cap_from_an_abandoned_pattern() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        fs::write(dir.path().join("b.txt"), "needle\n").unwrap();
        let snapshot = snapshot_of(dir.path());
        let query =
            CompiledQuery::parse(&["needle".into()], false, CaseMode::Sensitive, false).unwrap();
        let outcome = run(
            &query,
            std::slice::from_ref(&snapshot),
            &SearchOptions {
                max_results: 1,
                ..Default::default()
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();

        assert_eq!(outcome.response.results.len(), 1);
        assert_eq!(outcome.response.total_results, 2);
        assert!(outcome.response.truncation.results_capped);
        assert!(!outcome.response.truncation.matches_capped);
        assert!(!outcome.response.truncation.pattern_gave_up);
        // the compatibility boolean still summarises all three
        assert!(outcome.response.truncated);
    }

    /// The headline path-filter behaviours, end to end. `--path '*.md'` used
    /// to find only the root-level file, and there was no way at all to
    /// exclude a directory.
    #[test]
    fn path_filters_widen_by_depth_and_narrow_by_exclusion() {
        let dir = tempdir().unwrap();
        for (path, text) in [
            ("README.md", "needle\n"),
            ("docs/guide.md", "needle\n"),
            ("src/main.rs", "needle\n"),
            ("vendor/dep.rs", "needle\n"),
            ("src/test/case.rs", "needle\n"),
        ] {
            let file = dir.path().join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, text).unwrap();
        }
        let snapshot = snapshot_of(dir.path());
        let query =
            CompiledQuery::parse(&["needle".into()], false, CaseMode::Sensitive, false).unwrap();
        let found = |options: SearchOptions| {
            let outcome = run(
                &query,
                std::slice::from_ref(&snapshot),
                &options,
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
            let mut paths: Vec<String> = outcome
                .response
                .results
                .iter()
                .map(|result| result.path.clone())
                .collect();
            paths.sort();
            paths
        };

        // a pattern with no separator reaches every depth
        assert_eq!(
            found(SearchOptions {
                paths: vec!["*.md".into()],
                ..Default::default()
            }),
            ["README.md", "docs/guide.md"]
        );
        // and `./` is the way back to the repository root alone
        assert_eq!(
            found(SearchOptions {
                paths: vec!["./*.md".into()],
                ..Default::default()
            }),
            ["README.md"]
        );
        // spellings that used to match nothing without saying so
        assert_eq!(
            found(SearchOptions {
                paths: vec!["src/".into()],
                ..Default::default()
            }),
            ["src/main.rs", "src/test/case.rs"]
        );
        assert_eq!(
            found(SearchOptions {
                paths: vec!["./src/**".into()],
                ..Default::default()
            }),
            ["src/main.rs", "src/test/case.rs"]
        );
        // exclusion, in both spellings
        assert_eq!(
            found(SearchOptions {
                exclude_paths: vec!["**/test/**".into()],
                paths: vec!["*.rs".into()],
                ..Default::default()
            }),
            ["src/main.rs", "vendor/dep.rs"]
        );
        assert_eq!(
            found(SearchOptions {
                paths: vec!["*.rs".into(), "!vendor/**".into()],
                ..Default::default()
            }),
            ["src/main.rs", "src/test/case.rs"]
        );
        assert_eq!(
            found(SearchOptions {
                paths: vec!["*.rs".into()],
                no_vendor: true,
                ..Default::default()
            }),
            ["src/main.rs", "src/test/case.rs"]
        );
    }

    /// A filter that eliminates everything must say so rather than looking
    /// like an empty result set.
    #[test]
    fn a_path_filter_that_selects_nothing_is_reported() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "needle\n").unwrap();
        let snapshot = snapshot_of(dir.path());
        let query =
            CompiledQuery::parse(&["needle".into()], false, CaseMode::Sensitive, false).unwrap();
        let outcome = run(
            &query,
            std::slice::from_ref(&snapshot),
            &SearchOptions {
                paths: vec!["nowhere/**".into()],
                ..Default::default()
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert!(outcome.response.results.is_empty());
        let warning = outcome
            .filter
            .empty_result_warning(&outcome.filter_counts)
            .expect("a filter that selected nothing must be reported");
        assert!(warning.contains("nowhere/**"), "{warning}");
        assert!(warning.contains('1'), "{warning}");
    }

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
        let query = CompiledQuery::parse(
            &["alpha AND beta".into()],
            false,
            CaseMode::Sensitive,
            false,
        )
        .unwrap();
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
