//! Path filtering for a search.
//!
//! `--path` used to be a raw globset with no normalisation, no negation and no
//! diagnostics, which made three separate problems out of one:
//!
//! - There was no way to exclude a path at all. `NOT` excludes on *content*,
//!   not on path, so "find X everywhere except the vendored tree" - the shape
//!   of almost every real audit query - could not be expressed.
//! - `*.md` matched six root-level files instead of ninety-six, because `*`
//!   does not cross `/`. Correct, and never what anyone means.
//! - `./src/**`, `/src/**`, `src/` and `''` each matched nothing and said
//!   nothing, and every one of them is a spelling a reasonable person tries.

use anyhow::{Result, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// Directories the relevance ranking already demotes. `--no-vendor` turns that
/// same knowledge into a filter rather than only a score nudge.
pub const VENDOR_DIRECTORIES: [&str; 5] = ["vendor", "generated", "dist", "build", "node_modules"];

/// Why a file is not in the result set, so a filter that eliminates everything
/// can name which half did it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Selected,
    DroppedByInclude,
    DroppedByExclude,
}

/// Tallies from one walk, used to tell "your query found nothing" apart from
/// "your path filter removed everything before the query ran".
#[derive(Debug, Clone, Copy, Default)]
pub struct FilterCounts {
    /// Files that survived the `.git` and size filters, before path globs.
    pub considered: usize,
    pub selected: usize,
    pub dropped_by_include: usize,
    pub dropped_by_exclude: usize,
}

#[derive(Debug, Default)]
pub struct PathFilter {
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
    /// Original spellings, for the diagnostic.
    include_sources: Vec<String>,
    exclude_sources: Vec<String>,
}

impl PathFilter {
    pub fn new(paths: &[String], exclude_paths: &[String], no_vendor: bool) -> Result<Self> {
        let mut include_sources = Vec::new();
        let mut exclude_sources = Vec::new();
        // A leading `!` inside --path is an exclusion, for symmetry with
        // .gitignore. It is the spelling people reach for first.
        for pattern in paths {
            match pattern.strip_prefix('!') {
                Some(rest) => exclude_sources.push(rest.to_string()),
                None => include_sources.push(pattern.clone()),
            }
        }
        for pattern in exclude_paths {
            exclude_sources.push(pattern.strip_prefix('!').unwrap_or(pattern).to_string());
        }
        if no_vendor {
            for directory in VENDOR_DIRECTORIES {
                exclude_sources.push(directory.to_string());
            }
        }
        Ok(Self {
            include: build_set(&include_sources, "--path")?,
            exclude: build_set(&exclude_sources, "--exclude-path")?,
            include_sources,
            exclude_sources,
        })
    }

    pub fn is_active(&self) -> bool {
        self.include.is_some() || self.exclude.is_some()
    }

    /// `relative` must already use `/` separators.
    pub fn verdict(&self, relative: &str) -> Verdict {
        if self
            .include
            .as_ref()
            .is_some_and(|set| !set.is_match(relative))
        {
            return Verdict::DroppedByInclude;
        }
        if self
            .exclude
            .as_ref()
            .is_some_and(|set| set.is_match(relative))
        {
            return Verdict::DroppedByExclude;
        }
        Verdict::Selected
    }

    /// The message for a filter that eliminated every file. A filter that
    /// leaves nothing is nearly always a typo, and silence reads as "no
    /// matches" rather than "unsupported spelling".
    ///
    /// It reports files *considered* rather than files the query would have
    /// matched, because the walk already visited them: answering "was it my
    /// filter or my query?" costs nothing, while re-running the search
    /// unfiltered to count query matches would double the work.
    pub fn empty_result_warning(&self, counts: &FilterCounts) -> Option<String> {
        if !self.is_active() || counts.selected > 0 || counts.considered == 0 {
            return None;
        }
        let quoted = |sources: &[String]| {
            sources
                .iter()
                .map(|source| format!("`{source}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        if counts.dropped_by_exclude > 0 && counts.dropped_by_include == 0 {
            return Some(format!(
                "every one of {} considered files was removed by --exclude-path {}",
                counts.considered,
                quoted(&self.exclude_sources)
            ));
        }
        Some(format!(
            "no file matched --path {}; {} files were considered",
            quoted(&self.include_sources),
            counts.considered
        ))
    }
}

fn build_set(sources: &[String], flag: &str) -> Result<Option<GlobSet>> {
    if sources.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for source in sources {
        for pattern in normalize(source, flag)? {
            builder.add(
                GlobBuilder::new(&pattern)
                    .literal_separator(true)
                    .backslash_escape(false)
                    .build()
                    .map_err(|error| anyhow::anyhow!("invalid path glob `{source}`: {error}"))?,
            );
        }
    }
    Ok(Some(builder.build()?))
}

/// Turns one written pattern into the globs it should actually mean.
///
/// Returns more than one because a bare directory name has to match both the
/// entry and everything beneath it: only files are ever tested, so
/// `**/node_modules` alone would exclude nothing at all.
fn normalize(source: &str, flag: &str) -> Result<Vec<String>> {
    if source.trim().is_empty() {
        bail!("{flag} cannot be empty");
    }
    // Whether the pattern was anchored is decided on the text as written. A
    // leading `./` or `/` is the documented way to say "the repository root and
    // nowhere else", so the test has to happen before it is stripped.
    let anchored = source.contains('/');
    let mut pattern = source.replace('\\', "/");
    pattern = pattern
        .strip_prefix("./")
        .or_else(|| pattern.strip_prefix('/'))
        .unwrap_or(&pattern)
        .to_string();
    if pattern.is_empty() {
        bail!("{flag} `{source}` names no path");
    }
    // A trailing `/` means the directory's contents.
    if let Some(stripped) = pattern.strip_suffix('/') {
        pattern = format!("{stripped}/**");
    }
    // ripgrep's rule: a pattern with no separator matches at any depth. Nobody
    // types `*.md` meaning "only in the root", and the root-only reading is
    // still available as `./*.md`.
    if !anchored {
        pattern = format!("**/{pattern}");
    }
    let mut patterns = vec![pattern.clone()];
    if !pattern.ends_with("/**") {
        patterns.push(format!("{pattern}/**"));
    }
    Ok(patterns)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalized(source: &str) -> Vec<String> {
        normalize(source, "--path").unwrap()
    }

    #[test]
    fn a_pattern_without_a_separator_matches_at_any_depth() {
        assert_eq!(normalized("*.md"), ["**/*.md", "**/*.md/**"]);
        assert_eq!(
            normalized("node_modules"),
            ["**/node_modules", "**/node_modules/**"]
        );
    }

    /// The escape hatch from the rule above: a leading `./` or `/` anchors the
    /// pattern at the repository root.
    #[test]
    fn a_leading_dot_slash_or_slash_anchors_at_the_root() {
        assert_eq!(normalized("./*.md"), ["*.md", "*.md/**"]);
        assert_eq!(normalized("/x.md"), ["x.md", "x.md/**"]);
        assert_eq!(normalized("./src/**"), ["src/**"]);
        assert_eq!(normalized("/src/**"), ["src/**"]);
    }

    #[test]
    fn a_trailing_slash_means_everything_beneath() {
        assert_eq!(normalized("src/"), ["src/**"]);
        assert_eq!(normalized("src/**"), ["src/**"]);
    }

    #[test]
    fn an_empty_pattern_is_refused_rather_than_matching_nothing() {
        assert!(
            normalize("", "--path")
                .unwrap_err()
                .to_string()
                .contains("cannot be empty")
        );
        assert!(normalize("   ", "--path").is_err());
        assert!(normalize("/", "--path").is_err());
    }

    #[test]
    fn include_patterns_select_at_any_depth() {
        let filter = PathFilter::new(&["*.md".into()], &[], false).unwrap();
        assert_eq!(filter.verdict("README.md"), Verdict::Selected);
        assert_eq!(filter.verdict("docs/guide.md"), Verdict::Selected);
        assert_eq!(filter.verdict("a/b/c/deep.md"), Verdict::Selected);
        assert_eq!(filter.verdict("src/main.rs"), Verdict::DroppedByInclude);

        let rooted = PathFilter::new(&["./*.md".into()], &[], false).unwrap();
        assert_eq!(rooted.verdict("README.md"), Verdict::Selected);
        assert_eq!(rooted.verdict("docs/guide.md"), Verdict::DroppedByInclude);
    }

    /// `--path '!vendor/**'` used to return zero results with no error, which
    /// reads as "no matches" rather than "unsupported syntax".
    #[test]
    fn a_bang_inside_path_excludes_and_so_does_exclude_path() {
        for filter in [
            PathFilter::new(&["!vendor/**".into()], &[], false).unwrap(),
            PathFilter::new(&[], &["vendor/**".into()], false).unwrap(),
            PathFilter::new(&[], &["!vendor/**".into()], false).unwrap(),
        ] {
            assert_eq!(filter.verdict("vendor/dep.go"), Verdict::DroppedByExclude);
            assert_eq!(filter.verdict("src/main.go"), Verdict::Selected);
        }
    }

    /// A bare directory name must remove the tree, not just an entry that is
    /// never tested because only files are.
    #[test]
    fn excluding_a_bare_directory_name_removes_its_whole_tree() {
        let filter = PathFilter::new(&[], &["test".into()], false).unwrap();
        assert_eq!(filter.verdict("src/test/a.rs"), Verdict::DroppedByExclude);
        assert_eq!(filter.verdict("test/a.rs"), Verdict::DroppedByExclude);
        assert_eq!(filter.verdict("src/main.rs"), Verdict::Selected);
    }

    #[test]
    fn no_vendor_excludes_every_directory_the_ranking_demotes() {
        let filter = PathFilter::new(&[], &[], true).unwrap();
        for directory in VENDOR_DIRECTORIES {
            assert_eq!(
                filter.verdict(&format!("a/{directory}/b.js")),
                Verdict::DroppedByExclude,
                "{directory} should be excluded"
            );
        }
        assert_eq!(filter.verdict("src/main.js"), Verdict::Selected);
    }

    #[test]
    fn include_and_exclude_compose() {
        let filter = PathFilter::new(&["**/*.rs".into()], &["**/test/**".into()], false).unwrap();
        assert_eq!(filter.verdict("src/main.rs"), Verdict::Selected);
        assert_eq!(filter.verdict("src/test/a.rs"), Verdict::DroppedByExclude);
        assert_eq!(filter.verdict("src/main.go"), Verdict::DroppedByInclude);
    }

    #[test]
    fn a_filter_that_removes_everything_says_which_half_did_it() {
        let include = PathFilter::new(&["src/".into()], &[], false).unwrap();
        let message = include
            .empty_result_warning(&FilterCounts {
                considered: 412,
                selected: 0,
                dropped_by_include: 412,
                dropped_by_exclude: 0,
            })
            .unwrap();
        assert!(
            message.contains("no file matched --path `src/`"),
            "{message}"
        );
        assert!(message.contains("412"), "{message}");

        let exclude = PathFilter::new(&[], &["**/*".into()], false).unwrap();
        let message = exclude
            .empty_result_warning(&FilterCounts {
                considered: 412,
                selected: 0,
                dropped_by_include: 0,
                dropped_by_exclude: 412,
            })
            .unwrap();
        assert!(message.contains("--exclude-path"), "{message}");

        // no filter, or a filter that selected something, says nothing
        assert!(
            PathFilter::default()
                .empty_result_warning(&FilterCounts::default())
                .is_none()
        );
        assert!(
            include
                .empty_result_warning(&FilterCounts {
                    considered: 412,
                    selected: 7,
                    dropped_by_include: 405,
                    dropped_by_exclude: 0,
                })
                .is_none()
        );
    }
}
