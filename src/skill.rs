//! Installing the bundled agent skill into coding harnesses.
//!
//! `bbs` ships an Agent Skill — a `SKILL.md` plus reference files, following
//! the format at <https://agentskills.io> — that teaches a coding agent how to
//! drive the CLI. Every harness reads the same format; they differ only in
//! which directory they scan. So installing is a copy into one directory per
//! harness, and the interesting work is deciding *which* directories exist on
//! this machine and refusing to clobber a skill somebody else wrote.

use anyhow::{Context, Result, bail};
use rust_embed::RustEmbed;
use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};

/// The skill directory name, which the Agent Skills spec requires to equal the
/// `name` in the frontmatter.
pub const SKILL_NAME: &str = "bbs";

/// Written into the frontmatter of every file we ship. An existing skill
/// without it came from somewhere else, and is not ours to overwrite.
const PROVENANCE: &str = "source: better-bitbucket-search";

#[derive(RustEmbed)]
#[folder = "skills/bbs/"]
struct SkillFiles;

/// One coding agent, and the directory it scans for personal skills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Harness {
    /// Stable identifier, for `--harness`.
    pub id: &'static str,
    pub name: &'static str,
    /// Where a personal skill goes: the parent of the skill directory.
    pub skills_dir: PathBuf,
    /// Why this harness is listed, or `None` when nothing suggests it is here.
    pub detected: Option<Detection>,
    /// Shown beside the path when the directory is shared with other agents.
    pub note: Option<&'static str>,
}

impl Harness {
    /// Where this harness's copy of the skill lands.
    pub fn skill_dir(&self) -> PathBuf {
        self.skills_dir.join(SKILL_NAME)
    }

    pub fn is_available(&self) -> bool {
        self.detected.is_some()
    }
}

/// What made a harness look present. Reported so an unexpected entry in the
/// list is explainable rather than mysterious.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detection {
    /// Its executable is on `PATH`.
    Executable(String),
    /// Its configuration directory exists.
    Directory(PathBuf),
}

impl std::fmt::Display for Detection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Detection::Executable(name) => write!(f, "`{name}` on PATH"),
            Detection::Directory(path) => write!(f, "{}", path.display()),
        }
    }
}

/// What happened to one harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Installed,
    /// Ours, and the contents changed.
    Updated,
    /// Ours, and byte-identical already.
    Unchanged,
    /// A skill of this name exists and is not ours. `--force` overrides.
    Occupied,
}

impl Outcome {
    pub fn describe(&self) -> &'static str {
        match self {
            Outcome::Installed => "installed",
            Outcome::Updated => "updated",
            Outcome::Unchanged => "already up to date",
            Outcome::Occupied => "left alone: a different `bbs` skill is already there",
        }
    }
}

fn home() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .context("cannot locate the home directory")
}

/// `$XDG_CONFIG_HOME`, or `~/.config`. Deliberately *not*
/// `BaseDirs::config_dir()`: opencode and Amp use `~/.config/<name>` on macOS
/// and Windows too, where that function would answer with the platform's own
/// convention and point at a directory neither of them reads.
fn xdg_config(home: &Path) -> PathBuf {
    match env::var_os("XDG_CONFIG_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => home.join(".config"),
    }
}

fn on_path(program: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    } else {
        vec![String::new()]
    };
    env::split_paths(&paths).find_map(|dir| {
        exts.iter().find_map(|ext| {
            let candidate = dir.join(format!("{program}{ext}"));
            candidate.is_file().then_some(candidate)
        })
    })
}

/// Every harness `bbs` knows how to install into, whether or not it is here.
///
/// The paths are each harness's documented *personal* skills directory. A
/// project-level directory would be a different feature: it lands in someone's
/// repository, so it wants a commit, not a menu.
pub fn harnesses() -> Result<Vec<Harness>> {
    let home = home()?;
    let config = xdg_config(&home);
    let build = |id: &'static str,
                 name: &'static str,
                 skills_dir: PathBuf,
                 executables: &[&'static str],
                 markers: &[PathBuf],
                 note: Option<&'static str>| {
        let detected = executables
            .iter()
            .find_map(|program| on_path(program).map(|_| Detection::Executable((*program).into())))
            .or_else(|| {
                markers
                    .iter()
                    .find(|path| path.is_dir())
                    .map(|path| Detection::Directory(path.clone()))
            });
        Harness {
            id,
            name,
            skills_dir,
            detected,
            note,
        }
    };
    Ok(vec![
        build(
            "claude-code",
            "Claude Code",
            home.join(".claude/skills"),
            &["claude"],
            &[home.join(".claude")],
            None,
        ),
        build(
            "codex",
            "OpenAI Codex",
            home.join(".agents/skills"),
            &["codex"],
            &[home.join(".codex"), home.join(".agents")],
            // Codex's own personal skills directory is the cross-vendor one,
            // so this single copy is also picked up by Cursor, Gemini CLI,
            // Amp, Copilot and Droid without installing to them separately.
            Some(
                "shared Agent Skills location, also read by Cursor, Gemini CLI, Amp, Copilot and Droid",
            ),
        ),
        build(
            "cursor",
            "Cursor",
            home.join(".cursor/skills"),
            &["cursor-agent", "cursor"],
            &[home.join(".cursor")],
            None,
        ),
        build(
            "opencode",
            "opencode",
            config.join("opencode/skills"),
            &["opencode"],
            &[config.join("opencode")],
            None,
        ),
        build(
            "gemini-cli",
            "Gemini CLI",
            home.join(".gemini/skills"),
            &["gemini"],
            &[home.join(".gemini")],
            None,
        ),
        build(
            "copilot",
            "GitHub Copilot",
            home.join(".copilot/skills"),
            &["copilot"],
            &[home.join(".copilot")],
            None,
        ),
        build(
            "amp",
            "Amp",
            config.join("amp/skills"),
            &["amp"],
            &[config.join("amp")],
            None,
        ),
        build(
            "droid",
            "Factory Droid",
            home.join(".factory/skills"),
            &["droid"],
            &[home.join(".factory")],
            None,
        ),
    ])
}

/// The skill as it would be written: relative path to contents, sorted so the
/// install order is stable and `SKILL.md` is not the last thing to appear.
pub fn files() -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = SkillFiles::iter()
        .filter_map(|name| {
            let file = SkillFiles::get(&name)?;
            Some((name.to_string(), file.data.into_owned()))
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

/// The bundled `SKILL.md`, for `bbs skill --print`.
pub fn skill_markdown() -> Result<String> {
    let file = SkillFiles::get("SKILL.md").context("the bundled skill is missing SKILL.md")?;
    String::from_utf8(file.data.into_owned()).context("the bundled SKILL.md is not UTF-8")
}

/// Whether the skill already at `dir` is one we wrote, and so ours to replace.
///
/// A harness's skills directory is shared with hand-written and third-party
/// skills. Overwriting one that happens to be called `bbs` would destroy work
/// silently, so an unrecognised `SKILL.md` stops the install instead.
fn is_ours(dir: &Path) -> bool {
    match fs::read_to_string(dir.join("SKILL.md")) {
        Ok(existing) => existing.contains(PROVENANCE),
        // Nothing there, or unreadable: `install` deals with both.
        Err(_) => false,
    }
}

/// Writes the skill into `harness`, creating directories as needed.
pub fn install(harness: &Harness, force: bool) -> Result<Outcome> {
    let dir = harness.skill_dir();
    let occupied = dir.join("SKILL.md").exists();
    if occupied && !is_ours(&dir) && !force {
        return Ok(Outcome::Occupied);
    }
    let files = files();
    let mut changed = false;
    for (relative, contents) in &files {
        let target = dir.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        if fs::read(&target).is_ok_and(|current| current == *contents) {
            continue;
        }
        fs::write(&target, contents)
            .with_context(|| format!("failed to write {}", target.display()))?;
        changed = true;
    }
    Ok(match (occupied, changed) {
        (true, false) => Outcome::Unchanged,
        (true, true) => Outcome::Updated,
        (false, _) => Outcome::Installed,
    })
}

/// Resolves `--harness` names against the table, so a typo names the valid
/// identifiers rather than silently installing nothing.
pub fn select<'a>(all: &'a [Harness], ids: &[String]) -> Result<Vec<&'a Harness>> {
    let mut seen = BTreeSet::new();
    let mut selected = Vec::new();
    for id in ids {
        let harness = all
            .iter()
            .find(|harness| harness.id.eq_ignore_ascii_case(id))
            .with_context(|| {
                let known: Vec<&str> = all.iter().map(|harness| harness.id).collect();
                format!(
                    "unknown harness `{id}`; known harnesses are {}",
                    known.join(", ")
                )
            })?;
        if seen.insert(harness.id) {
            selected.push(harness);
        }
    }
    if selected.is_empty() {
        bail!("no harness selected");
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec requires the directory name and the frontmatter `name` to
    /// agree, and several harnesses derive the slash command from the
    /// directory. A mismatch is invisible until a harness rejects the skill.
    #[test]
    fn the_bundled_skill_declares_the_name_its_directory_carries() {
        let markdown = skill_markdown().unwrap();
        assert!(
            markdown.starts_with("---\n"),
            "frontmatter must open the file, or harnesses read it as body text"
        );
        assert!(
            markdown.contains(&format!("\nname: {SKILL_NAME}\n")),
            "{markdown:.200}"
        );
        assert!(markdown.contains("\ndescription: "));
    }

    /// `install` refuses to overwrite a skill it did not write, and it can
    /// only tell by this marker being present in what it ships.
    #[test]
    fn every_bundled_file_can_be_traced_back_to_bbs() {
        let files = files();
        assert!(files.iter().any(|(name, _)| name == "SKILL.md"));
        assert!(
            skill_markdown().unwrap().contains(PROVENANCE),
            "SKILL.md must carry `{PROVENANCE}` or an upgrade would refuse to replace it"
        );
    }

    /// Progressive disclosure: the body loads in full on every activation, so
    /// the spec asks for under 500 lines with the detail in `references/`.
    #[test]
    fn the_skill_body_stays_small_enough_to_load_eagerly() {
        let lines = skill_markdown().unwrap().lines().count();
        assert!(lines < 500, "SKILL.md is {lines} lines");
        assert!(
            files()
                .iter()
                .any(|(name, _)| name.starts_with("references/")),
            "the detail belongs in references/, loaded on demand"
        );
    }

    #[test]
    fn a_harness_puts_the_skill_under_its_own_name() {
        let harnesses = harnesses().unwrap();
        let claude = harnesses.iter().find(|h| h.id == "claude-code").unwrap();
        assert!(claude.skill_dir().ends_with("skills/bbs"));
        // Distinct directories, or one install would silently shadow another.
        let mut dirs: Vec<PathBuf> = harnesses.iter().map(Harness::skill_dir).collect();
        dirs.sort();
        let total = dirs.len();
        dirs.dedup();
        assert_eq!(dirs.len(), total, "two harnesses share a skills directory");
    }

    #[test]
    fn selecting_by_name_is_case_insensitive_and_deduplicated() {
        let harnesses = harnesses().unwrap();
        let selected = select(
            &harnesses,
            &["codex".into(), "Claude-Code".into(), "codex".into()],
        )
        .unwrap();
        assert_eq!(
            selected.iter().map(|h| h.id).collect::<Vec<_>>(),
            ["codex", "claude-code"]
        );
    }

    /// A mistyped harness used to be indistinguishable from one that is simply
    /// not installed: both installed nothing.
    #[test]
    fn an_unknown_harness_lists_the_ones_that_exist() {
        let harnesses = harnesses().unwrap();
        let error = select(&harnesses, &["claude".into()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("claude-code"), "{error}");
    }

    #[test]
    fn installing_writes_then_recognises_its_own_work() {
        let temp = tempfile::tempdir().unwrap();
        let harness = Harness {
            id: "test",
            name: "Test",
            skills_dir: temp.path().join("skills"),
            detected: None,
            note: None,
        };
        assert_eq!(install(&harness, false).unwrap(), Outcome::Installed);
        let skill = harness.skill_dir().join("SKILL.md");
        assert!(skill.exists());
        assert!(harness.skill_dir().join("references/query.md").exists());
        assert_eq!(install(&harness, false).unwrap(), Outcome::Unchanged);

        // An edited copy is still ours, so an upgrade restores it.
        fs::write(&skill, format!("---\n{PROVENANCE}\n---\nstale\n")).unwrap();
        assert_eq!(install(&harness, false).unwrap(), Outcome::Updated);
        assert_eq!(
            fs::read_to_string(&skill).unwrap(),
            skill_markdown().unwrap()
        );
    }

    /// Somebody else's `bbs` skill is somebody else's work.
    #[test]
    fn a_foreign_skill_of_the_same_name_survives_unless_forced() {
        let temp = tempfile::tempdir().unwrap();
        let harness = Harness {
            id: "test",
            name: "Test",
            skills_dir: temp.path().join("skills"),
            detected: None,
            note: None,
        };
        let skill = harness.skill_dir().join("SKILL.md");
        fs::create_dir_all(harness.skill_dir()).unwrap();
        fs::write(&skill, "---\nname: bbs\n---\nhand written\n").unwrap();

        assert_eq!(install(&harness, false).unwrap(), Outcome::Occupied);
        assert_eq!(
            fs::read_to_string(&skill).unwrap(),
            "---\nname: bbs\n---\nhand written\n"
        );
        assert_eq!(install(&harness, true).unwrap(), Outcome::Updated);
        assert_eq!(
            fs::read_to_string(&skill).unwrap(),
            skill_markdown().unwrap()
        );
    }

    /// opencode and Amp read `~/.config/<name>` on every platform, so the
    /// override has to be the XDG one rather than the platform default.
    #[test]
    fn config_directories_follow_xdg_rather_than_the_platform_convention() {
        let home = Path::new("/home/example");
        match env::var_os("XDG_CONFIG_HOME") {
            Some(value) if !value.is_empty() => {
                assert_eq!(xdg_config(home), PathBuf::from(value));
            }
            _ => assert_eq!(xdg_config(home), home.join(".config")),
        }
    }
}
