//! Every query written in docs/usage.md must actually parse. Keep this list in
//! step with the examples there.

use better_bitbucket_search::query::{CaseMode, CompiledQuery};

#[test]
fn documented_queries_parse() {
    let cases: &[(&str, bool, bool)] = &[
        // (query, raw_regex, multiline)
        ("getUser", false, false),
        ("valueGenerator AND account-summary", false, false),
        ("/valueGenerator.*?account-summary/s", false, false),
        ("valueGenerator*account-summary", false, true),
        ("getuser AND FetchUser", false, false),
        ("PaymentIntent AND NOT /test|spec/", false, false),
        ("\"apiVersion:\"", false, false),
        ("\"parse_query(\"", false, false),
        ("\"fn parse_query\" AND \"parse_query(\"", false, false),
        ("TODO\\([a-z.]+\\)", true, false),
        ("parser AND src", false, false),
        ("(a OR b) AND c", false, false),
        ("/re/isxm", false, false),
    ];
    let mut bad = Vec::new();
    for (source, raw, multi) in cases {
        if let Err(error) =
            CompiledQuery::parse(&[source.to_string()], *raw, CaseMode::Smart, *multi)
        {
            bad.push(format!("{source}  ->  {error}"));
        }
    }
    // this one must fail: an unescaped paren is grouping syntax
    let unescaped =
        CompiledQuery::parse(&["parse_query(".to_string()], false, CaseMode::Smart, false);
    assert!(unescaped.is_err(), "expected bare `parse_query(` to fail");

    assert!(
        bad.is_empty(),
        "these documented queries do not parse:\n{}",
        bad.join("\n")
    );
}
