use crate::{
    cli::{ColorChoice, OutputFormat},
    model::{
        ResultLine, SearchResponse, SearchResult, SkippedFiles, SkippedRepository, Truncation,
    },
};
use anyhow::Result;
use serde::Serialize;
use std::io::{self, IsTerminal, Write};
use syntect::{
    easy::HighlightLines,
    highlighting::{Style, ThemeSet},
    parsing::{SyntaxReference, SyntaxSet},
};

/// What to show for each match. `--files-with-matches` and `--count` answer
/// distinct, cheaper questions than "show me the code", and they render from
/// the same stored scan as a full search rather than forcing one of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Full,
    FilesWithMatches,
    Count,
}

#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    pub format: OutputFormat,
    pub color: ColorChoice,
    pub mode: OutputMode,
    /// Break the result list into per-repository sections. Seventy
    /// repositories of `--sort repo` output is otherwise an undifferentiated
    /// wall.
    pub group_by_repository: bool,
    pub stats: bool,
}

/// A `jsonl` result line. The result's own fields stay at the top level so
/// `jq -r .repository` keeps working; `type` is what lets a consumer tell a
/// result apart from the summary that follows them.
#[derive(Serialize)]
struct JsonlResult<'a> {
    r#type: &'static str,
    #[serde(flatten)]
    result: &'a SearchResult,
}

/// The last `jsonl` line. Everything a script should react to -- what was
/// skipped, why the search stopped early, how long each phase took -- was
/// unreachable in the streaming format, which is the one most likely to be
/// consumed by a script.
#[derive(Serialize)]
struct JsonlSummary<'a> {
    r#type: &'static str,
    query: &'a [String],
    total_results: usize,
    results_shown: usize,
    repositories_searched: usize,
    files_searched: usize,
    skipped_files: SkippedFiles,
    skipped: &'a [SkippedRepository],
    truncation: Truncation,
    truncated: bool,
    cached: bool,
    offline: bool,
    elapsed_ms: u128,
    sync_ms: u128,
    scan_ms: u128,
}

pub fn render(response: &SearchResponse, options: RenderOptions) -> Result<()> {
    // Context lines are not part of the answer to "which files" or "how many",
    // so they are not published in those modes either.
    let stripped;
    let response = if options.mode == OutputMode::Full {
        response
    } else {
        stripped = without_lines(response);
        &stripped
    };
    match options.format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(response)?),
        OutputFormat::Jsonl => {
            let mut stdout = io::BufWriter::new(io::stdout().lock());
            for line in jsonl_lines(response)? {
                writeln!(stdout, "{line}")?;
            }
        }
        OutputFormat::Terminal => render_terminal(response, should_color(options.color), &options)?,
    }
    Ok(())
}

/// One line per result, then one summary line. `truncated`, `skipped`,
/// `files_searched` and the repository-skip list were unreachable in the
/// streaming format, which is the one most likely to be consumed by a script
/// that should react to them.
pub fn jsonl_lines(response: &SearchResponse) -> Result<Vec<String>> {
    let mut lines = Vec::with_capacity(response.results.len() + 1);
    for result in &response.results {
        lines.push(serde_json::to_string(&JsonlResult {
            r#type: "result",
            result,
        })?);
    }
    lines.push(serde_json::to_string(&JsonlSummary {
        r#type: "summary",
        query: &response.query,
        total_results: response.total_results,
        results_shown: response.results.len(),
        repositories_searched: response.repositories_searched,
        files_searched: response.files_searched,
        skipped_files: response.skipped_files,
        skipped: &response.skipped,
        truncation: response.truncation,
        truncated: response.truncated,
        cached: response.cached,
        offline: response.offline,
        elapsed_ms: response.elapsed_ms,
        sync_ms: response.sync_ms,
        scan_ms: response.scan_ms,
    })?);
    Ok(lines)
}

fn without_lines(response: &SearchResponse) -> SearchResponse {
    let mut copy = response.clone();
    for result in &mut copy.results {
        result.lines.clear();
    }
    copy
}

fn should_color(choice: ColorChoice) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => io::stdout().is_terminal(),
    }
}

/// The stderr twin of [`should_color`].
///
/// A separate function, not a parameter on the existing one, because the
/// banner goes to stderr while results go to stdout. Reusing the stdout
/// version would decide the banner's colour from whether *stdout* is a
/// terminal — wrong in exactly the `bbs … --format json > out.json` case,
/// where stdout is a file and stderr is still the user's terminal.
///
/// Public because `main.rs` is a separate binary crate.
pub fn should_color_stderr(choice: ColorChoice) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => io::stderr().is_terminal(),
    }
}

fn render_terminal(response: &SearchResponse, color: bool, options: &RenderOptions) -> Result<()> {
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    match options.mode {
        OutputMode::FilesWithMatches => {
            for result in &response.results {
                writeln!(stdout, "{}", heading(result, color, false))?;
            }
        }
        OutputMode::Count => {
            for result in &response.results {
                writeln!(
                    stdout,
                    "{}  {}",
                    heading(result, color, false),
                    result.match_count
                )?;
            }
            writeln!(stdout, "{}", count_summary(response))?;
        }
        OutputMode::Full => render_matches(&mut stdout, response, color, options)?,
    }
    writeln!(stdout, "{}", summary_line(response))?;
    if options.stats {
        write!(stdout, "{}", stats_block(response))?;
    }
    Ok(())
}

pub fn count_summary(response: &SearchResponse) -> String {
    let matches: usize = response
        .results
        .iter()
        .map(|result| result.match_count)
        .sum();
    format!(
        "{matches} matches in {} files across {} repositories",
        response.results.len(),
        distinct_repositories(response)
    )
}

fn distinct_repositories(response: &SearchResponse) -> usize {
    let mut names: Vec<&str> = response
        .results
        .iter()
        .map(|result| result.repository.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();
    names.len()
}

fn heading(result: &SearchResult, color: bool, with_commit: bool) -> String {
    let tail = if with_commit {
        let commit = &result.commit[..result.commit.len().min(12)];
        if color {
            format!("  \x1b[2m{}@{commit}\x1b[0m", result.branch)
        } else {
            format!("  {}@{commit}", result.branch)
        }
    } else {
        String::new()
    };
    if color {
        format!(
            "\x1b[1;36m{}\x1b[0m  \x1b[1m{}\x1b[0m{tail}",
            result.repository, result.path
        )
    } else {
        format!("{}  {}{tail}", result.repository, result.path)
    }
}

fn render_matches(
    stdout: &mut impl Write,
    response: &SearchResponse,
    color: bool,
    options: &RenderOptions,
) -> Result<()> {
    let syntax_set = two_face::syntax::extra_newlines();
    let themes = ThemeSet::load_defaults();
    let theme = themes
        .themes
        .get("base16-ocean.dark")
        .or_else(|| themes.themes.values().next())
        .expect("syntect includes a theme");
    let mut current_repository: Option<&str> = None;
    for result in &response.results {
        // Grouping is only sound when the list is ordered by repository;
        // otherwise a repository would get a header every time it recurred.
        if options.group_by_repository && current_repository != Some(result.repository.as_str()) {
            current_repository = Some(result.repository.as_str());
            let files = response
                .results
                .iter()
                .filter(|other| other.repository == result.repository)
                .count();
            let matches: usize = response
                .results
                .iter()
                .filter(|other| other.repository == result.repository)
                .map(|other| other.match_count)
                .sum();
            let header = format!("{} ({files} files, {matches} matches)", result.repository);
            if color {
                writeln!(stdout, "\x1b[1;36m{header}\x1b[0m")?;
            } else {
                writeln!(stdout, "{header}")?;
            }
        }
        writeln!(stdout, "{}", heading(result, color, true))?;
        let syntax = syntax_for_result_path(&syntax_set, &result.path);
        let mut highlighter = HighlightLines::new(syntax, theme);
        let width = result
            .lines
            .last()
            .map(|l| l.number.to_string().len())
            .unwrap_or(1);
        let mut previous = None;
        for line in &result.lines {
            if previous.is_some_and(|number| line.number > number + 1) {
                writeln!(stdout, "{:>width$}  …", "", width = width)?;
            }
            let marker = if line.ranges.is_empty() { " " } else { ">" };
            write!(
                stdout,
                "{} {:>width$} │ ",
                marker,
                line.number,
                width = width
            )?;
            if color {
                let source_line = format!("{}\n", line.text);
                let styled = highlighter.highlight_line(&source_line, &syntax_set)?;
                render_styled(stdout, &styled, line)?;
            } else {
                writeln!(stdout, "{}", line.text)?;
            }
            previous = Some(line.number);
        }
        if color {
            writeln!(stdout, "\x1b[2;4m{}\x1b[0m\n", result.web_url)?;
        } else {
            writeln!(stdout, "{}\n", result.web_url)?;
        }
    }
    Ok(())
}

/// Fetching and scanning are very different costs on a large workspace, and
/// one `elapsed_ms` could not tell them apart. `sync` covers repository
/// discovery as well as the fetches. The same numbers are always present in
/// JSON output; `--stats` is what puts them on a terminal.
pub fn stats_block(response: &SearchResponse) -> String {
    let mut scan = format!("{} files", response.files_searched);
    if response.skipped_files.total() > 0 {
        scan.push_str(&format!(
            ", {} skipped ({})",
            response.skipped_files.total(),
            response.skipped_files.reasons().join(", ")
        ));
    }
    let mut sync = format!("{} repositories", response.repositories_searched);
    if !response.skipped.is_empty() {
        sync.push_str(&format!(", {} skipped", response.skipped.len()));
    }
    format!(
        "sync  {:>8} ms   {sync}\nscan  {:>8} ms   {scan}\ntotal {:>8} ms   {} matched, {} shown\n",
        response.sync_ms,
        response.scan_ms,
        response.elapsed_ms,
        response.total_results,
        response.results.len(),
    )
}

/// The one line that says what happened. It has to carry three things the user
/// cannot otherwise see: files the scan walked past, results the display
/// dropped, and repositories that could not contribute at all.
pub fn summary_line(response: &SearchResponse) -> String {
    let mut inside = vec![format!("{} files", response.files_searched)];
    let skipped_files = response.skipped_files;
    if skipped_files.total() > 0 {
        inside.push(format!(
            "{} skipped: {}",
            skipped_files.total(),
            skipped_files.reasons().join(", ")
        ));
    }
    inside.push(format!("{} ms", response.elapsed_ms));
    if response.cached {
        inside.push("cache hit".into());
    }
    let shown = if response.total_results > response.results.len() {
        format!(
            "{} of {} results",
            response.results.len(),
            response.total_results
        )
    } else {
        format!("{} results", response.results.len())
    };
    let mut line = format!(
        "{shown} across {} repositories ({})",
        response.repositories_searched,
        inside.join(", ")
    );
    let reasons = response.truncation.reasons();
    if !reasons.is_empty() {
        line.push_str(&format!("; stopped early: {}", reasons.join("; ")));
    }
    if !response.skipped.is_empty() {
        line.push_str(&format!(
            "; {} repositories skipped",
            response.skipped.len()
        ));
    }
    line
}

fn syntax_for_result_path<'a>(syntax_set: &'a SyntaxSet, path: &str) -> &'a SyntaxReference {
    let path = std::path::Path::new(path);
    path.extension()
        .and_then(|extension| extension.to_str())
        .and_then(|extension| syntax_set.find_syntax_by_extension(extension))
        .or_else(|| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| syntax_set.find_syntax_by_extension(name))
        })
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text())
}

fn render_styled(
    writer: &mut impl Write,
    styled: &[(Style, &str)],
    line: &ResultLine,
) -> Result<()> {
    let mut offset = 0usize;
    for (style, text) in styled {
        let text = text.trim_end_matches('\n');
        if text.is_empty() {
            continue;
        }
        let mut points = vec![0, text.len()];
        for range in &line.ranges {
            if range.start > offset && range.start < offset + text.len() {
                points.push(range.start - offset);
            }
            if range.end > offset && range.end < offset + text.len() {
                points.push(range.end - offset);
            }
        }
        points.sort_unstable();
        points.dedup();
        for window in points.windows(2) {
            let start = window[0];
            let end = window[1];
            if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
                continue;
            }
            let matched = line
                .ranges
                .iter()
                .any(|range| range.start < offset + end && range.end > offset + start);
            let fg = style.foreground;
            if matched {
                write!(
                    writer,
                    "\x1b[38;2;{};{};{};1;4m{}\x1b[0m",
                    fg.r,
                    fg.g,
                    fg.b,
                    &text[start..end]
                )?;
            } else {
                write!(
                    writer,
                    "\x1b[38;2;{};{};{}m{}\x1b[0m",
                    fg.r,
                    fg.g,
                    fg.b,
                    &text[start..end]
                )?;
            }
        }
        offset += text.len();
    }
    writeln!(writer)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SearchResult;

    fn result(repository: &str, path: &str, match_count: usize) -> SearchResult {
        SearchResult {
            repository: repository.into(),
            repository_name: repository.into(),
            path: path.into(),
            branch: "main".into(),
            commit: "0a1b2c3d4e5f6789".into(),
            web_url: format!("https://example.invalid/{repository}/{path}"),
            score: 1.0,
            match_count,
            lines: vec![ResultLine {
                number: 1,
                text: "needle".into(),
                ranges: vec![],
                is_context: false,
            }],
            stale: false,
        }
    }

    fn response() -> SearchResponse {
        SearchResponse {
            query: vec!["needle".into()],
            results: vec![
                result("team/api", "src/a.rs", 12),
                result("team/web", "src/b.ts", 3),
            ],
            repositories_searched: 2,
            files_searched: 412,
            elapsed_ms: 8460,
            sync_ms: 8140,
            scan_ms: 320,
            total_results: 2,
            ..Default::default()
        }
    }

    /// The summary has to carry what the user cannot otherwise see.
    #[test]
    fn the_summary_names_skips_truncation_and_the_true_total() {
        let plain = summary_line(&response());
        assert_eq!(
            plain,
            "2 results across 2 repositories (412 files, 8460 ms)"
        );

        let mut capped = response();
        capped.total_results = 412;
        capped.truncation = Truncation::new(true, 0, 0);
        capped.truncated = true;
        capped.skipped_files = SkippedFiles {
            too_large: 2,
            binary: 1,
            not_utf8: 0,
        };
        capped.cached = true;
        capped.skipped = vec![SkippedRepository {
            repository: "team/empty".into(),
            branch: Some("main".into()),
            reason: "no commits".into(),
        }];
        let line = summary_line(&capped);
        assert!(line.starts_with("2 of 412 results"), "{line}");
        assert!(line.contains("3 skipped: 2 too large, 1 binary"), "{line}");
        assert!(line.contains("cache hit"), "{line}");
        assert!(line.contains("more results than --max-results"), "{line}");
        assert!(line.contains("1 repositories skipped"), "{line}");

        // an abandoned pattern is the one worth acting on, so it leads
        let mut gave_up = response();
        gave_up.truncation = Truncation::new(false, 1, 3);
        gave_up.truncated = true;
        let line = summary_line(&gave_up);
        let expensive = line.find("pattern too expensive in 3 files").unwrap();
        let cap = line.find("match cap reached in 1 files").unwrap();
        assert!(expensive < cap, "{line}");
    }

    /// `jq -r .repository` must keep working on result lines, and a script must
    /// be able to tell a result apart from the summary.
    #[test]
    fn jsonl_tags_result_lines_and_ends_with_one_summary() {
        let lines = jsonl_lines(&response()).unwrap();
        assert_eq!(lines.len(), 3);
        let parsed: Vec<serde_json::Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(parsed[0]["type"], "result");
        // flattened, so the result's own fields stay at the top level
        assert_eq!(parsed[0]["repository"], "team/api");
        assert_eq!(parsed[0]["path"], "src/a.rs");
        assert_eq!(parsed[1]["type"], "result");
        assert_eq!(parsed[2]["type"], "summary");
        assert_eq!(parsed[2]["total_results"], 2);
        assert_eq!(parsed[2]["files_searched"], 412);
        assert_eq!(parsed[2]["sync_ms"], 8140);
        assert!(parsed[2]["truncation"].is_object());
        assert!(parsed[2]["skipped_files"].is_object());
        assert_eq!(
            parsed
                .iter()
                .filter(|line| line["type"] == "summary")
                .count(),
            1
        );
    }

    #[test]
    fn count_and_file_list_modes_render_without_context() {
        assert_eq!(
            count_summary(&response()),
            "15 matches in 2 files across 2 repositories"
        );
        assert_eq!(
            heading(&response().results[0], false, false),
            "team/api  src/a.rs"
        );
        assert_eq!(
            heading(&response().results[0], false, true),
            "team/api  src/a.rs  main@0a1b2c3d4e5f"
        );
        // the modes that answer "which files" and "how many" do not publish
        // context lines they were not asked for
        let stripped = without_lines(&response());
        assert!(
            stripped
                .results
                .iter()
                .all(|result| result.lines.is_empty())
        );
    }

    #[test]
    fn stats_separate_fetching_from_scanning() {
        let block = stats_block(&response());
        assert!(
            block.contains("sync      8140 ms   2 repositories"),
            "{block}"
        );
        assert!(block.contains("scan       320 ms   412 files"), "{block}");
        assert!(
            block.contains("total     8460 ms   2 matched, 2 shown"),
            "{block}"
        );
    }

    #[test]
    fn stderr_colour_follows_the_explicit_choice() {
        assert!(should_color_stderr(ColorChoice::Always));
        assert!(!should_color_stderr(ColorChoice::Never));
    }

    #[test]
    fn detects_modern_syntaxes_without_opening_the_result_path() {
        let syntax_set = two_face::syntax::extra_newlines();

        assert_eq!(
            syntax_for_result_path(&syntax_set, "missing/source.rs").name,
            "Rust"
        );
        assert_eq!(
            syntax_for_result_path(&syntax_set, "missing/Dockerfile").name,
            "Dockerfile"
        );
        assert_eq!(
            syntax_for_result_path(&syntax_set, "missing/component.tsx").name,
            "TypeScriptReact"
        );
    }
}
