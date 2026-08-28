# Architecture

## Search flow

1. Resolve `BB_TOKEN` or the OS credential-store entry.
2. Page through `/2.0/user/workspaces` and `/2.0/repositories/{workspace}?role=member`.
3. Resolve short repository names uniquely and determine the default or requested branch.
4. Under the cross-process search lock, clone missing snapshots or depth-1 fetch existing ones through libgit2 credential callbacks.
5. Record exact commit SHAs, compile the Boolean AST and PCRE2 atoms, and derive the filesystem-cache key.
6. On a cache miss, walk tracked checkout files, apply path globs and size/binary filters, evaluate expressions at file level, collect context, rank, and cache canonical JSON.
7. Render the same canonical results as ANSI terminal output, JSON/JSONL, or versioned loopback API events.

Normal searches are all-or-nothing: a failed discovery or fetch returns no potentially stale results. Offline mode loads the cached catalog and snapshots, marks every result stale, and includes the cached commit.

## Matching semantics

Atoms run against whole file bytes, not line by line, so a pattern may span line breaks when it is allowed to. What differs is the default:

- Wildcards compile to `[^\r\n]*` and `[^\r\n]`, confining a term to one line.
- `/.../` atoms compile with PCRE2 defaults, so `.` excludes newlines, and accept trailing `i`, `s`, `m`, and `x` flags. A following `AND`, `OR`, or `NOT` is not read as flags.
- Multiline mode compiles wildcards as lazy `[\s\S]*?` and enables dotall for regex atoms. Laziness is load-bearing: a greedy cross-line wildcard would run from the first hit to the last one in the file.
- Boolean operators are evaluated per file regardless of the mode, so terms on different lines already satisfy `foo AND bar` without it.

Because the mode changes what a pattern matches, it is part of the query fingerprint and therefore of the result-cache key.

Match collection is bounded rather than fallible. At most 20,000 spans per atom per file are kept, and a PCRE2 runtime limit on a pathological pattern stops that atom's scan. Neither aborts the search; both mark the response truncated, so a heavy query degrades to partial results instead of an error.

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
  snapshots/<repository-uuid-hash>/<branch-hash>/
  results/<sha256-key>.json.zst
```

Repository snapshots and result entries have separate configurable budgets. Result writes and catalog writes use temporary files followed by atomic persistence. Corrupt result entries are discarded as misses.

## Current boundaries

- Bitbucket Cloud only.
- One branch per repository per request; no Git history search.
- Up to roughly 100 repositories is the initial performance target.
- UTF-8 tracked files only; no submodule recursion or Git LFS hydration.
- No persistent content index. The scanner interface can later be replaced with a trigram candidate index without changing CLI or API result types.
- Updates verify a published SHA-256 but no signature, and resolve the newest release through the unauthenticated GitHub API, which is rate limited to 60 requests per hour per address.

