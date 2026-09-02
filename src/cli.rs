use crate::{
    app::{SearchRequest, WarmupRequest},
    output::{OutputMode, RenderOptions},
    query::CaseMode,
    search::SortMode,
};
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "bbs", version, about = "Fast local code search for Bitbucket Cloud", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Boolean query expressions. Multiple expressions are ORed.
    #[arg(value_name = "QUERY")]
    pub queries: Vec<String>,

    /// Treat every query as a raw PCRE2 regular expression.
    #[arg(short = 'r', long)]
    pub regex: bool,

    /// Let wildcards and `.` span line breaks.
    #[arg(short = 'M', long)]
    pub multiline: bool,

    /// Require a word boundary either side of every term.
    #[arg(short = 'w', long)]
    pub word: bool,

    /// Repositories as unique slugs, workspace/slug names, or UUIDs.
    #[arg(long, num_args = 1.., value_delimiter = ',')]
    pub repos: Vec<String>,

    /// Git-style path glob. May be repeated. A pattern with no `/` matches at
    /// any depth; prefix `./` for the repository root only, `!` to exclude.
    #[arg(long = "path")]
    pub paths: Vec<String>,

    /// Exclude paths matching this glob. May be repeated. Equivalent to
    /// `--path '!<glob>'`.
    #[arg(long = "exclude-path")]
    pub exclude_paths: Vec<String>,

    /// Exclude vendor, generated, dist, build and node_modules trees.
    #[arg(long)]
    pub no_vendor: bool,

    /// Search this branch instead of each repository's default branch.
    #[arg(long)]
    pub branch: Option<String>,

    /// Search cached snapshots without contacting Bitbucket.
    #[arg(long)]
    pub offline: bool,

    /// Authenticate with the BB_TOKEN environment variable in preference to
    /// the credential saved by `bbs login`. Without this, BB_TOKEN is only a
    /// fallback: for an account that has never logged in, and for one whose
    /// saved credential Bitbucket has started rejecting.
    #[arg(long, global = true)]
    pub env_token: bool,

    /// Reuse any snapshot fetched within this window instead of fetching it
    /// again, e.g. `5m`, `1h30m`, `2d`. Default is to always fetch.
    #[arg(long, value_name = "DURATION", value_parser = crate::duration::parse_duration_secs)]
    pub max_age: Option<u64>,

    #[arg(short = 'i', long, conflicts_with = "case_sensitive")]
    pub ignore_case: bool,

    #[arg(short = 's', long, conflicts_with = "ignore_case")]
    pub case_sensitive: bool,

    #[arg(short = 'C', long)]
    pub context: Option<usize>,

    #[arg(long)]
    pub max_results: Option<usize>,

    /// Skip files larger than this, e.g. `512k`, `4M`, `1.5G`. `none` (or `0`)
    /// searches every file whatever its size. Defaults to the configured
    /// `max_file_bytes`, 10 MiB out of the box.
    #[arg(long, value_name = "SIZE", value_parser = crate::size::parse_size)]
    pub max_file_size: Option<u64>,

    #[arg(long, value_enum, default_value = "relevance")]
    pub sort: CliSort,

    /// Print only the repository and path of each matching file.
    #[arg(short = 'l', long, conflicts_with = "count")]
    pub files_with_matches: bool,

    /// Print the match count for each matching file.
    #[arg(long)]
    pub count: bool,

    /// Break out synchronization time from scan time.
    #[arg(long)]
    pub stats: bool,

    #[arg(long, value_enum, default_value = "terminal")]
    pub format: OutputFormat,

    #[arg(long, value_enum, default_value = "auto")]
    pub color: ColorChoice,

    #[arg(long)]
    pub no_cache: bool,
}

impl Cli {
    pub fn search_request(&self) -> SearchRequest {
        SearchRequest {
            queries: self.queries.clone(),
            repositories: self.repos.clone(),
            paths: self.paths.clone(),
            exclude_paths: self.exclude_paths.clone(),
            no_vendor: self.no_vendor,
            branch: self.branch.clone(),
            regex: self.regex,
            multiline: self.multiline,
            word: self.word,
            case_mode: if self.ignore_case {
                CaseMode::Ignore
            } else if self.case_sensitive {
                CaseMode::Sensitive
            } else {
                CaseMode::Smart
            },
            offline: self.offline,
            max_age_seconds: self.max_age,
            context: self.context,
            max_results: self.max_results,
            max_file_bytes: self.max_file_size,
            sort: self.sort.into(),
            no_cache: self.no_cache,
        }
    }

    pub fn render_options(&self) -> RenderOptions {
        RenderOptions {
            format: self.format,
            color: self.color,
            mode: if self.files_with_matches {
                OutputMode::FilesWithMatches
            } else if self.count {
                OutputMode::Count
            } else {
                OutputMode::Full
            },
            // Only meaningful when the list is actually ordered by repository.
            group_by_repository: matches!(self.sort, CliSort::Repo),
            stats: self.stats,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Save and validate a scoped Bitbucket API token.
    Login(LoginArgs),
    /// Remove the saved Bitbucket credential.
    Logout,
    /// Inspect the Bitbucket credential.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Install the bundled coding-agent skill into the agents on this machine.
    Skill(SkillArgs),
    /// List what bbs can see.
    List {
        #[command(subcommand)]
        command: ListCommand,
    },
    /// List repositories visible to the authenticated account. Shorthand for
    /// `bbs list repos`.
    Repos(ReposArgs),
    /// Clone and refresh repositories ahead of time, so the next search
    /// starts from a warm cache.
    Warmup(WarmupArgs),
    /// Start the local browser interface.
    Serve(ServeArgs),
    /// Update bbs to the latest published release.
    Update(UpdateArgs),
    /// Turn automatic updates on or off.
    AutoUpdate(AutoUpdateArgs),
    /// Inspect or prune local filesystem caches.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Report whether a credential is available. Exits 0 when one is, 1 when
    /// none is, and 2 on a real failure, so a script -- or a coding agent --
    /// can branch on it without parsing prose.
    Status(AuthStatusArgs),
}

#[derive(Debug, Args)]
pub struct AuthStatusArgs {
    /// Present the credential to Bitbucket, rather than only reporting that
    /// one exists. Costs a round trip.
    #[arg(long)]
    pub verify: bool,

    /// Print the status as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct SkillArgs {
    /// Install into these harnesses by identifier instead of choosing from a
    /// menu. May be repeated or comma-separated.
    #[arg(long = "harness", value_name = "ID", num_args = 1.., value_delimiter = ',')]
    pub harnesses: Vec<String>,

    /// Install into every harness detected on this machine.
    #[arg(long, conflicts_with = "harnesses")]
    pub all: bool,

    /// List the known harnesses, where each one would be installed, and
    /// whether it was detected. Installs nothing.
    #[arg(long, conflicts_with_all = ["all", "harnesses", "force", "print"])]
    pub list: bool,

    /// Write the bundled SKILL.md to standard output. Installs nothing.
    #[arg(long, conflicts_with_all = ["all", "harnesses", "force"])]
    pub print: bool,

    /// Replace a skill named `bbs` that bbs did not write.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Read the token from standard input instead of prompting.
    #[arg(long)]
    pub token_stdin: bool,
}

#[derive(Debug, Subcommand)]
pub enum ListCommand {
    /// List repositories visible to the authenticated account.
    #[command(alias = "repositories")]
    Repos(ReposArgs),
}

#[derive(Debug, Args)]
pub struct ReposArgs {
    /// Positional form of `--filter`.
    #[arg(value_name = "FILTER", conflicts_with = "filter")]
    pub positional_filter: Option<String>,

    /// Show only repositories whose slug, workspace/slug, or display name
    /// matches: a substring, a `*`/`?` glob, or a `/regex/` with the same
    /// trailing `icsmx` flags a query takes.
    #[arg(long, value_name = "PATTERN")]
    pub filter: Option<String>,

    /// Read the filter as a raw PCRE2 regular expression, with no surrounding
    /// slashes.
    #[arg(short = 'r', long)]
    pub regex: bool,

    /// List the last discovered catalog without contacting Bitbucket.
    #[arg(long)]
    pub offline: bool,

    /// Print the catalog as JSON.
    #[arg(long)]
    pub json: bool,
}

impl ReposArgs {
    /// The filter, written either way. Keeping the positional form is what
    /// lets `bbs repos api` keep working now that `--filter` exists.
    pub fn pattern(&self) -> Option<&str> {
        self.filter.as_deref().or(self.positional_filter.as_deref())
    }
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Report whether an update is available without installing it.
    #[arg(long)]
    pub check: bool,
}

#[derive(Debug, Args)]
pub struct AutoUpdateArgs {
    /// `on` installs an available update when a command runs; `off` only
    /// reports it.
    #[arg(value_name = "STATE")]
    pub state: AutoUpdateState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AutoUpdateState {
    On,
    Off,
}

#[derive(Debug, Args)]
pub struct WarmupArgs {
    /// Repositories as unique slugs, workspace/slug names, UUIDs, or `*`
    /// patterns. Defaults to every accessible repository.
    #[arg(long, num_args = 1.., value_delimiter = ',')]
    pub repos: Vec<String>,

    /// Warm this branch instead of each repository's default branch.
    #[arg(long)]
    pub branch: Option<String>,

    /// Leave alone any snapshot fetched within this window, e.g. `6h`, `2d`.
    /// A scheduled warmup wants this; the default refetches everything.
    #[arg(long, value_name = "DURATION", value_parser = crate::duration::parse_duration_secs)]
    pub max_age: Option<u64>,

    /// Repositories to fetch at once. Defaults to the configured
    /// `sync_concurrency`.
    #[arg(long, short = 'j', value_name = "N")]
    pub concurrency: Option<usize>,

    /// Print the report as JSON.
    #[arg(long)]
    pub json: bool,
}

impl WarmupArgs {
    pub fn request(&self) -> WarmupRequest {
        WarmupRequest {
            repositories: self.repos.clone(),
            branch: self.branch.clone(),
            max_age_seconds: self.max_age,
            concurrency: self.concurrency,
        }
    }
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long)]
    pub no_open: bool,
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Report cache sizes and entry counts.
    Status {
        /// List every snapshot with its repository, branch, commit and age.
        #[arg(long)]
        verbose: bool,
    },
    /// Trim the cache to the configured budgets.
    Prune,
    /// Drop cached results, keeping the clones.
    ClearResults,
    /// Drop every cached snapshot of one repository.
    Forget {
        /// Repository slug, workspace/slug name, or UUID.
        #[arg(value_name = "REPOSITORY")]
        repository: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliSort {
    Relevance,
    Repo,
    Path,
}
impl From<CliSort> for SortMode {
    fn from(value: CliSort) -> Self {
        match value {
            CliSort::Relevance => Self::Relevance,
            CliSort::Repo => Self::Repo,
            CliSort::Path => Self::Path,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    Terminal,
    Json,
    Jsonl,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[test]
    fn parses_the_update_command() {
        let plain = Cli::try_parse_from(["bbs", "update"]).unwrap();
        assert!(matches!(plain.command, Some(Command::Update(ref a)) if !a.check));
        let checking = Cli::try_parse_from(["bbs", "update", "--check"]).unwrap();
        assert!(matches!(checking.command, Some(Command::Update(ref a)) if a.check));
    }

    #[test]
    fn parses_the_auto_update_command() {
        let on = Cli::try_parse_from(["bbs", "auto-update", "on"]).unwrap();
        assert!(matches!(
            on.command,
            Some(Command::AutoUpdate(ref a)) if a.state == AutoUpdateState::On
        ));
        let off = Cli::try_parse_from(["bbs", "auto-update", "off"]).unwrap();
        assert!(matches!(
            off.command,
            Some(Command::AutoUpdate(ref a)) if a.state == AutoUpdateState::Off
        ));
    }

    /// The argument is required, and only these two words are accepted.
    #[test]
    fn auto_update_rejects_a_missing_or_unknown_state() {
        assert!(Cli::try_parse_from(["bbs", "auto-update"]).is_err());
        assert!(Cli::try_parse_from(["bbs", "auto-update", "yes"]).is_err());
        assert!(Cli::try_parse_from(["bbs", "auto-update", "on", "off"]).is_err());
    }

    /// The size limit is the difference between finding a generated file and
    /// silently walking past it, so it is a flag, in the units people use.
    #[test]
    fn max_file_size_takes_a_unit_and_reaches_the_request() {
        let cli = Cli::try_parse_from(["bbs", "needle", "--max-file-size", "32M"]).unwrap();
        assert_eq!(cli.search_request().max_file_bytes, Some(32 * 1024 * 1024));

        let unlimited = Cli::try_parse_from(["bbs", "needle", "--max-file-size", "none"]).unwrap();
        assert_eq!(unlimited.search_request().max_file_bytes, Some(u64::MAX));

        // Absent, the configured limit decides -- not a default baked in here.
        let plain = Cli::try_parse_from(["bbs", "needle"]).unwrap();
        assert_eq!(plain.search_request().max_file_bytes, None);

        assert!(Cli::try_parse_from(["bbs", "needle", "--max-file-size", "big"]).is_err());
    }

    #[test]
    fn repos_takes_its_filter_positionally_or_by_flag() {
        for argv in [
            ["bbs", "list", "repos", "edge-*"].as_slice(),
            ["bbs", "list", "repos", "--filter", "edge-*"].as_slice(),
            ["bbs", "repos", "edge-*"].as_slice(),
            ["bbs", "repos", "--filter", "edge-*"].as_slice(),
        ] {
            let args = match Cli::try_parse_from(argv).unwrap().command {
                Some(Command::Repos(args)) => args,
                Some(Command::List {
                    command: ListCommand::Repos(args),
                }) => args,
                other => panic!("{argv:?} parsed as {other:?}"),
            };
            assert_eq!(args.pattern(), Some("edge-*"), "for {argv:?}");
            assert!(!args.regex, "for {argv:?}");
        }
    }

    #[test]
    fn warmup_defaults_to_the_whole_workspace_and_takes_the_search_scopes() {
        let bare = match Cli::try_parse_from(["bbs", "warmup"]).unwrap().command {
            Some(Command::Warmup(args)) => args.request(),
            other => panic!("parsed as {other:?}"),
        };
        assert!(bare.repositories.is_empty());
        assert_eq!(bare.branch, None);
        assert_eq!(bare.max_age_seconds, None);
        assert_eq!(bare.concurrency, None);

        let scoped = match Cli::try_parse_from([
            "bbs",
            "warmup",
            "--repos",
            "edge-*",
            "team/api",
            "--branch",
            "develop",
            "--max-age",
            "6h",
            "-j",
            "16",
        ])
        .unwrap()
        .command
        {
            Some(Command::Warmup(args)) => args.request(),
            other => panic!("parsed as {other:?}"),
        };
        assert_eq!(scoped.repositories, ["edge-*", "team/api"]);
        assert_eq!(scoped.branch.as_deref(), Some("develop"));
        assert_eq!(scoped.max_age_seconds, Some(6 * 60 * 60));
        assert_eq!(scoped.concurrency, Some(16));
    }

    /// Two spellings of one value would otherwise have to be silently ranked.
    #[test]
    fn a_filter_cannot_be_given_both_ways_at_once() {
        assert!(Cli::try_parse_from(["bbs", "list", "repos", "a", "--filter", "b"]).is_err());
    }

    #[test]
    fn accepts_multiple_scopes_and_queries() {
        let cli = Cli::try_parse_from([
            "bbs",
            "foo AND bar",
            "baz",
            "--repos",
            "one",
            "two",
            "--path",
            "src/**/*.rs",
            "--branch",
            "release/2",
        ])
        .unwrap();
        let request = cli.search_request();
        assert_eq!(request.queries.len(), 2);
        assert_eq!(request.repositories.len(), 2);
        assert_eq!(request.paths, ["src/**/*.rs"]);
        assert_eq!(request.branch.as_deref(), Some("release/2"));
    }
}
