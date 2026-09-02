---
name: bbs
description: Search code across every Bitbucket Cloud repository an account can read, with Boolean queries, regex, and path globs, using the local `bbs` CLI. Use when the user asks to find, grep, audit, or count something across Bitbucket repositories, an organisation, a workspace, "all our repos", "the other services", or any code that is not in the current checkout - and when they ask which Bitbucket repositories exist. Not for searching the local working tree; use ordinary grep for that.
license: MIT
compatibility: Requires the `bbs` CLI on PATH. Network access to api.bitbucket.org unless run with --offline.
allowed-tools: Bash(bbs:*) Bash(command -v bbs:*)
metadata:
  homepage: https://github.com/klar-web-services/bbs
  source: better-bitbucket-search
---

# Searching Bitbucket Cloud with `bbs`

`bbs` searches whole Bitbucket Cloud organisations from one command. It keeps local
depth-1 snapshots of every repository the token can read and greps them, so a query
crosses hundreds of repositories in seconds and returns file paths, line numbers, and
context that you can act on directly.

## 1. Gate: is `bbs` usable?

Run this **first**, every time, before promising the user anything:

```sh
command -v bbs >/dev/null && bbs auth status
```

`bbs auth status` is local-only and instant — it never touches the network. Read the
exit code:

| Result | Meaning | What to do |
| --- | --- | --- |
| `0` | A credential is present | Go to section 2 and answer the request |
| `1` | No credential saved and `BB_TOKEN` unset | Follow the gate rule below |
| `2` | `bbs` failed (e.g. locked credential store) | Report the message verbatim |
| `bbs` not on PATH | Not installed | Follow the gate rule below |

### The gate rule

**Decide how you got here.**

- **Manual** — the user typed `/bbs`, named this skill, named `bbs`, or explicitly
  asked to search Bitbucket.
- **Automatic** — you loaded this skill yourself because a task looked related. The
  user never mentioned `bbs` or Bitbucket search.

Then:

- **Automatic and unusable → stop silently.** Do not mention `bbs`, this skill, or
  installation. Do not ask the user to set anything up. Return to the original task
  and use your ordinary tools. An unconfigured tool is not the user's problem right
  now, and interrupting a discovery sweep to advertise a CLI is noise.
- **Manual and unusable → explain the one thing that is missing**, in section 5's
  words, then stop. Do not attempt a search, and do not run `bbs login` yourself.
- **Usable → say nothing about setup at all.** Just do what the user asked. Do not
  narrate the auth check.

## 2. Answer the request

Work in this order.

**a. Know the corpus.** If the user named repositories, or you need to check what
exists, list them — this is cached and cheap:

```sh
bbs list repos --offline          # last discovered catalog, no network
bbs list repos --filter 'edge-*'  # substring, glob, or /regex/
```

`--offline` fails if nothing has ever been discovered; drop it to fetch the catalog.

**b. Scope before you search.** An unscoped query fetches and scans every accessible
repository. That is the tool's whole point, and on a cold cache it can take minutes.
Narrow whenever the request allows it:

```sh
bbs 'PaymentIntent' --repos team/api            # one repository
bbs 'PaymentIntent' --repos 'edge-*'            # a family
bbs 'PaymentIntent' --path 'src/**/*.ts'        # a subtree or file type
bbs 'PaymentIntent' --no-vendor                 # drop vendor/generated/dist/build/node_modules
```

**c. Survey wide, then read narrow.** For "where is X used?", get the file list first
and only pull context for what matters:

```sh
bbs 'PaymentIntent' -l                          # repository + path only
bbs 'PaymentIntent' --count --sort repo         # matches per file, grouped
bbs 'PaymentIntent' --repos team/api --context 6
```

**d. Parse with `--format jsonl`** when you are going to post-process. Every result is
one line with `"type":"result"`, followed by one `{"type":"summary", ...}` line:

```sh
bbs 'TODO' --format jsonl | jq -r 'select(.type=="result") | "\(.repository)/\(.path):\(.lines[0].number)"'
```

**e. Report what you found**, with `repository/path:line`. `bbs` prints a summary line
naming what the numbers hide; pass it on when it matters — especially
`stopped early: pattern too expensive`, which means results are genuinely incomplete.

## 3. Query language, in brief

Full grammar: [references/query.md](references/query.md). Read it when a query needs
more than the table below, or when one is rejected and you do not know why.

| Form | Means |
| --- | --- |
| `foo` | Literal term, `*` and `?` wildcards |
| `"foo bar"` | Literal phrase |
| `/re/` `/re/isxm` | PCRE2 regex atom |
| `a AND b` | Both, anywhere in the same file |
| `a OR b`, `NOT a`, `(…)` | Or, not, grouping |

Five rules that account for nearly every rejected query:

1. **No implicit AND.** `foo bar` is an error. Write `foo AND bar`, or `"foo bar"` for
   a phrase.
2. **Operators are uppercase only.** `and`/`or`/`not` stay ordinary search terms.
3. **A query needs something to find.** `NOT x` alone is refused; `foo AND NOT x` is
   fine.
4. **Smart case, per term.** A term with an uppercase letter is case-sensitive; an
   all-lowercase one is not. `-i`/`-s` force the whole query, `-w` demands word
   boundaries.
5. **Quote everything.** Single-quote every query and glob so the shell does not
   expand `*`, `?`, `(`, or `$` before `bbs` sees it.

Prefer `a AND b` over a regex spanning lines: Boolean terms are evaluated per **file**,
so they match no matter how far apart the two things sit. Reach for `/a.*?b/s` or `-M`
only when order or adjacency actually matters.

## 4. Working efficiently

These are the differences between a fast agent loop and a slow one.

- **Reuse the cache.** A repeat search re-syncs every snapshot by default. Once you
  have synced during this session, add `--max-age 5m` to subsequent queries — it
  reuses anything fetched inside the window, covers discovery too, and makes a follow-up
  query near-instant without going stale behind your back. Use `--offline` when the
  user explicitly wants no network, or when you are only re-cutting results you already
  have.
- **Refine, do not re-fetch.** `--sort`, `--max-results`, `--context`, `-l`, and
  `--count` are display-only and are not part of the cache key. Re-asking the same
  query in a different shape is served from cache. Never pass `--no-cache` unless the
  user is debugging `bbs` itself.
- **Warn once about a cold first run.** The first search discovers every accessible
  repository and clones each one at depth 1. Say so before you start it, rather than
  going quiet for several minutes.
- **Exit code 1 means "no matches", not failure.** Only `2` is an error. In a chained
  command write `bbs 'q' || [ $? -eq 1 ]`.
- **Never run `bbs serve`.** It starts a blocking local web server and opens a browser.
  Suggest it to the user if a visual review would help; do not run it yourself.
- **Never run `bbs login`.** It prompts for a secret on a TTY you do not have, and it
  would hang. Ask the user to run it.
- **Do not paste tokens** into commands, files, or your replies. `bbs` reads them from
  the OS credential store.
- Output goes uncoloured automatically when piped, so no `--color never` is needed.
- One branch per search: `--branch release/2.x` applies to every selected repository.
  There is no history search.

## 5. When `bbs` is not ready

Use these words, only on a **manual** invocation (see section 1).

**Not installed** — `bbs` is not on PATH:

> Install it with `curl -fsSL https://tools.klar.ws/bbs/install.sh | sh` (PowerShell:
> `irm https://tools.klar.ws/bbs/install.ps1 | iex`), then run `bbs login`.

**Not authenticated** — `bbs auth status` exited 1:

> `bbs` has no Bitbucket credential yet. Create an Atlassian API token at
> https://id.atlassian.com/manage-profile/security/api-tokens with the
> `read:workspace:bitbucket` and `read:repository:bitbucket` scopes, then run
> `bbs login` and paste it. For CI, set `BB_TOKEN` instead.

Then stop and wait. Do not run `bbs login`, do not ask for the token, and do not retry
the search until the user says they have done it.

## 6. Reading failures

| Message | Meaning |
| --- | --- |
| `repository X is not accessible; did you mean Y?` | Wrong slug; retry with the suggestion or `bbs list repos --filter` |
| `warning: no file matched --path …` | The path filter, not the query, emptied the result set |
| `operators must be uppercase` | Section 3, rule 2 |
| `this query has nothing to find` | Section 3, rule 3 |
| `warning: skipped …` on stderr | One repository could not contribute (no commits, no such branch, no cached snapshot). The rest of the search still ran — mention it if it covers something the user asked about |
| `N too large (over 4.0 MiB; raise --max-file-size)` in the summary | Files were never opened, so the query could not match them. If the target could plausibly be a bundle, a generated client, or a lock file, re-run with `--max-file-size 32M` (or `none`) |
| `stopped early: pattern too expensive` | PCRE2 abandoned the pattern in some files; results are incomplete. Simplify the regex |
| 401 / rejected credential | The token expired. Tell the user to run `bbs login` again |

## Reference

- [references/query.md](references/query.md) — full query grammar, escapes, regex flags, case rules
- [references/cli.md](references/cli.md) — every command and flag, exit codes, JSON shapes, recipes
