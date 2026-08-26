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

impl CompiledAtom {
    pub fn find_all(&self, bytes: &[u8]) -> Result<Vec<(usize, usize)>> {
        let mut output = Vec::new();
        for item in self.regex.find_iter(bytes) {
            let found = item?;
            output.push((found.start(), found.end()));
            if output.len() >= 20_000 {
                break;
            }
        }
        Ok(output)
    }
}

#[derive(Debug)]
pub struct CompiledQuery {
    pub sources: Vec<String>,
    pub expression: Expr,
    pub atoms: Vec<CompiledAtom>,
    pub case_mode: CaseMode,
}

impl CompiledQuery {
    pub fn parse(sources: &[String], raw_regex: bool, case_mode: CaseMode) -> Result<Self> {
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
            .map(|spec| compile_atom(spec, case_mode))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            sources: sources.to_vec(),
            expression,
            atoms: compiled,
            case_mode,
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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryFingerprint {
    pub sources: Vec<String>,
    pub expression: Expr,
    pub atoms: Vec<AtomSpec>,
    pub case_mode: CaseMode,
}

fn compile_atom(spec: AtomSpec, case_mode: CaseMode) -> Result<CompiledAtom> {
    let pattern = match spec.kind {
        AtomKind::Regex => spec.source.clone(),
        AtomKind::Wildcard => wildcard_pattern(&spec.source),
    };
    let sensitive = match case_mode {
        CaseMode::Sensitive => true,
        CaseMode::Ignore => false,
        CaseMode::Smart => spec.source.chars().any(|c| c.is_uppercase()),
    };
    let regex = RegexBuilder::new()
        .caseless(!sensitive)
        .utf(true)
        .ucp(true)
        .build(&pattern)
        .map_err(|error| anyhow::anyhow!("invalid query `{}`: {error}", spec.source))?;
    Ok(CompiledAtom { spec, regex })
}

fn wildcard_pattern(source: &str) -> String {
    let mut output = String::new();
    let mut chars = source.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some(next) => push_escaped(&mut output, next),
                None => output.push_str("\\\\"),
            },
            '*' => output.push_str("[^\\r\\n]*"),
            '?' => output.push_str("[^\\r\\n]"),
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
                return Ok(Token::Atom(AtomSpec {
                    source,
                    kind: AtomKind::Regex,
                }));
            } else {
                source.push(ch);
            }
        }
        bail!("unterminated /regex/ atom")
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
            })),
        }
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
            bail!("unexpected token after query expression");
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
            other => bail!("unexpected token {other:?}"),
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
        )
        .unwrap();
        assert!(query.expression.evaluate(&[true, false, true]));
        assert!(query.expression.evaluate(&[false, true, false]));
        assert!(!query.expression.evaluate(&[false, true, true]));
    }
    #[test]
    fn wildcard_stays_on_one_line() {
        let query =
            CompiledQuery::parse(&["myVar*end".into()], false, CaseMode::Sensitive).unwrap();
        assert_eq!(
            query.atoms[0]
                .find_all(b"myVar123end\nmyVar\nend")
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn regex_atom_preserves_escapes() {
        let query =
            CompiledQuery::parse(&[r"/\bfoo\d+\b/".into()], false, CaseMode::Sensitive).unwrap();
        assert_eq!(query.atoms[0].find_all(b"foo42 nofoo").unwrap().len(), 1);
    }
}
