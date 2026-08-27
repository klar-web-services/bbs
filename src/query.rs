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

/// Matches collected for one atom in one file. `truncated` records that the
/// scan stopped early, either at the cap or because PCRE2 gave up on a
/// pathological pattern. Neither case is worth failing an entire search over,
/// so the matches found so far are reported instead.
#[derive(Debug, Default)]
pub struct AtomMatches {
    pub spans: Vec<(usize, usize)>,
    pub truncated: bool,
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
                        found.truncated = true;
                        break;
                    }
                }
                // PCRE2 reports its own limits here, e.g. the backtracking
                // match limit on a catastrophic pattern.
                Err(_) => {
                    found.truncated = true;
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
                let flags = self.regex_flags()?;
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
    fn regex_flags(&mut self) -> Result<String> {
        let start = self.pos;
        while self.pos < self.chars.len() && self.chars[self.pos].is_ascii_alphabetic() {
            self.pos += 1;
        }
        let run: String = self.chars[start..self.pos].iter().collect();
        if matches!(run.to_ascii_uppercase().as_str(), "AND" | "OR" | "NOT") {
            self.pos = start;
            return Ok(String::new());
        }
        if let Some(unknown) = run
            .chars()
            .find(|flag| !matches!(flag, 'i' | 'm' | 's' | 'x'))
        {
            bail!(
                "unknown regex flag `{unknown}` in `/{}/{run}`; supported flags are i (ignore case), s (. matches newlines), m (^ and $ match at line breaks), and x (ignore whitespace)",
                self.chars[..start]
                    .iter()
                    .collect::<String>()
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
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
        match source.to_ascii_uppercase().as_str() {
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

struct Parser<'a, 'b> {
    lexer: Lexer<'a>,
    lookahead: Token,
    atoms: &'b mut Vec<AtomSpec>,
}

impl<'a, 'b> Parser<'a, 'b> {
    fn new(source: &'a str, atoms: &'b mut Vec<AtomSpec>) -> Result<Self> {
        let mut lexer = Lexer::new(source);
        let lookahead = lexer.next()?;
        Ok(Self {
            lexer,
            lookahead,
            atoms,
        })
    }
    fn bump(&mut self) -> Result<Token> {
        let current = std::mem::replace(&mut self.lookahead, Token::End);
        self.lookahead = self.lexer.next()?;
        Ok(current)
    }
    fn parse(mut self) -> Result<Expr> {
        let expr = self.parse_or()?;
        if self.lookahead != Token::End {
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
            Ok(Expr::Not(Box::new(self.parse_not()?)))
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
                let expr = self.parse_or()?;
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

    #[test]
    fn unknown_regex_flags_and_stray_tokens_explain_themselves() {
        let flag = CompiledQuery::parse(&["/foo/g".into()], false, CaseMode::Smart, false)
            .unwrap_err()
            .to_string();
        assert!(flag.contains("unknown regex flag `g`"), "{flag}");

        let stray = CompiledQuery::parse(&["foo bar".into()], false, CaseMode::Smart, false)
            .unwrap_err()
            .to_string();
        assert!(stray.contains("term `bar`"), "{stray}");
    }

    #[test]
    fn regex_atom_preserves_escapes() {
        let query =
            CompiledQuery::parse(&[r"/\bfoo\d+\b/".into()], false, CaseMode::Sensitive, false)
                .unwrap();
        assert_eq!(query.atoms[0].find_all(b"foo42 nofoo").spans.len(), 1);
    }
}
