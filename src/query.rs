use anyhow::{Result, bail};
use pcre2::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CaseMode {
    Smart,
    Ignore,
    Sensitive,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AtomKind {
    Wildcard,
    Regex,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AtomSpec {
    pub source: String,
    pub kind: AtomKind,
    #[serde(default)]
    pub flags: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Expr {
    Atom(usize),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

impl Expr {
    pub fn evaluate(&self, present: &[bool]) -> bool {
        match self {
            Expr::Atom(id) => present.get(*id).copied().unwrap_or(false),
            Expr::Not(inner) => !inner.evaluate(present),
            Expr::And(left, right) => left.evaluate(present) && right.evaluate(present),
            Expr::Or(left, right) => left.evaluate(present) || right.evaluate(present),
        }
    }

    /// Whether every way this expression can be satisfied involves at least one
    /// atom that actually matched something. `NOT x` and `a OR NOT b` can be
    /// true for a file while pointing at nothing in it, and results are built
    /// from matched spans, so such a query would silently report no matches at
    /// all rather than the files it logically selected.
    fn has_evidence(&self, negated: bool) -> bool {
        match self {
            Expr::Atom(_) => !negated,
            Expr::Not(inner) => inner.has_evidence(!negated),
            Expr::And(left, right) => {
                if negated {
                    left.has_evidence(negated) && right.has_evidence(negated)
                } else {
                    left.has_evidence(negated) || right.has_evidence(negated)
                }
            }
            Expr::Or(left, right) => {
                if negated {
                    left.has_evidence(negated) || right.has_evidence(negated)
                } else {
                    left.has_evidence(negated) && right.has_evidence(negated)
                }
            }
        }
    }

    fn collect_positive(&self, negated: bool, output: &mut BTreeSet<usize>) {
        match self {
            Expr::Atom(id) if !negated => {
                output.insert(*id);
            }
            Expr::Atom(_) => {}
            Expr::Not(inner) => inner.collect_positive(!negated, output),
            Expr::And(left, right) | Expr::Or(left, right) => {
                left.collect_positive(negated, output);
                right.collect_positive(negated, output);
            }
        }
    }
}

#[derive(Debug)]
pub struct CompiledAtom {
    pub spec: AtomSpec,
    regex: Regex,
}

/// Matches collected for one atom in one file. Neither stopping condition is
/// worth failing an entire search over, so the matches found so far are
/// reported instead - but they are reported separately, because hitting the
/// cap is benign and PCRE2 giving up means the results may be wrong.
#[derive(Debug, Default)]
pub struct AtomMatches {
    pub spans: Vec<(usize, usize)>,
    /// Stopped at `MATCH_CAP`.
    pub capped: bool,
    /// PCRE2 reported one of its own limits, e.g. backtracking on a
    /// catastrophic pattern.
    pub gave_up: bool,
}

const MATCH_CAP: usize = 20_000;

impl CompiledAtom {
    pub fn find_all(&self, bytes: &[u8]) -> AtomMatches {
        let mut found = AtomMatches::default();
        for item in self.regex.find_iter(bytes) {
            match item {
                Ok(span) => {
                    found.spans.push((span.start(), span.end()));
                    if found.spans.len() >= MATCH_CAP {
                        found.capped = true;
                        break;
                    }
                }
                // PCRE2 reports its own limits here, e.g. the backtracking
                // match limit on a catastrophic pattern.
                Err(_) => {
                    found.gave_up = true;
                    break;
                }
            }
        }
        found
    }
}

#[derive(Debug)]
pub struct CompiledQuery {
    pub sources: Vec<String>,
    pub expression: Expr,
    pub atoms: Vec<CompiledAtom>,
    pub case_mode: CaseMode,
    pub multiline: bool,
}

impl CompiledQuery {
    pub fn parse(
        sources: &[String],
        raw_regex: bool,
        case_mode: CaseMode,
        multiline: bool,
    ) -> Result<Self> {
        if sources.is_empty() {
            bail!("at least one query is required");
        }
        let mut atoms = Vec::new();
        let mut expressions = Vec::new();
        for source in sources {
            if source.trim().is_empty() {
                bail!("queries cannot be empty");
            }
            let expr = if raw_regex {
                let id = atoms.len();
                atoms.push(AtomSpec {
                    source: source.clone(),
                    kind: AtomKind::Regex,
                    flags: String::new(),
                });
                Expr::Atom(id)
            } else {
                Parser::new(source, &mut atoms)?.parse()?
            };
            expressions.push(expr);
        }
        let expression = expressions
            .into_iter()
            .reduce(|a, b| Expr::Or(Box::new(a), Box::new(b)))
            .unwrap();
        if !expression.has_evidence(false) {
            bail!(
                "this query has nothing to find: every way of satisfying it is a `NOT`, so there would be no matches to show. Combine it with a term, for example `foo AND NOT bar`"
            );
        }
        let compiled = atoms
            .into_iter()
            .map(|spec| compile_atom(spec, case_mode, multiline))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            sources: sources.to_vec(),
            expression,
            atoms: compiled,
            case_mode,
            multiline,
        })
    }

    pub fn positive_atoms(&self) -> BTreeSet<usize> {
        let mut output = BTreeSet::new();
        self.expression.collect_positive(false, &mut output);
        output
    }

    pub fn normalized(&self) -> QueryFingerprint {
        QueryFingerprint {
            sources: self.sources.clone(),
            expression: self.expression.clone(),
            atoms: self.atoms.iter().map(|a| a.spec.clone()).collect(),
            case_mode: self.case_mode,
            multiline: self.multiline,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryFingerprint {
    pub sources: Vec<String>,
    pub expression: Expr,
    pub atoms: Vec<AtomSpec>,
    pub case_mode: CaseMode,
    #[serde(default)]
    pub multiline: bool,
}

fn compile_atom(spec: AtomSpec, case_mode: CaseMode, multiline: bool) -> Result<CompiledAtom> {
    let pattern = match spec.kind {
        AtomKind::Regex => spec.source.clone(),
        AtomKind::Wildcard => wildcard_pattern(&spec.source, multiline),
    };
    let sensitive = match case_mode {
        CaseMode::Sensitive => true,
        CaseMode::Ignore => false,
        CaseMode::Smart => spec.source.chars().any(|c| c.is_uppercase()),
    };
    let regex = RegexBuilder::new()
        .caseless(!sensitive || spec.flags.contains('i'))
        .dotall(multiline || spec.flags.contains('s'))
        .multi_line(spec.flags.contains('m'))
        .extended(spec.flags.contains('x'))
        .utf(true)
        .ucp(true)
        .build(&pattern)
        .map_err(|error| anyhow::anyhow!("invalid query `{}`: {error}", spec.source))?;
    Ok(CompiledAtom { spec, regex })
}

fn wildcard_pattern(source: &str, multiline: bool) -> String {
    let mut output = String::new();
    let mut chars = source.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some(next) => push_escaped(&mut output, next),
                None => output.push_str("\\\\"),
            },
            // Lazy across lines: a greedy match would run from the first hit
            // to the last one in the file and swallow everything between.
            '*' => output.push_str(if multiline {
                "[\\s\\S]*?"
            } else {
                "[^\\r\\n]*"
            }),
            '?' => output.push_str(if multiline { "[\\s\\S]" } else { "[^\\r\\n]" }),
            other => push_escaped(&mut output, other),
        }
    }
    output
}

fn push_escaped(output: &mut String, ch: char) {
    if ".^$|()[]{}+*?\\".contains(ch) {
        output.push('\\');
    }
    output.push(ch);
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Atom(AtomSpec),
    And,
    Or,
    Not,
    Left,
    Right,
    End,
}

struct Lexer<'a> {
    chars: Vec<char>,
    pos: usize,
    _source: &'a str,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            chars: source.chars().collect(),
            pos: 0,
            _source: source,
        }
    }
    fn next(&mut self) -> Result<Token> {
        while self.pos < self.chars.len() && self.chars[self.pos].is_whitespace() {
            self.pos += 1;
        }
        if self.pos >= self.chars.len() {
            return Ok(Token::End);
        }
        match self.chars[self.pos] {
            '(' => {
                self.pos += 1;
                Ok(Token::Left)
            }
            ')' => {
                self.pos += 1;
                Ok(Token::Right)
            }
            '"' | '\'' => self.quoted(),
            '/' => self.regex(),
            _ => self.bare(),
        }
    }

    fn quoted(&mut self) -> Result<Token> {
        let quote = self.chars[self.pos];
        self.pos += 1;
        let mut source = String::new();
        let mut escaped = false;
        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            self.pos += 1;
            if escaped {
                // Keep the escape intact: `wildcard_pattern` performs the one
                // and only unescaping pass. Consuming it here as well made
                // `"C:\\Users"` mean `C:Users`, and quoted terms need twice as
                // many backslashes as bare ones to say the same thing.
                source.push('\\');
                source.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                return Ok(Token::Atom(AtomSpec {
                    source,
                    kind: AtomKind::Wildcard,
                    flags: String::new(),
                }));
            } else {
                source.push(ch);
            }
        }
        bail!("unterminated quoted phrase")
    }

    fn regex(&mut self) -> Result<Token> {
        self.pos += 1;
        let mut source = String::new();
        let mut escaped = false;
        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            self.pos += 1;
            if escaped {
                if ch == '/' {
                    source.push('/');
                } else {
                    source.push('\\');
                    source.push(ch);
                }
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '/' {
                let flags = self.regex_flags(&source)?;
                return Ok(Token::Atom(AtomSpec {
                    source,
                    kind: AtomKind::Regex,
                    flags,
                }));
            } else {
                source.push(ch);
            }
        }
        bail!("unterminated /regex/ atom")
    }

    /// Reads trailing regex flags after a closing `/`. A following Boolean
    /// keyword is left alone so `/foo/AND bar` still parses.
    fn regex_flags(&mut self, pattern: &str) -> Result<String> {
        let start = self.pos;
        while self.pos < self.chars.len() && self.chars[self.pos].is_ascii_alphabetic() {
            self.pos += 1;
        }
        let run: String = self.chars[start..self.pos].iter().collect();
        if matches!(run.as_str(), "AND" | "OR" | "NOT") {
            self.pos = start;
            return Ok(String::new());
        }
        if let Some(unknown) = run
            .chars()
            .find(|flag| !matches!(flag, 'i' | 'm' | 's' | 'x'))
        {
            bail!(
                "unknown regex flag `{unknown}` in `/{pattern}/{run}`; supported flags are i (ignore case), s (. matches newlines), m (^ and $ match at line breaks), and x (ignore whitespace)"
            );
        }
        Ok(run)
    }

    fn bare(&mut self) -> Result<Token> {
        let start = self.pos;
        let mut escaped = false;
        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            if !escaped && (ch.is_whitespace() || ch == '(' || ch == ')') {
                break;
            }
            self.pos += 1;
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            }
        }
        let source: String = self.chars[start..self.pos].iter().collect();
        // Operators are uppercase only, so `and`, `or` and `not` stay ordinary
        // searchable words. Lowercase keywords used as operators are caught by
        // the parser, which suggests the uppercase form.
        match source.as_str() {
            "AND" => Ok(Token::And),
            "OR" => Ok(Token::Or),
            "NOT" => Ok(Token::Not),
            _ => Ok(Token::Atom(AtomSpec {
                source,
                kind: AtomKind::Wildcard,
                flags: String::new(),
            })),
        }
    }
}

/// A bare word that is a Boolean keyword in the wrong case is almost always a
/// mistyped operator rather than a search term, so say so.
fn lowercase_keyword(token: &Token) -> Option<&str> {
    let Token::Atom(spec) = token else {
        return None;
    };
    if spec.kind != AtomKind::Wildcard {
        return None;
    }
    match spec.source.to_ascii_uppercase().as_str() {
        "AND" if spec.source != "AND" => Some("AND"),
        "OR" if spec.source != "OR" => Some("OR"),
        "NOT" if spec.source != "NOT" => Some("NOT"),
        _ => None,
    }
}

fn describe(token: &Token) -> String {
    match token {
        Token::Atom(spec) => match spec.kind {
            AtomKind::Regex => format!("regular expression `/{}/`", spec.source),
            AtomKind::Wildcard => format!("term `{}`", spec.source),
        },
        Token::And => "`AND`".into(),
        Token::Or => "`OR`".into(),
        Token::Not => "`NOT`".into(),
        Token::Left => "`(`".into(),
        Token::Right => "`)`".into(),
        Token::End => "end of the query".into(),
    }
}

/// Depth cap for the recursive-descent parser. Real queries nest a handful of
/// levels; without a cap, a query of a few thousand parentheses overflows the
/// stack and aborts the process. `bbs serve` accepts query bodies up to 1 MiB,
/// so that abort was reachable from a single local request.
const MAX_NESTING: usize = 128;

struct Parser<'a, 'b> {
    lexer: Lexer<'a>,
    lookahead: Token,
    atoms: &'b mut Vec<AtomSpec>,
    depth: usize,
}

impl<'a, 'b> Parser<'a, 'b> {
    fn new(source: &'a str, atoms: &'b mut Vec<AtomSpec>) -> Result<Self> {
        let mut lexer = Lexer::new(source);
        let lookahead = lexer.next()?;
        Ok(Self {
            lexer,
            lookahead,
            atoms,
            depth: 0,
        })
    }
    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        anyhow::ensure!(
            self.depth <= MAX_NESTING,
            "query nesting is too deep (limit {MAX_NESTING}); simplify the parentheses or the NOT chain"
        );
        Ok(())
    }
    fn bump(&mut self) -> Result<Token> {
        let current = std::mem::replace(&mut self.lookahead, Token::End);
        self.lookahead = self.lexer.next()?;
        Ok(current)
    }
    fn parse(mut self) -> Result<Expr> {
        let expr = self.parse_or()?;
        if self.lookahead != Token::End {
            if let Some(upper) = lowercase_keyword(&self.lookahead) {
                bail!(
                    "operators must be uppercase; write `{upper}` instead of `{}`",
                    match &self.lookahead {
                        Token::Atom(spec) => spec.source.as_str(),
                        _ => upper,
                    }
                );
            }
            bail!(
                "unexpected {} after the query expression; join terms with AND, OR, or NOT",
                describe(&self.lookahead)
            );
        }
        Ok(expr)
    }
    fn parse_or(&mut self) -> Result<Expr> {
        let mut expr = self.parse_and()?;
        while self.lookahead == Token::Or {
            self.bump()?;
            expr = Expr::Or(Box::new(expr), Box::new(self.parse_and()?));
        }
        Ok(expr)
    }
    fn parse_and(&mut self) -> Result<Expr> {
        let mut expr = self.parse_not()?;
        while self.lookahead == Token::And {
            self.bump()?;
            expr = Expr::And(Box::new(expr), Box::new(self.parse_not()?));
        }
        Ok(expr)
    }
    fn parse_not(&mut self) -> Result<Expr> {
        if self.lookahead == Token::Not {
            self.bump()?;
            self.enter()?;
            let inner = self.parse_not()?;
            self.depth -= 1;
            Ok(Expr::Not(Box::new(inner)))
        } else {
            self.parse_primary()
        }
    }
    fn parse_primary(&mut self) -> Result<Expr> {
        match self.bump()? {
            Token::Atom(atom) => {
                let id = self.atoms.len();
                self.atoms.push(atom);
                Ok(Expr::Atom(id))
            }
            Token::Left => {
                self.enter()?;
                let expr = self.parse_or()?;
                self.depth -= 1;
                if self.bump()? != Token::Right {
                    bail!("missing closing parenthesis");
                }
                Ok(expr)
            }
            Token::End => bail!("incomplete query expression"),
            other => bail!("unexpected {} in the query expression", describe(&other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn precedence_is_not_then_and_then_or() {
        let query = CompiledQuery::parse(
            &["foo OR bar AND NOT baz".into()],
            false,
            CaseMode::Sensitive,
            false,
        )
        .unwrap();
        assert!(query.expression.evaluate(&[true, false, true]));
        assert!(query.expression.evaluate(&[false, true, false]));
        assert!(!query.expression.evaluate(&[false, true, true]));
    }
    #[test]
    fn wildcard_stays_on_one_line() {
        let query =
            CompiledQuery::parse(&["myVar*end".into()], false, CaseMode::Sensitive, false).unwrap();
        assert_eq!(
            query.atoms[0]
                .find_all(b"myVar123end\nmyVar\nend")
                .spans
                .len(),
            1
        );
    }
    // "valueGenerator" and account-summary sit on adjacent lines, as they do in
    // the json this was reported against.
    const TWO_LINES: &[u8] =
        b"  \"valueGenerator\": {\n    \"template\": \"':aggregations/account-summary/'\"\n";

    fn matches(source: &str, multiline: bool) -> bool {
        let query =
            CompiledQuery::parse(&[source.to_string()], false, CaseMode::Smart, multiline).unwrap();
        let present = query
            .atoms
            .iter()
            .map(|atom| !atom.find_all(TWO_LINES).spans.is_empty())
            .collect::<Vec<_>>();
        query.expression.evaluate(&present)
    }

    #[test]
    fn patterns_stay_on_one_line_until_multiline_is_requested() {
        assert!(!matches("valueGenerator*account-summary", false));
        assert!(matches("valueGenerator*account-summary", true));
        assert!(!matches("/valueGenerator.*account-summary/", false));
        assert!(matches("/valueGenerator.*account-summary/", true));
        // the file-level boolean spans lines regardless of the toggle
        assert!(matches("valueGenerator AND account-summary", false));
    }

    #[test]
    fn multiline_wildcards_stop_at_the_nearest_match() {
        let query =
            CompiledQuery::parse(&["a*b".into()], false, CaseMode::Sensitive, true).unwrap();
        let spans = query.atoms[0].find_all(b"a\nb\nb\nb").spans;
        // lazy, so the first match ends at the first b rather than the last
        assert_eq!(spans[0], (0, 3));
    }

    #[test]
    fn trailing_regex_flags_are_accepted() {
        assert!(matches("/valueGenerator.*account-summary/s", false));
        let insensitive =
            CompiledQuery::parse(&["/VALUEGENERATOR/i".into()], false, CaseMode::Smart, false)
                .unwrap();
        assert!(!insensitive.atoms[0].find_all(TWO_LINES).spans.is_empty());
        assert_eq!(insensitive.atoms[0].spec.flags, "i");
    }

    #[test]
    fn a_boolean_keyword_after_a_regex_is_not_read_as_flags() {
        let query =
            CompiledQuery::parse(&["/foo/AND bar".into()], false, CaseMode::Sensitive, false)
                .unwrap();
        assert_eq!(query.atoms.len(), 2);
        assert_eq!(query.atoms[0].spec.flags, "");
    }

    /// Operators are uppercase only, as documented, so the three commonest
    /// Boolean words stay searchable as ordinary terms.
    #[test]
    fn lowercase_boolean_words_are_search_terms_not_operators() {
        for word in ["and", "or", "not", "And", "Or", "Not"] {
            let query =
                CompiledQuery::parse(&[word.into()], false, CaseMode::Sensitive, false).unwrap();
            assert_eq!(
                query.atoms.len(),
                1,
                "{word} should be one atom, not an operator"
            );
            assert_eq!(query.atoms[0].spec.source, word);
        }
        for word in ["and", "or", "not"] {
            let query =
                CompiledQuery::parse(&[word.into()], false, CaseMode::Sensitive, false).unwrap();
            assert!(
                !query.atoms[0].find_all(b"x and or not y").spans.is_empty(),
                "{word} should match the word in a file"
            );
        }
        let hint = CompiledQuery::parse(&["foo and bar".into()], false, CaseMode::Smart, false)
            .unwrap_err()
            .to_string();
        assert!(hint.contains("operators must be uppercase"), "{hint}");
        assert!(hint.contains("`AND`"), "{hint}");
    }

    /// A quoted phrase and a bare term must need the same number of
    /// backslashes: the lexer used to unescape once and `wildcard_pattern`
    /// again, so `"C:\\Users"` quietly searched for `C:Users`.
    #[test]
    fn quoted_and_bare_terms_escape_identically() {
        let subject = br"prefix C:\Users\admin suffix";
        // what a user types: two backslashes for one literal backslash
        for source in [r"C:\\Users", r#""C:\\Users""#, r"'C:\\Users'"] {
            let query =
                CompiledQuery::parse(&[source.into()], false, CaseMode::Sensitive, false).unwrap();
            assert_eq!(
                query.atoms[0].find_all(subject).spans.len(),
                1,
                "{source} should match a literal backslash"
            );
        }
        // an escaped wildcard is still a literal in both forms
        for source in [r"a\*b", r#""a\*b""#] {
            let query =
                CompiledQuery::parse(&[source.into()], false, CaseMode::Sensitive, false).unwrap();
            assert!(
                query.atoms[0].find_all(b"axxb").spans.is_empty(),
                "{source}"
            );
            assert_eq!(query.atoms[0].find_all(b"a*b").spans.len(), 1, "{source}");
        }
        // an escaped quote inside a phrase survives
        let query = CompiledQuery::parse(
            &[r#""say \"hi\"""#.into()],
            false,
            CaseMode::Sensitive,
            false,
        )
        .unwrap();
        assert_eq!(query.atoms[0].find_all(br#"say "hi""#).spans.len(), 1);
    }

    /// A query whose every satisfying branch is a negation would report zero
    /// matches for files it logically selected, so it is refused up front.
    #[test]
    fn queries_with_nothing_to_find_are_refused() {
        for source in [
            "NOT foo",
            "NOT (foo AND bar)",
            "foo OR NOT bar",
            "NOT foo AND NOT bar",
        ] {
            let error = CompiledQuery::parse(&[source.into()], false, CaseMode::Smart, false)
                .unwrap_err()
                .to_string();
            assert!(error.contains("nothing to find"), "{source}: {error}");
        }
        for source in [
            "foo AND NOT bar",
            "(foo OR bar) AND NOT baz",
            "NOT bar AND foo",
        ] {
            assert!(
                CompiledQuery::parse(&[source.into()], false, CaseMode::Smart, false).is_ok(),
                "{source} should be accepted"
            );
        }
        // multiple positional queries are ORed, so each one needs its own term
        let error = CompiledQuery::parse(
            &["foo".into(), "NOT bar".into()],
            false,
            CaseMode::Smart,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("nothing to find"), "{error}");
    }

    #[test]
    fn unknown_regex_flags_and_stray_tokens_explain_themselves() {
        let flag = CompiledQuery::parse(&["/foo/g".into()], false, CaseMode::Smart, false)
            .unwrap_err()
            .to_string();
        assert!(flag.contains("unknown regex flag `g`"), "{flag}");
        // the message must name the pattern it is complaining about
        assert!(flag.contains("`/foo/g`"), "{flag}");

        let stray = CompiledQuery::parse(&["foo bar".into()], false, CaseMode::Smart, false)
            .unwrap_err()
            .to_string();
        assert!(stray.contains("term `bar`"), "{stray}");
    }

    /// A pathological query must be refused, not abort the process. `bbs serve`
    /// accepts bodies up to 1 MiB, so an unbounded recursive parser was a
    /// remote-ish crash rather than only a silly CLI input.
    #[test]
    fn deep_nesting_is_refused_instead_of_overflowing_the_stack() {
        for depth in [MAX_NESTING + 1, 20_000, 200_000] {
            let source = format!("{}x{}", "(".repeat(depth), ")".repeat(depth));
            let error = CompiledQuery::parse(&[source], false, CaseMode::Smart, false)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("nesting is too deep"),
                "depth {depth}: {error}"
            );
        }
        let nots = format!("{}x", "NOT ".repeat(50_000));
        let error = CompiledQuery::parse(&[nots], false, CaseMode::Smart, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("nesting is too deep"), "{error}");

        // ordinary nesting still parses, and sibling groups do not accumulate
        let wide = (0..500)
            .map(|i| format!("(a{i} OR b{i})"))
            .collect::<Vec<_>>()
            .join(" AND ");
        assert!(CompiledQuery::parse(&[wide], false, CaseMode::Smart, false).is_ok());
        assert!(
            CompiledQuery::parse(
                &["((((((((((x))))))))))".into()],
                false,
                CaseMode::Smart,
                false
            )
            .is_ok()
        );
    }

    #[test]
    fn regex_atom_preserves_escapes() {
        let query =
            CompiledQuery::parse(&[r"/\bfoo\d+\b/".into()], false, CaseMode::Sensitive, false)
                .unwrap();
        assert_eq!(query.atoms[0].find_all(b"foo42 nofoo").spans.len(), 1);
    }
}
