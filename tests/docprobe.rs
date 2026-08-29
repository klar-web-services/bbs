//! Every query written in docs/usage.md must actually parse, and every query
//! documented as refused must actually be refused. Keep both lists in step
//! with the examples there.

use better_bitbucket_search::{
    paths::PathFilter,
    query::{CompiledQuery, QueryOptions},
};

#[test]
fn documented_queries_parse() {
    // (query, raw_regex, multiline, word)
    let cases: &[(&str, bool, bool, bool)] = &[
        ("getUser", false, false, false),
        ("valueGenerator AND account-summary", false, false, false),
        ("/valueGenerator.*?account-summary/s", false, false, false),
        ("valueGenerator*account-summary", false, false, true),
        ("getuser AND FetchUser", false, false, false),
        ("PaymentIntent AND NOT /test|spec/", false, false, false),
        ("\"apiVersion:\"", false, false, false),
        ("\"parse_query(\"", false, false, false),
        (
            "\"fn parse_query\" AND \"parse_query(\"",
            false,
            false,
            false,
        ),
        ("TODO\\([a-z.]+\\)", true, false, false),
        ("parser AND src", false, false, false),
        ("(a OR b) AND c", false, false, false),
        ("/re/isxm", false, false, false),
        // case control, per term
        ("/Foo/c", false, false, false),
        (r"/todo\D/", false, false, false),
        (r"/todo\s/", false, false, false),
        (r"/\Qtodo A\E/", false, false, false),
        // word mode
        ("getUser", false, false, true),
        ("PaymentIntent", false, false, true),
        // escape sequences that name characters
        (r"a\tb", false, false, false),
        (r"C:\\Users", false, false, false),
        (r#""C:\\Users""#, false, false, false),
    ];
    let mut bad = Vec::new();
    for (source, raw, multi, word) in cases {
        if let Err(error) = CompiledQuery::parse(
            &[source.to_string()],
            QueryOptions {
                regex: *raw,
                multiline: *multi,
                word: *word,
                ..Default::default()
            },
        ) {
            bad.push(format!("{source}  ->  {error}"));
        }
    }
    assert!(
        bad.is_empty(),
        "these documented queries do not parse:\n{}",
        bad.join("\n")
    );
}

#[test]
fn documented_refusals_are_refused() {
    // an unescaped paren is grouping syntax
    assert!(
        CompiledQuery::parse(&["parse_query(".to_string()], QueryOptions::default()).is_err(),
        "expected bare `parse_query(` to fail"
    );
    // a query with nothing to find
    assert!(
        CompiledQuery::parse(&["NOT deprecated".to_string()], QueryOptions::default()).is_err()
    );
    // a lowercase operator is a mistyped operator, not a term pair
    assert!(CompiledQuery::parse(&["foo and bar".to_string()], QueryOptions::default()).is_err());
    // patterns that match at every position
    let mut accepted = Vec::new();
    for source in [r#""""#, "//", "*", "?", "/a*/"] {
        if CompiledQuery::parse(&[source.to_string()], QueryOptions::default()).is_ok() {
            accepted.push(source);
        }
    }
    assert!(
        accepted.is_empty(),
        "these should be refused as matching everything: {accepted:?}"
    );
}

/// The path-filter table in docs/usage.md, asserted rather than described.
#[test]
fn documented_path_filters_select_what_the_table_says() {
    let selects = |paths: &[&str], exclude: &[&str], no_vendor: bool, file: &str| {
        let paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        let exclude: Vec<String> = exclude.iter().map(|p| p.to_string()).collect();
        PathFilter::new(&paths, &exclude, no_vendor)
            .unwrap()
            .verdict(file)
            == better_bitbucket_search::paths::Verdict::Selected
    };

    // `*.md` -- every .md file at any depth
    assert!(selects(&["*.md"], &[], false, "README.md"));
    assert!(selects(&["*.md"], &[], false, "docs/a/b.md"));
    // `./*.md` -- the repository root only
    assert!(selects(&["./*.md"], &[], false, "README.md"));
    assert!(!selects(&["./*.md"], &[], false, "docs/a.md"));
    assert!(selects(&["/*.md"], &[], false, "README.md"));
    // `src/` and `src` -- everything under a src directory
    assert!(selects(&["src/"], &[], false, "src/main.rs"));
    assert!(selects(&["src"], &[], false, "src/deep/main.rs"));
    // `src/**` -- everything under the root src
    assert!(selects(&["src/**"], &[], false, "src/main.rs"));
    // exclusion, in all three spellings
    assert!(!selects(&[], &["vendor/**"], false, "vendor/dep.go"));
    assert!(!selects(&["!vendor/**"], &[], false, "vendor/dep.go"));
    assert!(!selects(&[], &["vendor"], false, "vendor/dep.go"));
    // --no-vendor
    for directory in better_bitbucket_search::paths::VENDOR_DIRECTORIES {
        assert!(
            !selects(&[], &[], true, &format!("a/{directory}/b.js")),
            "--no-vendor should exclude {directory}"
        );
    }
    assert!(selects(&[], &[], true, "src/main.js"));
}

/// The durations docs/usage.md offers for `--max-age`.
#[test]
fn documented_durations_parse() {
    use better_bitbucket_search::duration::parse_duration_secs;
    assert_eq!(parse_duration_secs("30s").unwrap(), 30);
    assert_eq!(parse_duration_secs("5m").unwrap(), 300);
    assert_eq!(parse_duration_secs("1h30m").unwrap(), 5400);
    assert_eq!(parse_duration_secs("2d").unwrap(), 172_800);
    assert_eq!(parse_duration_secs("90").unwrap(), 90);
}
