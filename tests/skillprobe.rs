//! The shipped agent skill, checked against the engine it documents.
//!
//! An agent copies these examples verbatim, so a query in `SKILL.md` that no
//! longer parses, or a quoted error message that no longer matches, is a bug
//! that reaches the user as a failed command rather than as a stale document.
//! `docprobe.rs` does the same job for `docs/usage.md`, by hand; here the
//! cases are lifted out of the skill itself, so a new example is covered the
//! moment it is written.

use better_bitbucket_search::query::{CaseMode, CompiledQuery, QueryOptions};

const SKILL: &str = include_str!("../skills/bbs/SKILL.md");
const QUERY_REFERENCE: &str = include_str!("../skills/bbs/references/query.md");
const CLI_REFERENCE: &str = include_str!("../skills/bbs/references/cli.md");

fn sources() -> [(&'static str, &'static str); 3] {
    [
        ("SKILL.md", SKILL),
        ("references/query.md", QUERY_REFERENCE),
        ("references/cli.md", CLI_REFERENCE),
    ]
}

/// Flags that consume the token after them, so their value is never mistaken
/// for a query expression.
const FLAGS_WITH_VALUES: [&str; 12] = [
    "--repos",
    "--path",
    "--exclude-path",
    "--branch",
    "--format",
    "--context",
    "--max-results",
    "--sort",
    "--max-age",
    "--color",
    "--filter",
    "--harness",
];

/// The first word after `bbs` when the line invokes something other than a
/// search. None of these take a query.
const SUBCOMMANDS: [&str; 9] = [
    "auth", "login", "logout", "list", "repos", "serve", "skill", "update", "cache",
];

#[derive(Debug, Default, PartialEq)]
struct Invocation {
    queries: Vec<String>,
    options: QueryOptions,
}

/// Reads one documented `bbs` command line into the queries it would run.
///
/// Returns `None` for a line that is not a search: a subcommand, or a search
/// whose expression comes from somewhere this test cannot see.
fn parse_invocation(line: &str) -> Option<Invocation> {
    // Documented pipelines end in `| jq ...`; only the bbs half is ours.
    let command = line.split('|').next()?.trim();
    let command = command.strip_prefix("$ ").unwrap_or(command);
    // `BB_TOKEN=... bbs ...` and `bbs ...` alike.
    let words = shell_words::split(command).ok()?;
    let start = words.iter().position(|word| word == "bbs")?;
    let mut words = words[start + 1..].iter();

    let mut invocation = Invocation {
        options: QueryOptions {
            regex: false,
            case_mode: CaseMode::Smart,
            multiline: false,
            word: false,
        },
        ..Invocation::default()
    };
    let mut first = true;
    while let Some(word) = words.next() {
        if first && SUBCOMMANDS.contains(&word.as_str()) {
            return None;
        }
        first = false;
        if FLAGS_WITH_VALUES.contains(&word.as_str()) {
            words.next();
            continue;
        }
        match word.as_str() {
            "-r" | "--regex" => invocation.options.regex = true,
            "-M" | "--multiline" => invocation.options.multiline = true,
            "-w" | "--word" => invocation.options.word = true,
            "-i" | "--ignore-case" => invocation.options.case_mode = CaseMode::Ignore,
            "-s" | "--case-sensitive" => invocation.options.case_mode = CaseMode::Sensitive,
            // A placeholder, not an example: `bbs <query>...`.
            "<query>" | "'query'" | "query" => return None,
            other if other.starts_with('-') => {}
            other => invocation.queries.push(other.to_owned()),
        }
    }
    (!invocation.queries.is_empty()).then_some(invocation)
}

/// Every `bbs` line inside a fenced code block, with its source and line
/// number so a failure names the file to edit.
fn documented_commands() -> Vec<(&'static str, usize, String)> {
    let mut found = Vec::new();
    for (name, text) in sources() {
        let mut fenced = false;
        for (index, line) in text.lines().enumerate() {
            if line.starts_with("```") {
                fenced = !fenced;
                continue;
            }
            let trimmed = line.trim();
            if fenced && (trimmed.starts_with("bbs ") || trimmed.starts_with("$ bbs ")) {
                found.push((name, index + 1, trimmed.to_owned()));
            }
        }
    }
    assert!(found.len() > 30, "only found {} commands", found.len());
    found
}

/// Console blocks pair a command with the error it is documented to produce:
/// a `$ bbs ...` line, then an `error: ...` line continued until a blank line.
fn documented_refusals() -> Vec<(&'static str, String, String)> {
    let mut found = Vec::new();
    for (name, text) in sources() {
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let Some(rest) = line.strip_prefix("error: ") else {
                continue;
            };
            let Some(command) = lines[..index]
                .iter()
                .rev()
                .find_map(|earlier| earlier.strip_prefix("$ "))
            else {
                continue;
            };
            // Wrapped for the 90-column docs; the real message is one line.
            let mut message = rest.trim().to_owned();
            for continuation in &lines[index + 1..] {
                if continuation.trim().is_empty() || continuation.starts_with("```") {
                    break;
                }
                message.push(' ');
                message.push_str(continuation.trim());
            }
            found.push((name, command.to_owned(), message));
        }
    }
    found
}

/// Every query the skill hands an agent has to compile. A stale one costs the
/// agent a failed command and a wasted turn.
#[test]
fn every_query_in_the_skill_parses() {
    let mut checked = 0;
    for (name, line_number, command) in documented_commands() {
        // A `$ ` prompt introduces a console block, where the point of the
        // example is the refusal underneath it. Those are checked below.
        if command.starts_with("$ ") {
            continue;
        }
        let Some(invocation) = parse_invocation(&command) else {
            continue;
        };
        CompiledQuery::parse(&invocation.queries, invocation.options)
            .unwrap_or_else(|error| panic!("{name}:{line_number} `{command}` — {error:#}"));
        checked += 1;
    }
    assert!(checked > 15, "only checked {checked} queries");
}

/// ...and every refusal it quotes has to be the message the engine actually
/// produces. The first draft of this skill quoted an invented one.
#[test]
fn every_quoted_refusal_is_the_real_message() {
    let mut checked = 0;
    for (name, command, expected) in documented_refusals() {
        // Refusals that are not the query compiler's to give -- an
        // inaccessible repository, an uncompilable repository filter -- have
        // no expression for this test to hand it.
        let Some(invocation) = parse_invocation(&command) else {
            continue;
        };
        let error = CompiledQuery::parse(&invocation.queries, invocation.options)
            .err()
            .unwrap_or_else(|| panic!("{name}: `{command}` was documented as refused, but parsed"));
        assert_eq!(format!("{error:#}"), expected, "{name}: `{command}`");
        checked += 1;
    }
    assert!(checked >= 4, "only checked {checked} refusals");
}

/// The commands a coding agent must never run, because both would hang: one
/// prompts on a TTY the agent does not have, the other blocks forever.
#[test]
fn the_skill_warns_against_the_two_blocking_commands() {
    for command in ["bbs serve", "bbs login"] {
        assert!(
            SKILL.contains(&format!("Never run `{command}`")),
            "SKILL.md must tell the agent never to run `{command}`"
        );
    }
}

/// The gate is the whole of AC3: an unconfigured `bbs` must not interrupt a
/// task that never asked for it.
#[test]
fn the_skill_gates_on_authentication_before_anything_else() {
    let gate = SKILL
        .split("## 2.")
        .next()
        .expect("SKILL.md must open with the gate section");
    assert!(
        gate.contains("bbs auth status"),
        "the gate must run the check"
    );
    assert!(
        gate.contains("stop silently"),
        "an automatic invocation with no credential must be a no-op"
    );
    assert!(
        gate.contains("bbs login"),
        "a manual invocation must name the fix"
    );
}
