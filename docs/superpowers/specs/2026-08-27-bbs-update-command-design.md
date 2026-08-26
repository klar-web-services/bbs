# `bbs update` design

Date: 2026-08-27
Status: approved, not yet implemented

## Goal

Let an installed `bbs` upgrade itself from GitHub Releases without curl, sh,
PowerShell, or the install scripts. The command consumes exactly the artifacts
the release workflow already publishes: the per-target archive and
`checksums.txt`.

## Command surface

```
bbs update [--check]
```

`bbs update` upgrades to the latest release, or reports that the current build
is already newest. `--check` resolves and reports the same information without
downloading or writing anything.

Exit codes follow the existing 0/1/2 convention in `main.rs`:

| Code | `bbs update` | `bbs update --check` |
| --- | --- | --- |
| 0 | updated, or already current | already current |
| 1 | (not used) | an update is available |
| 2 | error | error |

The split lets `--check` drive a shell prompt or a cron job, and keeps the
version-comparison path testable without ever writing a binary.

## Module: `src/update.rs`

Three seams, so each is testable in isolation and only one touches the network.

### `resolve()`

GETs `https://api.github.com/repos/{repo}/releases/latest`, reads `tag_name`,
strips a leading `v`, and compares against `env!("CARGO_PKG_VERSION")`.

The repository defaults to `klar-web-services/bbs` and is overridable with
`BBS_REPOSITORY`, matching `scripts/install.sh` and `scripts/install.ps1`.
Requests reuse the existing `reqwest` rustls client and must send the
`bbs/{version}` user agent, which the GitHub API requires.

Versions parse as three numeric components. Anything else — a prerelease
suffix, a non-numeric tag — is an error rather than a silent no-op, so a
malformed release is loud instead of stranding users on an old build.

The API is unauthenticated and limited to 60 requests per hour per IP, and can
return a transient 403. Both cases report a clear message naming the limit
rather than a bare HTTP error.

### `fetch()`

Downloads the target archive and `checksums.txt` from
`https://github.com/{repo}/releases/download/v{version}/`, then verifies the
archive's SHA-256 with the existing `sha2` dependency.

`checksums.txt` is parsed by splitting each line on whitespace and comparing
the filename field for **exact equality**. This is deliberately stricter than
the regex in `install.sh`, where `.` in the asset name matches any character
and a near-miss filename could in principle match the wrong line.

The asset name is derived at compile time from `cfg!(target_os)` and
`cfg!(target_arch)`, mirroring the `uname` mapping in `install.sh`:

| target_os | target_arch | asset |
| --- | --- | --- |
| linux | x86_64 | `bbs-x86_64-unknown-linux-gnu.tar.gz` |
| linux | aarch64 | `bbs-aarch64-unknown-linux-gnu.tar.gz` |
| macos | x86_64 | `bbs-x86_64-apple-darwin.tar.gz` |
| macos | aarch64 | `bbs-aarch64-apple-darwin.tar.gz` |
| windows | x86_64 | `bbs-x86_64-pc-windows-msvc.zip` |

Any other combination fails at compile time, so a target that has no published
asset cannot ship a broken `update` command.

### `replace()`

Takes archive bytes and a target path. Extracts the single `bbs` (or `bbs.exe`)
member to a temporary file **in the same directory as the target**, so the
final rename stays on one filesystem and is therefore atomic. Sets mode `0755`
on Unix.

- **Unix**: rename the temp file over the target. This succeeds while the
  binary is running, because the running process holds the old inode.
- **Windows**: a running `.exe` cannot be overwritten, but it can be renamed.
  Move the target aside to `bbs.exe.old`, move the new binary into place, then
  delete the stale file on a best-effort basis. A leftover `.old` is removed on
  the next run.

Any failure restores the original and leaves no partial executable. The target
is never written in place.

## Permissions

If the directory holding the binary is not writable — a root-owned
`/usr/local/bin`, a Homebrew copy, a `cargo install` copy — the command fails
with a message naming the exact path.

It never escalates with `sudo` and never falls back to installing into a
different directory, because a second `bbs` earlier on `PATH` would silently
shadow the first and make the version reported by `bbs --version` depend on
shell configuration.

## Dependencies

Extraction requires crates the project does not currently carry. They are
target-gated so neither platform pays for the other:

- Unix: `flate2` (pure-Rust backend) and `tar`
- Windows: `zip`, default features off, `deflate` only

Shelling out to `tar` or `Expand-Archive` was rejected: the binary presently
requires nothing external at runtime, and that property is worth keeping.

## Testing

No test touches the network.

- Version comparison: newer, older, equal, malformed, leading `v`.
- Asset-name mapping for each supported target triple.
- `checksums.txt` parsing, including a line whose filename is a prefix of, or
  differs by one character from, the wanted asset.
- `replace()` against a temp directory holding a dummy file: verifies the swap
  lands, and that an induced failure rolls back with the original intact.

`resolve()` and `fetch()` take a base URL so a test can point them at a local
fixture directory instead of GitHub.

## Non-goals

- No `--version` pinning or downgrade. Reconsider if a bad release ships.
- No update check on startup. `bbs` stays silent unless asked.
- No signature or provenance verification beyond the published SHA-256.
- No detection of how the binary was installed. The permission check is the
  only guard, and its error message covers the package-manager cases.
