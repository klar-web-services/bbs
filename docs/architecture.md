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

## Trust boundaries

- API tokens come only from the process environment, an interactive no-echo prompt, or an OS credential store.
- Tokens are passed to REST as bearer credentials and to libgit2 through credential callbacks. Clone URLs, cache files, logs, and result objects contain no secrets.
- Repository UUIDs and branch names are hashed for cache paths. Branch names are validated as Git refs before use.
- Destructive checkout/cache operations validate that the target is beneath the application cache root.
- One cross-process lock protects snapshot consistency from synchronization through scan completion.
- The browser service binds to IPv4 loopback, rejects non-local Host/Origin headers, limits request bodies, and requires a random CSRF token for POST requests.

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

