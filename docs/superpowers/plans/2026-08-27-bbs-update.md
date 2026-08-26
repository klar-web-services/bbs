# `bbs update` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `bbs update [--check]` command that upgrades the running binary from GitHub Releases, verifying SHA-256 before replacing anything.

**Architecture:** One new module, `src/update.rs`, split into pure logic (version parsing, asset mapping, checksum lookup, archive extraction, binary replacement) and two thin networked functions that take a base URL so tests can serve fixtures locally. `main.rs` gains a `Command::Update` arm mapping outcomes onto the existing 0/1/2 exit codes.

**Tech Stack:** Rust 2024, clap derive, reqwest (rustls), sha2, tempfile, flate2 + tar (unix), zip (windows), axum for the offline fixture server in tests.

Spec: `docs/superpowers/specs/2026-08-27-bbs-update-command-design.md`

---

### Task 1: Dependencies

**Files:** Modify `Cargo.toml`

- [ ] Add target-gated deps so neither platform pays for the other:
      `cargo add flate2 tar --target 'cfg(unix)'`
      `cargo add zip --target 'cfg(windows)' --no-default-features --features deflate`
- [ ] `cargo check` passes.
- [ ] Commit.

### Task 2: Version parsing and comparison

**Files:** Create `src/update.rs`; modify `src/lib.rs`

- [ ] Write failing tests: `Version::parse` on `"0.1.0"`, `"v0.1.0"`, ordering
      (newer/older/equal), and errors on `"0.1"`, `"0.1.0-rc1"`, `"abc"`.
- [ ] Run `cargo test update::` — expect failure to compile.
- [ ] Implement `Version(u64,u64,u64)` deriving `PartialOrd`/`Ord`, plus
      `parse()` requiring exactly three numeric components.
- [ ] Tests pass. Commit.

### Task 3: Asset name for the running target

**Files:** Modify `src/update.rs`

- [ ] Add `ASSET` and `BINARY` consts behind `cfg(target_os, target_arch)` for
      the five published triples, with `compile_error!` for anything else.
- [ ] Test asserts `ASSET` ends with `.tar.gz` on unix / `.zip` on windows and
      contains the arch.
- [ ] Tests pass. Commit.

### Task 4: checksums.txt lookup

**Files:** Modify `src/update.rs`

- [ ] Write failing tests: exact filename match; ignores a line whose name is a
      prefix of the wanted asset; ignores a name differing by one character;
      tolerates the `*` binary-mode prefix; errors when absent.
- [ ] Implement `expected_digest(checksums, asset)` splitting on whitespace and
      comparing the filename field for equality.
- [ ] Implement `verify(bytes, expected)` using `sha2` + `hex`.
- [ ] Tests pass. Commit.

### Task 5: Archive extraction

**Files:** Modify `src/update.rs`

- [ ] Write a failing test that builds a `.tar.gz` in memory containing a `bbs`
      member and asserts `extract()` returns its bytes; plus one asserting an
      archive without a `bbs` member errors.
- [ ] Implement `extract()` for unix (flate2 + tar) and windows (zip).
- [ ] Tests pass. Commit.

### Task 6: Atomic replacement and rollback

**Files:** Modify `src/update.rs`

- [ ] Write failing tests against a temp dir holding a dummy binary: successful
      swap replaces contents and keeps mode 0755; a non-writable directory
      fails with the path named; on windows the `.old` backup is cleaned up.
- [ ] Implement `replace(target, binary)` writing a temp file in the target's
      own directory, then renaming. Windows renames the target aside first and
      restores it if the move fails.
- [ ] Tests pass. Commit.

### Task 7: Networked resolve and download

**Files:** Modify `src/update.rs`

- [ ] Implement `repository()` honouring `BBS_REPOSITORY`.
- [ ] Implement `latest_version(http, api_base, repo)` and
      `download(http, download_base, repo, version)`, both taking a base URL,
      sending the `bbs/{version}` user agent GitHub requires, and reporting
      403/rate-limit distinctly.
- [ ] Write an integration test that serves fixture bytes from a local axum
      server and drives download → verify → extract → replace end to end.
- [ ] Tests pass. Commit.

### Task 8: CLI wiring

**Files:** Modify `src/cli.rs`, `src/main.rs`

- [ ] Add `Command::Update(UpdateArgs)` with `--check`.
- [ ] Add the `main.rs` arm: `--check` returns 0 current / 1 available; a real
      update downloads, verifies, replaces, prints the old and new version.
- [ ] Test that clap parses `bbs update` and `bbs update --check`.
- [ ] Tests pass. Commit.

### Task 9: Docs and full verification

**Files:** Modify `README.md`

- [ ] Document `bbs update` and its exit codes.
- [ ] Run the full CI set: `cargo fmt --all -- --check`,
      `cargo clippy --all-targets --all-features -- -D warnings`,
      `cargo test --all-features`.
- [ ] Commit and push.
