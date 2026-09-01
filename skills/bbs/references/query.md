# `bbs` query grammar

## Atoms

| Form | Meaning |
| --- | --- |
| `foo` | Literal term |
| `"foo bar"` | Literal phrase, spaces included |
| `foo*bar` | `*` matches any run of characters |
| `fo?` | `?` matches exactly one character |
| `/re/` | PCRE2 regular expression |
| `/re/isxm` | Regular expression with flags |

## Operators

| Form | Meaning |
| --- | --- |
| `a AND b` | Both present in the same file |
| `a OR b` | Either present |
| `NOT a` | Absent |
| `(a OR b) AND c` | Grouping |

`NOT` binds tightest, then `AND`, then `OR`. Operators must be uppercase, so `and`,
`or`, and `not` remain ordinary search terms.

Boolean expressions are evaluated **per file**, not per line. `foo AND bar` matches a
file containing both, however far apart they sit — that is almost always what you want,
and it is cheaper and more robust than a multiline regex.

Multiple positional queries are ORed and deduplicated:

```sh
bbs 'getUser' 'fetchUser'
```

Parentheses are grouping syntax. For a literal one, quote or escape the term:
`'"parse_query("'` or `'parse_query\('`.

## Refusals

There is no implicit `AND`, and several queries are rejected rather than run, because
each would otherwise look like an ordinary empty result:

```console
$ bbs 'foo bar'
error: unexpected term `bar` after the query expression; join terms with AND, OR, or NOT

$ bbs 'foo and bar'
error: operators must be uppercase; write `AND` instead of `and`

$ bbs 'NOT deprecated'
error: this query has nothing to find: every way of satisfying it is a `NOT`, so there
would be no matches to show. Combine it with a term, for example `foo AND NOT bar`

$ bbs '*'
error: `*` matches at every position rather than searching for anything; add a term,
or list files with `--path <glob> --files-with-matches`
```

`""`, `//`, `*`, `?`, and `/a*/` all match the empty string or every character, so they
are refused. To enumerate files instead of matching content, use
`--path <glob> --files-with-matches`.

## Escapes

Backslash escapes the next character, identically in bare and quoted terms. A literal
backslash is `\\`:

```sh
bbs 'C:\\Users'
bbs '"C:\\Users"'
```

`\t`, `\n`, and `\r` mean tab, newline, and carriage return. Every other escape means
the character that follows it, so `\.` is a literal dot and `\*` a literal asterisk.

## Regex flags

| Flag | Effect |
| --- | --- |
| `i` | Ignore case |
| `c` | Force case-sensitive (the inverse of `i`; under `-i` it brings one term back) |
| `s` | `.` matches newlines |
| `m` | `^` and `$` match at line breaks |
| `x` | Ignore whitespace in the pattern |

`-r`/`--regex` treats each complete query as raw PCRE2, with no surrounding slashes and
no Boolean parsing:

```sh
bbs -r '\bmyVar\.\d[A-Za-z0-9_$]*[a-z]\b'
```

## Line boundaries

Wildcards and `.` stop at line breaks by default. Three ways across:

```sh
bbs 'valueGenerator AND account-summary'      # anywhere in the file, any order
bbs '/valueGenerator.*?account-summary/s'     # ordered, s flag
bbs -M 'valueGenerator*account-summary'       # ordered, multiline mode
```

Prefer `AND` unless order or adjacency matters. `-M`/`--multiline` matches lazily,
stopping at the nearest hit rather than the last one in the file.

## Case

Smart-case by default, applied **per term**: a term containing an uppercase letter is
case-sensitive, one that is all lowercase is not.

```sh
bbs 'getuser AND FetchUser'   # first insensitive, second sensitive
```

Smart case reads only the characters that stand for themselves. Regex syntax is not
evidence of intent, so `/todo\D/` and `/todo\s/` are both insensitive; `\S`, `\W`,
`\B`, `\A`, `\Z`, `\K`, and `\p{...}` behave the same way. Text inside `\Q...\E` *is*
literal and does count.

Force the whole query with `-i` (ignore) or `-s` (sensitive), or one term with the `i`
and `c` regex flags.

## Word boundaries

```sh
bbs -w 'getUser'    # matches getUser, not getUserById
```

`-w`/`--word` requires a word boundary either side of every term. It applies to regex
atoms too, and unlike writing `/\bfoo\b/` by hand it does not change how the term is
case-matched.

## Ranking

Relevance favours, in rough order of weight: more distinct query terms matched, terms
appearing in the file path, matches close together, higher match density, and matches
near the top of the file. Paths containing `vendor`, `generated`, `dist`, `build`, or
`node_modules` are demoted.

To bias toward a directory, include it as a term: `bbs 'parser AND src'`.
