# `bbs` command reference

## Commands

| Command | Purpose |
| --- | --- |
| `bbs <query>...` | Search |
| `bbs auth status` | Report whether a credential is present. Local-only; `--verify` adds a round trip |
| `bbs login` | Save and validate a token. **Interactive — never run this from an agent** |
| `bbs logout` | Remove the saved token |
| `bbs list repos [filter]` | List accessible repositories (`bbs repos` is shorthand) |
| `bbs serve` | Start the browser interface. **Blocking — never run this from an agent** |
| `bbs skill` | Install this skill into the coding agents on this machine |
| `bbs update` | Upgrade to the latest release |
| `bbs cache status\|prune\|clear-results\|forget` | Inspect and reclaim cache |

## Exit codes

| Code | Search | `auth status` | `update --check` |
| --- | --- | --- | --- |
| 0 | matches found | credential present | already current |
| 1 | no matches | no credential | update available |
| 2 | error | error | error |

The search code follows what *matched*, not what was displayed, so `--max-results 0` on
a query with matches still exits `0`. In a chain: `bbs 'q' --format json || [ $? -eq 1 ]`.

## Scoping

```sh
bbs 'query' --repos api                    # unique short name
bbs 'query' --repos team/api               # workspace-qualified
bbs 'query' --repos api,web,docs           # comma- or space-separated
bbs 'query' --repos 'edge-*'               # glob over repository names
bbs 'query' --branch release/2.x           # one branch, every selected repository
```

Short names must be unique across workspaces; otherwise use `workspace/slug`. An
inaccessible name is offered the closest accessible one:

```console
$ bbs 'query' --repos api-gatewy
error: repository `api-gatewy` is not accessible; did you mean `api-gateway`?
```

A glob selects everything it matches, so the uniqueness rule does not apply to it. A
glob matching nothing is an error rather than an empty scope.

## Path filters

```sh
bbs 'query' --path 'src/**/*.ts'                 # repeatable
bbs 'query' --path 'src/**' --path 'docs/**'     # repeat to widen
bbs 'query' --exclude-path '**/test/**'          # repeatable
bbs 'query' --path '!vendor/**'                  # same thing, gitignore spelling
bbs 'query' --no-vendor                          # vendor generated dist build node_modules
```

`*` does not cross `/`; `**` does.

| You write | It means |
| --- | --- |
| `*.md` | every `.md` file at any depth |
| `./*.md`, `/*.md` | `.md` files in the repository root only |
| `src/`, `src` | everything under a `src` directory, at any depth |
| `src/**` | everything under the root `src` |
| `!vendor/**`, `--exclude-path vendor` | everything except the vendor tree |

Include patterns widen, exclude patterns narrow, and a file must pass both. A filter
that removes every candidate warns rather than looking like an empty result set:

```console
$ bbs 'query' --path 'src/'
warning: no file matched --path `src/`; 383102 files were considered
```

Note the asymmetry: `NOT` excludes on **content**, `--exclude-path` on **path**.

## Listing repositories

```sh
bbs list repos                               # every accessible repository
bbs list repos api                           # substring, positionally
bbs list repos --filter 'edge-*'             # glob
bbs list repos --filter '/^team\/(api|web)/' # PCRE2 regex
bbs list repos -r '^team/(api|web)'          # regex without the slashes
bbs list repos --offline                     # last discovered catalog, no network
bbs list repos --json                        # machine-readable
```

The filter takes the same three forms a query term does — substring, glob, or `/regex/`
with `icsmx` flags — and is matched against each repository's slug, its `workspace/slug`
full name, **and** its display name. Anchors bind to each separately, so `/^api/` still
finds `team/api-gateway`. Only a filter that both opens and closes with `/` is a regex.

When a filter is in play the listing ends with `N of M repositories`, so an over-narrow
pattern reads as a narrow filter rather than an empty account.

## Output

```sh
bbs 'query' --format json          # one object, pretty-printed
bbs 'query' --format jsonl         # one result per line, then a summary
bbs 'query' --context 6            # lines of context, default 2
bbs 'query' --max-results 50       # default 500
bbs 'query' --sort path            # relevance (default), repo, path
bbs 'query' -l                     # repository and path only
bbs 'query' --count                # match count per file
bbs 'query' --stats                # sync time and scan time separately
bbs 'query' --color never          # force ANSI off (already off when piped)
```

`--sort`, `--max-results`, `--context`, `-l`, and `--count` change only what is shown.
They are not part of the result-cache key, so re-asking the same query in a different
shape is served from cache rather than rescanning. Use `--sort path` or `--sort repo`
for stable, diffable output.

The summary line names what the numbers hide:

```
7 of 412 results across 69 repositories (383102 files, 3 skipped: 2 too large, 1 binary,
8460 ms); stopped early: pattern too expensive in 3 files
```

`pattern too expensive` is the one worth acting on — PCRE2 abandoned the pattern in
those files, so results may be materially incomplete. A result or match cap is benign.

### jsonl

Every result is one line carrying `"type":"result"` with its fields at the top level,
followed by one `{"type":"summary", ...}` line:

```sh
bbs 'TODO' --format jsonl | jq -r 'select(.type=="result") | "\(.repository)/\(.path):\(.lines[0].number)"'
bbs 'TODO' --format jsonl | jq 'select(.type=="summary") | .truncation'
```

The summary carries `total_results`, `results_shown`, `files_searched`,
`skipped_files`, `skipped`, `truncation`, `cached`, `offline`, `elapsed_ms`, `sync_ms`,
and `scan_ms`.

## Freshness

Every search syncs the selected snapshots first, then scans.

```sh
bbs 'query' --max-age 5m    # reuse anything fetched in the last five minutes
bbs 'query' --offline       # skip the network, use last cached commits
bbs 'query' --no-cache      # rescan even if results are cached
```

`--max-age` takes `30s`, `5m`, `1h30m`, `2d`, or a bare number of seconds, and covers
repository discovery as well as the snapshots — that is what makes a repeat query on a
warm cache near-instant. A snapshot reused inside the window is **not** labelled stale.
Unlike `--offline`, a repository that was never synced is still fetched.

Offline results are labelled stale and report the cached commit. Results cache on exact
commit SHAs, so a fetch invalidates them automatically.

## Authentication

```sh
bbs auth status                 # exit 0 present, 1 absent; add --json or --verify
bbs login                       # interactive prompt, no echo
echo "$TOKEN" | bbs login --token-stdin
BB_TOKEN=... bbs 'query'        # used only if there is no saved credential
bbs --env-token 'query'         # use BB_TOKEN even though one is saved
```

Token scopes: `read:workspace:bitbucket`, `read:repository:bitbucket`. Mint one at
https://id.atlassian.com/manage-profile/security/api-tokens.

`bbs` prefers the credential saved by `bbs login`. `BB_TOKEN` is a fallback, not a
rival: it is what an account that has never logged in searches with, and what a saved
credential falls through to once Bitbucket answers 401 — so an expired token is a
warning on stderr rather than a failed run.

## Cache

```sh
bbs cache status              # sizes and entry counts as JSON
bbs cache status --verbose    # every snapshot: repository, branch, commit, age, size
bbs cache prune               # trim to the configured budgets
bbs cache clear-results       # drop cached results, keep snapshots
bbs cache forget team/api     # drop one repository's snapshots, all branches
```

## Limits

- Only tracked UTF-8 text files are searched. Binary files, submodule contents, and Git
  LFS payloads are skipped. A file counts as binary if a NUL byte appears in its first
  8 KiB.
- Files above `max_file_bytes` (default 4 MiB) are skipped.
- Every skipped file is counted by reason and reported in `skipped_files` and in the
  summary line, so a file the scan walked past is never silently absent.
- At most 20,000 matches per term per file; beyond that the response is marked
  truncated, and `truncation` says which cause applied.
- One branch per repository per search. No history search.
- A repository that cannot contribute is skipped, not fatal: no commits yet, no such
  branch, or (offline) no cached snapshot. Each is named on stderr and listed in
  `skipped` in JSON output. The search fails only when nothing at all could be searched.

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
bbs '"apiVersion:"' --path '**/*.y*ml' --sort repo

# a definition and its call site in the same file
bbs '"fn parse_query" AND "parse_query("'

# every TODO with an owner, machine-readable
bbs -r 'TODO\([a-z.]+\)' --format jsonl

# audit one directory of one repository
bbs 'auth' --repos team/api --path 'src/server/**'

# fastest repeat query, still verified within five minutes
bbs 'query' --max-age 5m

# fastest repeat query, no network at all
bbs 'query' --offline
```
