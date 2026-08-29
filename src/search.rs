use crate::{
    model::{
        MatchRange, ResultLine, SearchResponse, SearchResult, SkippedFiles, Snapshot, Truncation,
    },
    paths::{FilterCounts, PathFilter, VENDOR_DIRECTORIES, Verdict},
    query::CompiledQuery,
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

/// What a search actually scans. This is the whole of the cache key beyond the
/// query and the snapshot commits: change any of it and a stored result set is
/// no longer a superset of what is being asked for.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanOptions {
    pub paths: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub no_vendor: bool,
    pub max_file_bytes: u64,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            paths: vec![],
            exclude_paths: vec![],
            no_vendor: false,
            max_file_bytes: 4 * 1024 * 1024,
        }
    }
}

/// How a scan is shown. Deliberately absent from the cache key: sorting,
/// limiting and trimming context are all things a stored scan can be re-asked
/// for without touching a single file again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presentation {
    pub sort: SortMode,
    pub max_results: usize,
    pub context: usize,
}

impl Default for Presentation {
    fn default() -> Self {
        Self {
            sort: SortMode::Relevance,
            max_results: 500,
            context: 2,
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

/// One scan, stored independently of how it was displayed.
///
/// `stored_context`, `stored_limit` and `complete` record what this body can
/// still answer for. A request for narrower context or fewer results is served
/// by trimming; anything wider is a miss and rescans.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedScan {
    pub results: Vec<SearchResult>,
    /// Matching files found, before `stored_limit` was applied.
    pub total_results: usize,
    pub stored_context: usize,
    pub stored_limit: usize,
    /// Whether `results` holds every match. When it does, any sort order can
    /// be served from it; when it does not, `results` is the top
    /// `stored_limit` in `stored_sort` order and only that order is sound.
    pub complete: bool,
    pub stored_sort: SortMode,
    pub repositories_searched: usize,
    pub files_searched: usize,
    pub skipped_files: SkippedFiles,
    pub matches_capped_files: usize,
    pub pattern_gave_up_files: usize,
    pub scan_ms: u128,
}

impl CachedScan {
    /// Whether this body can answer `presentation` without rescanning.
    pub fn satisfies(&self, presentation: &Presentation) -> bool {
        presentation.context <= self.stored_context
            && presentation.max_results <= self.stored_limit
            && (self.complete || presentation.sort == self.stored_sort)
    }
}

pub struct ScanOutcome {
    pub scan: CachedScan,
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

fn sort_results(results: &mut [SearchResult], sort: SortMode) {
    match sort {
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
}

/// Narrows a stored result to `context` lines around each match.
///
/// `ResultLine` already carries absolute line numbers and an `is_context`
/// flag, so a wider stored context can be trimmed to a narrower requested one
/// without reopening the file.
fn trim_context(result: &mut SearchResult, context: usize) {
    let matched: Vec<usize> = result
        .lines
        .iter()
        .filter(|line| !line.is_context)
        .map(|line| line.number)
        .collect();
    result.lines.retain(|line| {
        !line.is_context
            || matched
                .iter()
                .any(|number| line.number.abs_diff(*number) <= context)
    });
}

/// Renders a stored scan at the requested sort, limit and context. This is the
/// whole of what `--sort`, `--max-results` and `--context` do, which is why
/// none of them belongs in the cache key: changing one used to rescan 383,000
/// files to reach the same answer in a different order.
pub fn present(scan: &CachedScan, presentation: &Presentation, query: &[String]) -> SearchResponse {
    let mut results = scan.results.clone();
    sort_results(&mut results, presentation.sort);
    results.truncate(presentation.max_results);
    for result in &mut results {
        trim_context(result, presentation.context);
    }
    let truncation = Truncation::new(
        scan.total_results > presentation.max_results,
        scan.matches_capped_files,
        scan.pattern_gave_up_files,
    );
    SearchResponse {
        query: query.to_vec(),
        results,
        repositories_searched: scan.repositories_searched,
        files_searched: scan.files_searched,
        elapsed_ms: scan.scan_ms,
        cached: false,
        truncated: truncation.any(),
        truncation,
        skipped_files: scan.skipped_files,
        total_results: scan.total_results,
        offline: false,
        sync_ms: 0,
        scan_ms: scan.scan_ms,
        skipped: Vec::new(),
    }
}

pub fn run(
    query: &CompiledQuery,
    snapshots: &[Snapshot],
    options: &ScanOptions,
    stored: &Presentation,
    cancelled: Arc<AtomicBool>,
) -> Result<ScanOutcome> {
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
            search_file(query, snapshot, path, stored.context, &positive, &counters)
        })
        .collect();
    let mut results = Vec::new();
    for result in nested {
        if let Some(result) = result? {
            results.push(result);
        }
    }
    sort_results(&mut results, stored.sort);
    let total_results = results.len();
    let complete = total_results <= stored.max_results;
    results.truncate(stored.max_results);
    Ok(ScanOutcome {
        filter_counts,
        filter,
        scan: CachedScan {
            results,
            total_results,
            stored_context: stored.context,
            stored_limit: stored.max_results,
            complete,
            stored_sort: stored.sort,
            repositories_searched: snapshots.len(),
            files_searched,
            skipped_files: counters.skipped_files(),
            matches_capped_files: counters.matches_capped_files.load(Ordering::Relaxed),
            pattern_gave_up_files: counters.pattern_gave_up_files.load(Ordering::Relaxed),
            scan_ms: started.elapsed().as_millis(),
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

    /// Scans with the given options at a generous stored presentation, the way
    /// the application does before narrowing for display.
    fn scan_with(
        query: &CompiledQuery,
        snapshots: &[Snapshot],
        options: ScanOptions,
    ) -> ScanOutcome {
        run(
            query,
            snapshots,
            &options,
            &Presentation {
                sort: SortMode::Relevance,
                max_results: 2000,
                context: 6,
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap()
    }

    fn shown(
        query: &CompiledQuery,
        snapshots: &[Snapshot],
        options: ScanOptions,
        presentation: Presentation,
    ) -> SearchResponse {
        let outcome = scan_with(query, snapshots, options);
        present(&outcome.scan, &presentation, &query.sources)
    }
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
            let response = shown(
                &query,
                std::slice::from_ref(&snapshot),
                ScanOptions::default(),
                Presentation::default(),
            );
            for result in &response.results {
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
        let response = shown(
            &query,
            std::slice::from_ref(&snapshot),
            ScanOptions {
                max_file_bytes: 1024,
                ..Default::default()
            },
            Presentation::default(),
        );

        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].path, "wanted.txt");
        assert_eq!(
            response.skipped_files,
            crate::model::SkippedFiles {
                too_large: 1,
                binary: 1,
                not_utf8: 1,
            }
        );
        // the candidate count must exclude the oversized file it never opened
        assert_eq!(response.files_searched, 3);
        assert_eq!(response.total_results, 1);
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
        let response = shown(
            &query,
            std::slice::from_ref(&snapshot),
            ScanOptions::default(),
            Presentation {
                max_results: 1,
                ..Default::default()
            },
        );

        assert_eq!(response.results.len(), 1);
        assert_eq!(response.total_results, 2);
        assert!(response.truncation.results_capped);
        assert!(!response.truncation.matches_capped);
        assert!(!response.truncation.pattern_gave_up);
        // the compatibility boolean still summarises all three
        assert!(response.truncated);
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
        let found = |options: ScanOptions| {
            let response = shown(
                &query,
                std::slice::from_ref(&snapshot),
                options,
                Presentation::default(),
            );
            let mut paths: Vec<String> = response
                .results
                .iter()
                .map(|result| result.path.clone())
                .collect();
            paths.sort();
            paths
        };

        // a pattern with no separator reaches every depth
        assert_eq!(
            found(ScanOptions {
                paths: vec!["*.md".into()],
                ..Default::default()
            }),
            ["README.md", "docs/guide.md"]
        );
        // and `./` is the way back to the repository root alone
        assert_eq!(
            found(ScanOptions {
                paths: vec!["./*.md".into()],
                ..Default::default()
            }),
            ["README.md"]
        );
        // spellings that used to match nothing without saying so
        assert_eq!(
            found(ScanOptions {
                paths: vec!["src/".into()],
                ..Default::default()
            }),
            ["src/main.rs", "src/test/case.rs"]
        );
        assert_eq!(
            found(ScanOptions {
                paths: vec!["./src/**".into()],
                ..Default::default()
            }),
            ["src/main.rs", "src/test/case.rs"]
        );
        // exclusion, in both spellings
        assert_eq!(
            found(ScanOptions {
                exclude_paths: vec!["**/test/**".into()],
                paths: vec!["*.rs".into()],
                ..Default::default()
            }),
            ["src/main.rs", "vendor/dep.rs"]
        );
        assert_eq!(
            found(ScanOptions {
                paths: vec!["*.rs".into(), "!vendor/**".into()],
                ..Default::default()
            }),
            ["src/main.rs", "src/test/case.rs"]
        );
        assert_eq!(
            found(ScanOptions {
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
        let outcome = scan_with(
            &query,
            std::slice::from_ref(&snapshot),
            ScanOptions {
                paths: vec!["nowhere/**".into()],
                ..Default::default()
            },
        );
        assert!(outcome.scan.results.is_empty());
        let warning = outcome
            .filter
            .empty_result_warning(&outcome.filter_counts)
            .expect("a filter that selected nothing must be reported");
        assert!(warning.contains("nowhere/**"), "{warning}");
        assert!(warning.contains('1'), "{warning}");
    }

    /// A stored scan carries generous context; a narrower request is answered
    /// by dropping the context lines that fall outside it, without reopening a
    /// single file.
    #[test]
    fn context_narrows_from_a_stored_scan_without_rescanning() {
        let dir = tempdir().unwrap();
        let body: String = (1..=21)
            .map(|n| {
                if n == 11 {
                    "needle\n".to_string()
                } else {
                    format!("line {n}\n")
                }
            })
            .collect();
        fs::write(dir.path().join("a.txt"), body).unwrap();
        let snapshot = snapshot_of(dir.path());
        let query =
            CompiledQuery::parse(&["needle".into()], false, CaseMode::Sensitive, false).unwrap();
        let outcome = scan_with(
            &query,
            std::slice::from_ref(&snapshot),
            ScanOptions::default(),
        );
        // stored at six lines either side of the single match on line 11
        assert_eq!(outcome.scan.stored_context, 6);
        let numbers = |response: &SearchResponse| {
            response.results[0]
                .lines
                .iter()
                .map(|line| line.number)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            numbers(&present(
                &outcome.scan,
                &Presentation {
                    context: 6,
                    ..Default::default()
                },
                &query.sources
            )),
            (5..=17).collect::<Vec<_>>()
        );
        assert_eq!(
            numbers(&present(
                &outcome.scan,
                &Presentation {
                    context: 1,
                    ..Default::default()
                },
                &query.sources
            )),
            vec![10, 11, 12]
        );
        assert_eq!(
            numbers(&present(
                &outcome.scan,
                &Presentation {
                    context: 0,
                    ..Default::default()
                },
                &query.sources
            )),
            vec![11]
        );
    }

    /// A scan that holds every match can be re-displayed in any order. One
    /// that had to stop at the stored limit can only be trusted in the order
    /// it was stored in, because a different order would pick different rows.
    #[test]
    fn a_complete_scan_satisfies_any_sort_and_an_incomplete_one_does_not() {
        let dir = tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(dir.path().join(name), "needle\n").unwrap();
        }
        let snapshot = snapshot_of(dir.path());
        let query =
            CompiledQuery::parse(&["needle".into()], false, CaseMode::Sensitive, false).unwrap();

        let complete = scan_with(
            &query,
            std::slice::from_ref(&snapshot),
            ScanOptions::default(),
        );
        assert!(complete.scan.complete);
        assert!(complete.scan.satisfies(&Presentation {
            sort: SortMode::Path,
            max_results: 500,
            context: 2,
        }));

        let clipped = run(
            &query,
            std::slice::from_ref(&snapshot),
            &ScanOptions::default(),
            &Presentation {
                sort: SortMode::Relevance,
                max_results: 2,
                context: 6,
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert!(!clipped.scan.complete);
        assert_eq!(clipped.scan.total_results, 3);
        assert!(clipped.scan.satisfies(&Presentation {
            sort: SortMode::Relevance,
            max_results: 2,
            context: 2,
        }));
        assert!(!clipped.scan.satisfies(&Presentation {
            sort: SortMode::Path,
            max_results: 2,
            context: 2,
        }));
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
        let response = shown(
            &query,
            &[snapshot],
            ScanOptions::default(),
            Presentation::default(),
        );
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].path, "one.rs");
    }
}
