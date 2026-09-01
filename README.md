# Better Bitbucket Search

![The bbs browser interface running the query checksum AND /sha-?256/ AND (verify OR sha256sum OR Get-FileHash), with matches highlighted across a shell script and a Markdown file, two results collapsed](.github/assets/screenshot.png)

`bbs` is a fast, local-first code search experience for Bitbucket Cloud. It discovers every repository your scoped API token can read, keeps depth-1 snapshots fresh, and searches them with Boolean expressions, wildcards, PCRE2 regular expressions, path globs, relevance ranking, and syntax-highlighted context. Search from your terminal, or run `bbs serve` for a local browser UI with the same engine behind it.

## Install

Release binaries contain the CLI, Git implementation, search engine, and browser UI. Node, Rust, and system Git are not required.

```sh
curl -fsSL https://tools.klar.ws/bbs/install.sh | sh
```

PowerShell:

```powershell
irm https://tools.klar.ws/bbs/install.ps1 | iex
```

The scripts install to a user-local directory and verify release checksums. Set `BBS_REPOSITORY`, `BBS_VERSION`, or `BBS_INSTALL_DIR` to override their defaults. Direct archives and checksum manifests are also available from GitHub Releases.

## Update

```sh
bbs update
```

This downloads the latest release for your platform, verifies its SHA-256 against the published `checksums.txt`, and replaces the running binary in place. Nothing is written until the checksum matches.

`bbs update --check` reports whether a newer release exists without installing it, exiting `0` when already current and `1` when an update is available, so it composes in a shell prompt or a cron job.

Every command checks whether a newer release exists before it runs, and prints a short notice to standard error when one does — standard out stays exactly what `--format json` expects, so nothing downstream has to filter it out. That check is throttled to once every five minutes, with the result cached, so it stays well inside GitHub's anonymous rate limit and off the critical path of most commands. `bbs serve` runs the same check on the same five-minute schedule and shows a one-line, dismissible banner in the browser instead of a stderr notice.

If you would rather not see the notice at all, let `bbs` install updates for you:

```sh
bbs auto-update on
```

With that on, a command that finds an update installs it and carries on running on the new version, so the notice turns into an upgrade instead of a nag. `bbs serve` is deliberately left out: it is the one command people leave running unattended, and having the binary underneath it swapped out would be a surprise.

If the binary lives somewhere you cannot write, the command names that path and stops. It never escalates privileges and never installs a second copy elsewhere on your `PATH`.

## Authenticate

Create an Atlassian API token [here](https://id.atlassian.com/manage-profile/security/api-tokens), scoped to Bitbucket with:

- `read:workspace:bitbucket`
- `read:repository:bitbucket`

Then run:

```sh
bbs login
```

The token is validated and saved in macOS Keychain, Windows Credential Manager, or Linux Secret Service. It is never written to Git remotes or the filesystem. For development and CI, set `BB_TOKEN` instead: it is used when nothing is saved, and as a fallback when the saved credential has expired. Pass `--env-token` to prefer it outright.

## Search in your browser

```sh
bbs serve
```

That is the entire setup. `bbs serve` opens `http://localhost:7337` with the full search engine already behind it — the interface is embedded in the binary, so there is no separate service to start, nothing to configure, and nowhere else to authenticate.

- Repository picker with instant filtering, plus path, branch, case-mode, sort, raw-regex, and offline controls
- Live synchronization progress, then results streamed to the page
- Shiki syntax highlighting with every match marked in place, in context
- The Boolean grammar reference sits right under the query box
- Cache-hit, stale, and offline badges, so you always know how fresh a result is
- Permalinks to the exact commit and line in Bitbucket
- `⌘ K` / `Ctrl K` jumps back to the query box, and a running search can be cancelled

It drives the exact same engine as the CLI, so a query returns identical results either way. The server binds only to `127.0.0.1`, validates local Host/Origin headers, and protects mutations with a per-process CSRF token. Use `--port` to select another fixed port or `--no-open` to suppress the browser launch.

## Search from the terminal

```sh
bbs "myClassName"
bbs "myClassName" --repos api web
bbs "myClassName" --repos team/api --path "src/server/*.js"
bbs "myVarWith*" --path "src/**/*.ts"
bbs -r '\bmyVar\.\d[A-Za-z0-9_$]*[a-z]\b'
bbs 'foo AND (bar OR baz) AND NOT generated'
bbs '"exact phrase" AND /class\s+\w+/' --branch release/2.x
```

Quote path patterns so your shell does not expand them locally.

To find out what you can search over in the first place:

```sh
bbs list repos                       # every accessible repository
bbs list repos --filter 'edge-*'     # substring, glob, or /regex/
bbs list repos -r '^team/(api|web)'  # raw PCRE2, no slashes needed
```

The filter reads like a query term: a plain substring, a `*`/`?` glob, or a `/regex/` with the
usual `icsmx` flags. It is matched against each repository's slug, `workspace/slug`, and display
name. `bbs repos` is shorthand for the same command.

Query rules:

- `NOT` binds before `AND`, which binds before `OR`. Operators are uppercase only, so `and`/`or`/`not` stay searchable words.
- Boolean expressions are evaluated at file level.
- Multiple positional query expressions are ORed and deduplicated.
- Bare and quoted terms are literal, with `*` and `?` wildcards.
- `/.../` is a PCRE2 atom inside a Boolean expression; `-r` treats each complete query as raw PCRE2.
- `/.../` accepts trailing `i`, `s`, `m`, and `x` flags, so `/foo.*bar/s` spans lines.
- Wildcards stay on one line, and `.` in a regex excludes newlines. `-M`/`--multiline` lets both cross line breaks, matching lazily so a wildcard stops at the nearest hit rather than the last one in the file.
- To find two things in one file without caring where they sit, prefer `foo AND bar`: Boolean expressions are evaluated per file, so the terms may be lines apart.
- Matching is smart-case by default; use `-i` or `-s` to override it.
- A query needs at least one positive term: `NOT x` alone is refused, `foo AND NOT x` is fine.
- Repositories with no commits, or without the requested branch, are skipped with a warning rather than failing the search.
- `-w`/`--word` requires a word boundary either side of every term, without the case surprises of writing `/\bfoo\b/` by hand.
- A pattern that matches at every position -- `""`, `//`, `*`, `?` -- is refused rather than run.
- `--path` accepts Git-style `*`, `?`, character classes, and recursive `**` globs. A pattern with no `/` matches at any depth; `./x` anchors it to the root.
- `--exclude-path`, a leading `!` in `--path`, and `--no-vendor` remove paths; `NOT` only ever excluded on content.
- `--sort`, `--max-results`, `--context`, `-l` and `--count` change only what is shown, so they never force a rescan.
- Normal searches synchronize every selected snapshot before scanning. `--max-age 5m` reuses anything fetched recently; `--offline` explicitly uses the last cached commits.

At most 20,000 matches per pattern per file are collected; beyond that the result is reported as truncated rather than failing.

See [docs/usage.md](docs/usage.md) for the full reference: query grammar, scoping, output, ranking, freshness, cache, and configuration.

Useful automation options include `--format json`, `--format jsonl`, `--sort`, `--max-results`, `--context`, and `--no-cache`. Exit status is `0` for matches, `1` for no matches, and `2` for errors.

## Warm up first

```sh
bbs warmup
```

The first search on a large workspace pays for discovering every repository and cloning it. `bbs warmup` frontloads that, so the next query starts at the scan instead. Scope it with `--repos` and `--branch` exactly as a search, and add `--max-age 6h` to make a repeated or scheduled run refetch only what has gone stale. It writes the same snapshots a search reads, under the same lock, so the two can never disagree about what is cached.

## Cache and privacy

Managed depth-1 snapshots and compressed result entries use platform-standard cache directories. Result keys include the normalized query, options, repository UUIDs, branches, and exact commit SHAs, so a fetch automatically invalidates stale results.

```sh
bbs cache status
bbs cache prune
bbs cache clear-results
bbs list repos --offline
```

`prune` leaves the next search cold; `bbs warmup` puts it back.

Only tracked UTF-8 text files are searched. Binary files, Git metadata, submodule contents, and hydrated Git LFS payloads are excluded in v1.

## Development

Requirements are current stable Rust and Node.js 22 or newer.

```sh
npm --prefix web ci
npm --prefix web run build
cargo test
cargo run -- "query" --offline
```

The live integration harness can use `BB_TOKEN`; local `.env` files are ignored and no test prints credentials. See [docs/architecture.md](docs/architecture.md) for the data flow and trust boundaries.

## License

MIT
