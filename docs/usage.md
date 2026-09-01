# Usage

## Commands

| Command | Purpose |
| --- | --- |
| `bbs <query>...` | Search |
| `bbs login` | Save and validate a token |
| `bbs logout` | Remove the saved token |
| `bbs list repos [filter]` | List accessible repositories (`bbs repos` is shorthand) |
| `bbs serve` | Start the browser interface |
| `bbs update` | Upgrade to the latest release |
| `bbs cache status\|prune\|clear-results\|forget` | Inspect and reclaim cache |

## Exit codes

| Code | Search | `update --check` |
| --- | --- | --- |
| 0 | matches found | already current |
| 1 | no matches | update available |
| 2 | error | error |

Use these in scripts: `bbs "TODO" --format json || [ $? -eq 1 ]`. The code follows what
*matched*, not what was displayed, so `--max-results 0` on a query with matches still exits `0`.

## Query syntax

| Form | Meaning |
| --- | --- |
| `foo` | Literal term |
| `"foo bar"` | Literal phrase, spaces included |
| `foo*bar` | `*` matches any run of characters |
| `fo?` | `?` matches one character |
| `/re/` | PCRE2 regular expression |
| `/re/isxm` | Regular expression with flags |
| `a AND b` | Both present in the same file |
| `a OR b` | Either present |
| `NOT a` | Absent |
| `(a OR b) AND c` | Grouping |

`NOT` binds tightest, then `AND`, then `OR`. Operators must be uppercase, so `and`, `or` and `not` remain ordinary search terms. A lowercase keyword used as an operator is reported rather than guessed at:

```console
$ bbs 'foo and bar'
error: operators must be uppercase; write `AND` instead of `and`
```

There is no implicit `AND`. `foo bar` is an error; write `foo AND bar`, or `"foo bar"` for a literal phrase.

A query must have something to find. `NOT x` on its own, and `a OR NOT b`, are true for files while pointing at nothing in them, so they are refused instead of reporting no matches:

```console
$ bbs 'NOT deprecated'
error: this query has nothing to find: every way of satisfying it is a `NOT` ...
```

Write `foo AND NOT deprecated` instead.

Parentheses are grouping syntax. To match a literal one, quote the term or escape it: `"parse_query("` or `parse_query\(`.

Backslash escapes the next character, identically in bare and quoted terms. A literal backslash is `\\`:

```sh
bbs 'C:\\Users'        # both find C:\Users
bbs '"C:\\Users"'
```

`\t`, `\n` and `\r` mean tab, newline and carriage return. Every other escape means the character
that follows it, so `\.` is a literal dot and `\*` a literal asterisk.

Regex flags: `i` ignore case, `c` force case-sensitive, `s` `.` matches newlines, `m` `^` and `$` match at line breaks, `x` ignore whitespace in the pattern.

`c` is the inverse of `i`: under `-i` it brings a single term back to case-sensitive.

A pattern that matches at every position is refused rather than run. `""`, `//`, `*`, `?` and
`/a*/` all match the empty string or every character, so they hit the per-file match cap in every
file and report the whole corpus as truncated:

```console
$ bbs '*'
error: `*` matches at every position rather than searching for anything; add a term, or list
files with `--path <glob> --files-with-matches`
```

Multiple positional queries are ORed:

```sh
bbs "getUser" "fetchUser"
```

### Line boundaries

Wildcards and `.` stop at line breaks by default. Three ways to reach across lines:

```sh
bbs 'valueGenerator AND account-summary'          # anywhere in the file, any order
bbs '/valueGenerator.*?account-summary/s'         # ordered, s flag
bbs -M 'valueGenerator*account-summary'           # ordered, multiline mode
```

Prefer `AND` unless order or adjacency matters. `-M`/`--multiline` matches lazily, stopping at the nearest hit.

### Case

Smart-case by default and applied **per term**: a term containing an uppercase letter is case-sensitive, one that is all lowercase is not.

```sh
bbs 'getuser AND FetchUser'   # first is insensitive, second is sensitive
```

Smart case reads only the characters that stand for themselves. Regex syntax is not evidence of
intent, so `/todo\D/` and `/todo\s/` are both insensitive; `\S`, `\W`, `\B`, `\A`, `\Z`, `\K` and
`\p{...}` behave the same way. Text inside `\Q...\E` *is* literal and does count.

Force the whole query with `-i` (ignore) or `-s` (sensitive), or one term with the `i` and `c`
regex flags.

### Word boundaries

```sh
bbs -w getUser       # matches getUser, not getUserById
```

`-w`/`--word` requires a word boundary either side of every term. It applies to regex atoms too,
and unlike writing `/\bfoo\b/` by hand it does not change how the term is case-matched.

## Listing repositories

```sh
bbs list repos                              # every accessible repository
bbs list repos api                          # substring, positionally
bbs list repos --filter api                 # or by flag; the same thing
bbs list repos --filter 'edge-*'            # glob
bbs list repos --filter '/^team\/(api|web)/' # PCRE2 regex
bbs list repos -r '^team/(api|web)'         # regex without the slashes
bbs list repos --offline                    # last discovered catalog, no network
bbs list repos --json                       # the catalog, machine-readable
```

`bbs repos` is shorthand for `bbs list repos` and takes the same options.

The filter reads in one of three forms, the same three the query language uses, so there is no
second syntax to learn:

| Filter | Read as | Matches |
| --- | --- | --- |
| `api` | substring | anywhere in the name |
| `edge-*`, `api-?`, `[ab]*` | glob | the whole name |
| `/^api/`, `/gateway$/i` | PCRE2 regex | anywhere in the name |
| `-r '^api'` | PCRE2 regex | anywhere in the name |

A regex takes the same trailing flags a query atom does — `i`, `c`, `s`, `m`, `x`. Every form is
matched against the repository's slug, its `workspace/slug` full name, **and** its display name,
and every form ignores case unless a regex says `c`. Anchors bind to each of those names
separately, so `/^api/` still finds `team/api-gateway` by way of its slug.

Only a filter that opens with `/` *and* closes with one is a regex. A leading slash on its own
stays a substring, so `bbs list repos /api` still lists every repository whose slug starts with
`api`, in any workspace.

A filter that cannot be compiled is an error naming what is wrong, not an empty listing:

```console
$ bbs list repos --filter 'api['
error: invalid filter `api[`: error parsing glob 'api[': unclosed character class; missing ']'
```

Quote patterns so the shell does not expand them. When a filter is in play the listing ends with
`N of M repositories`, so an over-narrow pattern looks like a narrow filter rather than an empty
account.

## Scoping

```sh
bbs "query" --repos api                    # unique short name
bbs "query" --repos team/api               # workspace-qualified
bbs "query" --repos api,web,docs           # comma-separated
bbs "query" --repos api web docs           # or space-separated
bbs "query" --repos 'edge-*'               # glob pattern
bbs "query" --branch release/2.x
```

Quote patterns so the shell does not expand them. Short repository names must be unique across workspaces; otherwise use `workspace/slug`. A name that is not accessible is offered the closest one that is:

```console
$ bbs "query" --repos api-gatewy
error: repository `api-gatewy` is not accessible; did you mean `api-gateway`?
```

A glob selects every repository it matches, so the uniqueness rule does not apply to it; a glob matching nothing is an error rather than an empty scope.

### Path filters

```sh
bbs "query" --path "src/**/*.ts"                 # repeatable
bbs "query" --path "src/**" --path "docs/**"     # repeat to widen
bbs "query" --exclude-path "**/test/**"          # repeatable
bbs "query" --path '!vendor/**'                  # same thing, gitignore spelling
bbs "query" --no-vendor                          # vendor, generated, dist, build, node_modules
```

Path globs support `*`, `?`, character classes, and `**`. `*` does not cross `/`; `**` does.

| You write | It means |
| --- | --- |
| `*.md` | every `.md` file at any depth |
| `./*.md`, `/*.md` | `.md` files in the repository root only |
| `src/`, `src` | everything under a `src` directory |
| `src/**` | everything under the root `src` |
| `!vendor/**`, `--exclude-path vendor` | everything except the vendor tree |

A pattern containing no `/` matches at any depth, the way ripgrep's `--glob` does; a leading `./`
or `/` anchors it to the repository root. A bare directory name selects or excludes its whole
tree.

Include patterns widen, exclude patterns narrow, and a file has to pass both. A filter that
removes every candidate says so, rather than looking like an empty result set:

```console
$ bbs "query" --path 'src/'
warning: no file matched --path `src/`; 383102 files were considered
```

`--no-vendor` excludes exactly the directories the relevance ranking already demotes.

## Output

```sh
bbs "query" --format json          # one object, pretty-printed
bbs "query" --format jsonl         # one result per line, then a summary
bbs "query" --color never          # no ANSI
bbs "query" --context 6            # lines of context, default 2
bbs "query" --max-results 50       # default 500
bbs "query" --sort path            # relevance (default), repo, path
bbs "query" -l                     # repository and path only
bbs "query" --count                # match count per file
bbs "query" --stats                # sync time and scan time separately
```

`--sort`, `--max-results`, `--context`, `-l` and `--count` change only what is shown. They are not
part of the result-cache key, so re-asking the same query in a different shape is served from the
cache rather than rescanning.

Use `--sort path` or `--sort repo` for stable, diffable output; `--sort repo` groups results under
a per-repository header with file and match counts.

The summary line names what the numbers hide — files the scan walked past, results the limit
dropped, and why a search stopped short:

```
7 of 412 results across 69 repositories (383102 files, 3 skipped: 2 too large, 1 binary, 8460 ms);
stopped early: pattern too expensive in 3 files
```

`pattern too expensive` is the one worth acting on: PCRE2 abandoned the pattern in those files, so
results may be materially incomplete. A result or match cap is benign.

### jsonl

Every result is one line carrying `"type":"result"` with its fields at the top level, followed by
one `{"type":"summary", ...}` line:

```sh
bbs "TODO" --format jsonl | jq -r 'select(.type=="result") | "\(.repository)/\(.path):\(.lines[0].number)"'
bbs "TODO" --format jsonl | jq 'select(.type=="summary") | .truncation'
```

The summary carries `total_results`, `results_shown`, `files_searched`, `skipped_files`, `skipped`,
`truncation`, `cached`, `offline`, `elapsed_ms`, `sync_ms` and `scan_ms` — the things a script
should react to, none of which were reachable in the streaming format before.

## Ranking

Relevance favours, in rough order of weight: more distinct query terms matched, terms appearing in the file path, matches close together, higher match density, and matches near the top of the file. Paths containing `vendor`, `generated`, `dist`, `build`, or `node_modules` are demoted.

To bias toward a directory, include it as a term: `bbs 'parser AND src'`.

## Freshness

Every search syncs the selected snapshots first, then scans.

```sh
bbs "query" --offline       # skip the network, use last cached commits
bbs "query" --max-age 5m    # reuse anything fetched in the last five minutes
bbs "query" --no-cache      # rescan even if results are cached
bbs list repos --offline
```

Offline results are labelled stale and report the cached commit. Results cache on exact commit SHAs, so a fetch invalidates them automatically.

`--max-age` is the middle ground between fetching everything and pretending to be offline. It
takes `30s`, `5m`, `1h30m`, `2d`, or a bare number of seconds, and covers repository discovery as
well as the snapshots, which is what makes a repeat query on a warm cache near-instant. A snapshot
reused inside the window is **not** labelled stale — the freshness you asked for was met — while
`synchronized_at` still reports its real age. Unlike `--offline`, a repository that was never
synced is still fetched rather than skipped.

## Browser interface

```sh
bbs serve
bbs serve --port 8080
bbs serve --no-open
```

- `Ctrl`/`⌘` + `K` focuses the query box.
- Filters mirror the CLI: path, branch, case, sort, raw regex, multi-line, offline.
- The repository picker filters as you type; empty means all.
- Click the chevron on a result to collapse it.
- Badges mark cache hits, stale results, and offline mode.
- Result paths link to the exact commit and line in Bitbucket.
- Cancel stops a running search.

## Authentication

```sh
bbs login                       # prompts, no echo
echo "$TOKEN" | bbs login --token-stdin
BB_TOKEN=... bbs "query"        # used only if there is no saved credential
bbs --env-token "query"         # use BB_TOKEN even though one is saved
```

Token scopes: `read:workspace:bitbucket`, `read:repository:bitbucket`.

`bbs` prefers the credential saved by `bbs login`. `BB_TOKEN` is a fallback,
not a rival: it is what an account that has never logged in searches with, and
what a saved credential falls through to once Bitbucket answers 401 — so an
expired token is a warning on stderr rather than a failed run. `--env-token`
reverses the order for one run, and fails outright if `BB_TOKEN` is unset
rather than quietly using the credential it was asked to bypass.

## Updating

```sh
bbs update
bbs update --check
```

`--check` exits 1 when an update exists, so `bbs update --check || bbs update` upgrades only when needed.

## Cache

```sh
bbs cache status              # sizes and entry counts as JSON
bbs cache status --verbose    # every snapshot: repository, branch, commit, age, size
bbs cache prune               # trim to the configured budgets
bbs cache clear-results       # drop cached results, keep snapshots
bbs cache forget team/api     # drop one repository's snapshots, all branches
```

`prune` removes least-recently-used snapshots and results. `clear-results` keeps clones, so the next search rescans without refetching. `forget` takes the same names and patterns as `--repos`.

Each snapshot is described by a `<branch-hash>.meta.json` file beside it, naming its repository,
branch, commit and fetch time. That is what `status --verbose` reads, what `--max-age` checks, and
what an offline search falls back to when `catalog.json` is missing or corrupt — the catalog is
derived data, so a corrupt one reports how to rebuild it rather than being fatal.

## Configuration

`config.toml` in the config directory:

- Linux `~/.config/better-bitbucket-search/`
- macOS `~/Library/Application Support/dev.bbs.better-bitbucket-search/`
- Windows `%APPDATA%\bbs\better-bitbucket-search\config\`

```toml
default_port = 7337
sync_concurrency = 6         # repositories fetched in parallel
max_file_bytes = 4194304     # files larger than this are skipped
max_results = 500
context_lines = 2
cache_context_lines = 6      # context stored, so a narrower --context is a cache hit
cache_max_results = 2000     # results stored, so any --sort order is a cache hit
snapshot_budget_gb = 20
result_budget_mb = 1024
```

`cache_context_lines` and `cache_max_results` set how wide each scan is stored. A request narrower
than both is answered from the stored scan; a wider one rescans and stores at the larger size.

Cache lives beside it: `~/.cache/better-bitbucket-search/` on Linux.

## Environment variables

| Variable | Effect |
| --- | --- |
| `BB_TOKEN` | Token. A fallback behind the saved credential; `--env-token` puts it first |
| `BBS_REPOSITORY` | Release repository for `bbs update` |
| `BBS_VERSION`, `BBS_INSTALL_DIR` | Used by the install scripts |

## Limits

- Only tracked UTF-8 text files are searched. Binary files, submodule contents, and Git LFS payloads are skipped. A file counts as binary if a NUL byte appears in its first 8 KiB.
- Files above `max_file_bytes` are skipped.
- Every skipped file is counted by reason and reported in `skipped_files` and in the summary line, so a file the scan walked past is never silently absent.
- At most 20,000 matches per term per file; beyond that the response is marked truncated. `truncation` says which of the three causes applied.
- One branch per repository per search. No history search.
- A repository that cannot contribute is skipped, not fatal: no commits yet, no such branch, or (offline) no cached snapshot. Each one is named on stderr and listed in `skipped` in JSON output. The search fails only when nothing at all could be searched.

## Recipes

```sh
# find a symbol everywhere except the test and vendored trees
bbs 'PaymentIntent' --exclude-path '**/test/**' --no-vendor

# which files mention it at all, across one family of repositories
bbs 'PaymentIntent' --repos 'edge-*' -l

# how heavily, per file
bbs 'PaymentIntent' --count --sort repo

# an identifier, not a substring of one
bbs -w 'getUser'

# config drift across repositories
bbs '"apiVersion:"' --path "**/*.y*ml" --sort repo

# a definition and its call site in the same file
bbs '"fn parse_query" AND "parse_query("'

# every TODO with an owner
bbs -r 'TODO\([a-z.]+\)' --format jsonl

# audit one directory of one repository
bbs 'auth' --repos team/api --path "src/server/**"

# fastest possible repeat query, still verified within five minutes
bbs 'query' --max-age 5m

# fastest possible repeat query, no network at all
bbs 'query' --offline
```
