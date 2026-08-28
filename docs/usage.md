# Usage

## Commands

| Command | Purpose |
| --- | --- |
| `bbs <query>...` | Search |
| `bbs login` | Save and validate a token |
| `bbs logout` | Remove the saved token |
| `bbs repos` | List accessible repositories |
| `bbs serve` | Start the browser interface |
| `bbs update` | Upgrade to the latest release |
| `bbs cache status\|prune\|clear-results` | Inspect and reclaim cache |

## Exit codes

| Code | Search | `update --check` |
| --- | --- | --- |
| 0 | matches found | already current |
| 1 | no matches | update available |
| 2 | error | error |

Use these in scripts: `bbs "TODO" --format json || [ $? -eq 1 ]`.

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

`NOT` binds tightest, then `AND`, then `OR`. Operators must be uppercase.

There is no implicit `AND`. `foo bar` is an error; write `foo AND bar`, or `"foo bar"` for a literal phrase.

Parentheses are grouping syntax. To match a literal one, quote the term or escape it: `"parse_query("` or `parse_query\(`.

Regex flags: `i` ignore case, `s` `.` matches newlines, `m` `^` and `$` match at line breaks, `x` ignore whitespace in the pattern.

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

Force with `-i` (ignore) or `-s` (sensitive).

## Scoping

```sh
bbs "query" --repos api                    # unique short name
bbs "query" --repos team/api               # workspace-qualified
bbs "query" --repos api,web,docs           # comma-separated
bbs "query" --repos api web docs           # or space-separated
bbs "query" --path "src/**/*.ts"           # repeatable
bbs "query" --path "src/**" --path "docs/**"   # repeat to widen
bbs "query" --branch release/2.x
```

Quote globs so the shell does not expand them. Short repository names must be unique across workspaces; otherwise use `workspace/slug`.

Path globs support `*`, `?`, character classes, and `**`. `*` does not cross `/`; `**` does. Repeated `--path` widens the search; there is no negation, so exclude with `NOT` in the query instead.

## Output

```sh
bbs "query" --format json          # one object, pretty-printed
bbs "query" --format jsonl         # one result per line
bbs "query" --color never          # no ANSI
bbs "query" --context 6            # lines of context, default 2
bbs "query" --max-results 50       # default 500
bbs "query" --sort path            # relevance (default), repo, path
```

`jsonl` streams well:

```sh
bbs "TODO" --format jsonl | jq -r '"\(.repository)/\(.path):\(.lines[0].number)"'
```

Use `--sort path` or `--sort repo` for stable, diffable output.

## Ranking

Relevance favours, in rough order of weight: more distinct query terms matched, terms appearing in the file path, matches close together, higher match density, and matches near the top of the file. Paths containing `vendor`, `generated`, `dist`, `build`, or `node_modules` are demoted.

To bias toward a directory, include it as a term: `bbs 'parser AND src'`.

## Freshness

Every search syncs the selected snapshots first, then scans.

```sh
bbs "query" --offline     # skip the network, use last cached commits
bbs "query" --no-cache    # rescan even if results are cached
bbs repos --offline
```

Offline results are labelled stale and report the cached commit. Results cache on exact commit SHAs, so a fetch invalidates them automatically.

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
BB_TOKEN=... bbs "query"        # environment wins over the stored token
```

Token scopes: `read:workspace:bitbucket`, `read:repository:bitbucket`.

## Updating

```sh
bbs update
bbs update --check
```

`--check` exits 1 when an update exists, so `bbs update --check || bbs update` upgrades only when needed.

## Cache

```sh
bbs cache status          # sizes and entry counts as JSON
bbs cache prune           # trim to the configured budgets
bbs cache clear-results   # drop cached results, keep snapshots
```

`prune` removes least-recently-used snapshots and results. `clear-results` keeps clones, so the next search rescans without refetching.

## Configuration

`config.toml` in the config directory:

- Linux `~/.config/better-bitbucket-search/`
- macOS `~/Library/Application Support/dev.bbs.better-bitbucket-search/`
- Windows `%APPDATA%\bbs\better-bitbucket-search\config\`

```toml
default_port = 7337
sync_concurrency = 6        # repositories fetched in parallel
max_file_bytes = 4194304    # files larger than this are skipped
max_results = 500
context_lines = 2
snapshot_budget_gb = 20
result_budget_mb = 1024
```

Cache lives beside it: `~/.cache/better-bitbucket-search/` on Linux.

## Environment variables

| Variable | Effect |
| --- | --- |
| `BB_TOKEN` | Token, overrides the credential store |
| `BBS_REPOSITORY` | Release repository for `bbs update` |
| `BBS_VERSION`, `BBS_INSTALL_DIR` | Used by the install scripts |

## Limits

- Only tracked UTF-8 text files are searched. Binary files, submodule contents, and Git LFS payloads are skipped.
- Files above `max_file_bytes` are skipped.
- At most 20,000 matches per term per file; beyond that the response is marked truncated.
- One branch per repository per search. No history search.

## Recipes

```sh
# what changed hands: find a symbol everywhere except tests
bbs 'PaymentIntent AND NOT /test|spec/'

# config drift across repositories
bbs '"apiVersion:"' --path "**/*.y*ml" --sort repo

# a definition and its call site in the same file
bbs '"fn parse_query" AND "parse_query("'

# every TODO with an owner
bbs -r 'TODO\([a-z.]+\)' --format jsonl

# audit one directory of one repository
bbs 'auth' --repos team/api --path "src/server/**"

# fastest possible repeat query
bbs 'query' --offline
```
