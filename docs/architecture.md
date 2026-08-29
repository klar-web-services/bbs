# Architecture

## Search flow

1. Resolve `BB_TOKEN` or the OS credential-store entry.
2. Page through `/2.0/user/workspaces` and `/2.0/repositories/{workspace}?role=member`.
3. Resolve short repository names uniquely and determine the default or requested branch.
4. Under the cross-process search lock, clone missing snapshots or depth-1 fetch existing ones through libgit2 credential callbacks.
5. Record exact commit SHAs, compile the Boolean AST and PCRE2 atoms, and derive the filesystem-cache key.
6. On a cache miss, walk tracked checkout files, apply path filters and size/binary filters, evaluate expressions at file level, collect context, rank, and store the scan.
7. Narrow the stored scan to the requested sort, limit and context, then render it as ANSI terminal output, JSON/JSONL, or versioned loopback API events.

Normal searches are all-or-nothing: a failed discovery or fetch returns no potentially stale results. Offline mode loads the cached catalog and snapshots, marks every result stale, and includes the cached commit. `--max-age` sits between the two: a snapshot fetched inside the window is reused without contacting the remote, and is not labelled stale, because the freshness the caller asked for was met. The window covers repository discovery as well, which otherwise dominates back-to-back query latency on a large workspace.

## Scan and presentation

Step 6 and step 7 are separated deliberately, and the cache key covers only the first.

- **Scan**: the query fingerprint, the path filters, the file-size cap, and the exact commit of every snapshot. Changing any of it changes what could be found.
- **Presentation**: `--sort`, `--max-results`, `--context`, `-l` and `--count`. None of them changes what is found, only what is shown, so keying on them meant re-scanning the whole corpus to render an identical answer in a different order.

A scan is stored at least `cache_context_lines` and `cache_max_results` wide. A narrower request is served by re-sorting, truncating and trimming context in memory: `ResultLine` carries absolute line numbers and an `is_context` flag, so context can be narrowed without reopening a file. A wider request rescans and re-stores at the larger size.

A scan that held every match records `complete: true` and can be re-displayed in any sort order. One that stopped at the stored limit holds the top N in one particular order, so serving it in a different order would select different rows -- a wrong answer, not a stale one -- and it is reused only in the order it was stored in.

## Path filtering

`--path` and `--exclude-path` normalise before they match. A pattern containing no `/` is prefixed with `**/`, so it matches at any depth the way ripgrep's `--glob` does; a leading `./` or `/` anchors it to the root instead; a trailing `/` expands to `/**`; and any pattern not already ending in `/**` gains a `<pattern>/**` companion, because only files are ever tested and a bare directory name would otherwise match nothing. A leading `!` routes a pattern to the exclusion set. A file is selected when it matches some include pattern (or there are none) and no exclude pattern.

The walk counts what each half of the filter removed, so a filter that eliminates every candidate is reported rather than reading as an empty result set.

## Matching semantics

Atoms run against whole file bytes, not line by line, so a pattern may span line breaks when it is allowed to. What differs is the default:

- Wildcards compile to `[^\r\n]*` and `[^\r\n]`, confining a term to one line.
- `/.../` atoms compile with PCRE2 defaults, so `.` excludes newlines, and accept trailing `i`, `s`, `m`, and `x` flags. A following `AND`, `OR`, or `NOT` is not read as flags.
- Multiline mode compiles wildcards as lazy `[\s\S]*?` and enables dotall for regex atoms. Laziness is load-bearing: a greedy cross-line wildcard would run from the first hit to the last one in the file.
- Boolean operators are evaluated per file regardless of the mode, so terms on different lines already satisfy `foo AND bar` without it.

Because the mode changes what a pattern matches, it is part of the query fingerprint and therefore of the result-cache key.

Smart case is decided on the characters of a pattern that stand for themselves. Escapes, `\p{...}` property names and `(?...)` group prefixes are skipped, because reading them as evidence of intent made `/todo\D/` case-sensitive and therefore empty. `\Q...\E` is read, being the one construct where a backslash introduces literal text. In a wildcard term a backslash means "the next character literally", so escapes there do count.

A pattern that matches the empty string, or one built only from `*` and `?`, matches at every byte offset. It is refused at compile time rather than run: it would hit the per-file cap in every file and report the whole corpus as truncated. The check runs before `--word` wraps an atom in boundaries, which would otherwise mask it.

Match collection is bounded rather than fallible. At most 20,000 spans per atom per file are kept, and a PCRE2 runtime limit on a pathological pattern stops that atom's scan. Neither aborts the search. They are reported separately, because a cap is benign and an abandoned pattern means the results may be materially wrong, and the same distinction applies to results dropped by `--max-results`: `truncation` carries all three with the file counts behind them.

## Trust boundaries

- API tokens come only from the process environment, an interactive no-echo prompt, or an OS credential store.
- Tokens are passed to REST as bearer credentials and to libgit2 through credential callbacks. Clone URLs, cache files, logs, and result objects contain no secrets.
- Repository UUIDs and branch names are hashed for cache paths. Branch names are validated as Git refs before use.
- Destructive checkout/cache operations validate that the target is beneath the application cache root.
- One cross-process lock protects snapshot consistency from synchronization through scan completion.
- The browser service binds to IPv4 loopback, rejects non-local Host/Origin headers, limits request bodies, and requires a random CSRF token for POST requests.
- `bbs update` replaces the running executable, so it is the one path that writes outside the cache root. It fetches the release archive and `checksums.txt` over HTTPS, compares the archive's SHA-256 against the entry whose filename matches exactly, and only then extracts. The replacement is written to a temporary file in the target's own directory and renamed, so a failure leaves the original binary in place and never a partial executable. It never escalates privileges: an unwritable directory is reported by path rather than retried with `sudo` or redirected to another directory, which would shadow the original binary on `PATH`. Integrity rests on the published checksum and HTTPS alone; releases are not signed.

## Cache layout

```text
cache/
  catalog.json
  search.lock
  snapshots/<repository-uuid-hash>/<branch-hash>/            the checkout
  snapshots/<repository-uuid-hash>/<branch-hash>.meta.json   what it is
  snapshots/<repository-uuid-hash>/<branch-hash>.used        last used
  results/<sha256-key>.json.zst
```

Each snapshot is described by a sibling metadata file holding its repository record, branch, commit and fetch time. It is a sibling rather than a file inside the checkout for two reasons: the scanner would otherwise return it as a search result, and the `remove_untracked` checkout on the next fetch would delete it. It makes the cache legible to `cache status --verbose`, answers `--max-age` without opening the repository, and lets an offline search rebuild a lost `catalog.json` -- which is derived data, so its corruption reports how to rebuild rather than being fatal.

Repository snapshots and result entries have separate configurable budgets. Result writes and catalog writes use temporary files followed by atomic persistence. Corrupt result entries are discarded as misses.

## Current boundaries

- Bitbucket Cloud only.
- One branch per repository per request; no Git history search.
- Up to roughly 100 repositories is the initial performance target.
- UTF-8 tracked files only; no submodule recursion or Git LFS hydration.
- No persistent content index. The scanner interface can later be replaced with a trigram candidate index without changing CLI or API result types.
- Updates verify a published SHA-256 but no signature, and resolve the newest release through the unauthenticated GitHub API, which is rate limited to 60 requests per hour per address.

