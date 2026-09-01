use anyhow::{Context, Result, bail};
use better_bitbucket_search::{
    app::{BbsApp, Progress},
    auth, bitbucket, cache,
    cli::{AutoUpdateState, CacheCommand, Cli, Command, ListCommand},
    config::Config,
    model::{RepositoryCatalog, SearchEvent},
    output, server, update, update_check,
};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use std::{
    io::{self, IsTerminal, Read},
    process::ExitCode,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

const SPINNER_TICKS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .init();
    match run().await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> Result<u8> {
    let cli = Cli::parse();
    let config = Config::load()?;
    let app = BbsApp::new(config.clone())?.preferring_env_token(cli.env_token);
    if let Some(notice) = update_notice(&cli, &config).await {
        eprintln!("{notice}");
    }
    match &cli.command {
        Some(Command::Login(args)) => {
            eprintln!(
                "Create a scoped Bitbucket API token with read:workspace:bitbucket and read:repository:bitbucket."
            );
            eprintln!("Mint one at https://id.atlassian.com/manage-profile/security/api-tokens");
            let token = if args.token_stdin {
                let mut input = String::new();
                io::stdin().read_to_string(&mut input)?;
                input.trim().to_owned()
            } else {
                rpassword::prompt_password("Bitbucket API token: ")?
            };
            if token.is_empty() {
                bail!("token cannot be empty");
            }
            let access_summary = app
                .validate_login(&token)
                .await
                .context("token validation failed")?;
            auth::store_token(&token)?;
            println!("Logged in to Bitbucket with {access_summary}.");
            Ok(0)
        }
        Some(Command::Logout) => {
            auth::delete_token()?;
            println!("Removed the saved Bitbucket credential.");
            Ok(0)
        }
        Some(Command::Repos(args))
        | Some(Command::List {
            command: ListCommand::Repos(args),
        }) => {
            // Parsed before the catalog is fetched, so a mistyped pattern
            // costs nothing and is reported straight away.
            let filter = match args.pattern() {
                Some(pattern) => Some(bitbucket::RepoFilter::parse(pattern, args.regex)?),
                None if args.regex => {
                    bail!("--regex needs a filter to apply to; pass one, or drop --regex")
                }
                None => None,
            };
            let catalog = app.catalog(args.offline, None).await?;
            let total = catalog.repositories.len();
            let selected: Vec<_> = match &filter {
                Some(filter) => bitbucket::filter_repositories(&catalog.repositories, filter)
                    .into_iter()
                    .cloned()
                    .collect(),
                None => catalog.repositories.clone(),
            };
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&RepositoryCatalog {
                        repositories: selected,
                        ..catalog
                    })?
                );
            } else {
                for repo in &selected {
                    println!(
                        "{:<48} {}",
                        repo.full_name,
                        repo.default_branch.as_deref().unwrap_or("<empty>")
                    );
                }
                // Say how much was hidden, so an over-narrow filter is
                // obvious rather than looking like an empty account.
                if filter.is_some() {
                    println!("{} of {total} repositories", selected.len());
                }
            }
            Ok(0)
        }
        Some(Command::Warmup(args)) => {
            let spinner = spinner("Warming up");
            let progress = spinner_progress(&spinner);
            let warmed = app.warmup(args.request(), progress).await;
            spinner.finish_and_clear();
            let report = warmed?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", output::warmup_summary(&report));
            }
            Ok(0)
        }
        Some(Command::Serve(args)) => {
            server::serve(app, args.port.unwrap_or(config.default_port), !args.no_open).await?;
            Ok(0)
        }
        Some(Command::Update(args)) => update_command(args.check, &config).await,
        Some(Command::AutoUpdate(args)) => {
            let enabled = matches!(args.state, AutoUpdateState::On);
            config.set_auto_update(enabled)?;
            println!(
                "Automatic updates are {}.",
                if enabled { "on" } else { "off" }
            );
            Ok(0)
        }
        Some(Command::Cache { command }) => {
            match command {
                CacheCommand::Status { verbose } => {
                    let status = cache::status(&config, *verbose)?;
                    println!("{}", serde_json::to_string_pretty(&status)?);
                }
                CacheCommand::Prune => {
                    let lock_config = config.clone();
                    let _lock = tokio::task::spawn_blocking(move || {
                        better_bitbucket_search::git_sync::lock_searches(&lock_config)
                    })
                    .await??;
                    let results = cache::prune_results(&config)?;
                    let snapshots = cache::prune_snapshots(&config)?;
                    println!(
                        "Pruned {results} bytes of result cache and {snapshots} bytes of refetchable snapshots."
                    );
                }
                CacheCommand::ClearResults => {
                    cache::clear_results(&config)?;
                    println!("Cleared the result cache.");
                }
                CacheCommand::Forget { repository } => {
                    // Resolved through the cached catalog, so the same short
                    // names, patterns and did-you-mean apply here as to
                    // `--repos`.
                    let catalog = cache::load_catalog(&config)?;
                    let selected = bitbucket::resolve_repositories(
                        &catalog,
                        std::slice::from_ref(repository),
                    )?;
                    let lock_config = config.clone();
                    let _lock = tokio::task::spawn_blocking(move || {
                        better_bitbucket_search::git_sync::lock_searches(&lock_config)
                    })
                    .await??;
                    let mut freed = 0;
                    for repo in &selected {
                        freed += cache::forget(&config, repo)?;
                        println!("Forgot cached snapshots for {}.", repo.full_name);
                    }
                    println!("Reclaimed {freed} bytes.");
                }
            }
            Ok(0)
        }
        None => {
            if cli.queries.is_empty() {
                bail!("a query or command is required; run `bbs --help`");
            }
            let spinner = spinner("Preparing search");
            let progress = spinner_progress(&spinner);
            let search = app
                .search(
                    cli.search_request(),
                    progress,
                    Arc::new(AtomicBool::new(false)),
                )
                .await;
            spinner.finish_and_clear();
            let response = search?;
            output::render(&response, cli.render_options())?;
            // Based on what matched, not on what was displayed: `--max-results
            // 0` used to report "no matches" for a query with hundreds.
            Ok(if response.total_results == 0 { 1 } else { 0 })
        }
    }
}

/// Commands that manage the update itself, and so must not be preceded by an
/// update check.
fn exempt_from_check(cli: &Cli) -> bool {
    matches!(
        cli.command,
        Some(Command::Update(_)) | Some(Command::AutoUpdate(_))
    )
}

/// `bbs serve` is excluded even when the preference is on. See AC5: the web
/// client must not replace its own binary, because it is the one command
/// people leave running unattended.
fn should_auto_update(cli: &Cli, preference: bool) -> bool {
    preference && !matches!(cli.command, Some(Command::Serve(_)))
}

/// Resolves update state and returns the banner to show, if any.
///
/// Infallible by construction: a failed check, an unwritable cache, or a
/// rate-limited GitHub all produce `None` and leave the command untouched.
/// Exit codes are never affected by update state.
async fn update_notice(cli: &Cli, config: &Config) -> Option<String> {
    if exempt_from_check(cli) {
        return None;
    }
    let current = update::Version::current().ok()?;
    let available = update_check::resolve(config).await?;

    if should_auto_update(cli, config.auto_update)
        && auto_update_allowed(std::env::var_os(REEXEC_GUARD))
    {
        // Clear before handing over, so the new binary does not re-offer the
        // version it has just become. This restamps rather than unlinking, so
        // the child does not immediately spend a GitHub request.
        update_check::save(
            config,
            &update_check::UpdateState {
                last_checked: Some(chrono::Utc::now()),
                available: None,
            },
        );
        match auto_update(current, available).await {
            // Windows only; Unix `exec` does not return on success.
            Ok(code) => std::process::exit(i32::from(code)),
            Err(error) => {
                // A failed auto-update must not turn a working search into an
                // error. Say so and fall through to the ordinary banner.
                eprintln!("warning: automatic update failed: {error:#}");
            }
        }
    }

    Some(update_check::banner(
        current,
        available,
        output::should_color_stderr(cli.color),
    ))
}

/// Set in the environment of a re-exec'd child so it cannot update again.
const REEXEC_GUARD: &str = "BBS_AUTO_UPDATED";

fn auto_update_allowed(guard: Option<std::ffi::OsString>) -> bool {
    guard.is_none()
}

/// Installs the update, then hands the command to the new binary.
///
/// On Unix this never returns on success: `exec` replaces the process image.
/// On Windows the running executable cannot replace its own image, so the new
/// binary is spawned and its exit code forwarded.
async fn auto_update(current: update::Version, latest: update::Version) -> Result<u8> {
    let target = std::env::current_exe().context("cannot locate the running bbs binary")?;
    eprintln!("Updating bbs {current} -> {latest}");
    let http = update::client()?;
    let archive =
        update::download(&http, update::DOWNLOAD_BASE, &update::repository(), latest).await?;
    let binary = update::extract(&archive)?;
    update::replace(&target, &binary)?;
    eprintln!("Updated bbs to {latest}; continuing on the new version");

    let arguments: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let mut command = std::process::Command::new(&target);
    command.args(&arguments).env(REEXEC_GUARD, "1");

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Only returns on failure.
        let error = command.exec();
        Err(anyhow::Error::new(error).context("cannot restart bbs after updating"))
    }
    #[cfg(windows)]
    {
        let status = command
            .status()
            .context("cannot restart bbs after updating")?;
        Ok(status.code().unwrap_or(2) as u8)
    }
}

async fn update_command(check_only: bool, config: &Config) -> Result<u8> {
    let http = update::client()?;
    let repository = update::repository();
    let current = update::Version::current()?;
    let latest = update::latest_version(&http, update::API_BASE, &repository).await?;

    // Whatever happens below, this command has just asked GitHub, so the
    // shared clock is restamped and any cached offer is re-evaluated.
    let clear = |available: Option<update::Version>| {
        update_check::save(
            config,
            &update_check::UpdateState {
                last_checked: Some(chrono::Utc::now()),
                available,
            },
        )
    };

    if latest <= current {
        // Also the repair path for an out-of-band upgrade: a cached offer for
        // a version we are already running is dropped here.
        clear(None);
        println!("bbs {current} is already the latest release.");
        return Ok(0);
    }
    if check_only {
        // `--check` records what it found, so it and ordinary commands share
        // one throttle rather than each keeping their own.
        clear(Some(latest));
        println!("bbs {current} (latest {latest})");
        return Ok(1);
    }
    let target = std::env::current_exe().context("cannot locate the running bbs binary")?;
    eprintln!("Updating bbs {current} -> {latest}");
    let archive = update::download(&http, update::DOWNLOAD_BASE, &repository, latest).await?;
    let binary = update::extract(&archive)?;
    update::replace(&target, &binary)?;
    clear(None);
    println!("Updated bbs to {latest} at {}", target.display());
    Ok(0)
}

fn spinner(initial: &str) -> ProgressBar {
    if !io::stderr().is_terminal() {
        return ProgressBar::hidden();
    }
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg:.dim}")
            .expect("static spinner template is valid")
            .tick_strings(SPINNER_TICKS),
    );
    spinner.set_message(initial.to_owned());
    spinner.enable_steady_tick(Duration::from_millis(80));
    spinner
}

/// Drives `spinner` from the progress stream. Warnings are printed above the
/// spinner rather than into it: a skipped repository is a thing to keep, not a
/// label to be overwritten by the next event.
fn spinner_progress(spinner: &ProgressBar) -> Progress {
    let spinner = spinner.clone();
    Arc::new(move |event| {
        if let SearchEvent::Warning { message } = &event {
            spinner.suspend(|| eprintln!("warning: {message}"));
            return;
        }
        if let Some(message) = spinner_message(&event) {
            spinner.set_message(message);
        }
    })
}

fn spinner_message(event: &SearchEvent) -> Option<String> {
    match event {
        SearchEvent::Progress {
            phase,
            message,
            current,
            total,
        } => Some(match phase.as_str() {
            "discovery" => "Discovering repositories".into(),
            "sync" if *total > 0 => format!("Syncing repositories {current}/{total}"),
            "search" => "Searching snapshots".into(),
            _ => message.clone(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_events_get_concise_spinner_labels() {
        let event = SearchEvent::Progress {
            phase: "sync".into(),
            message: "Synchronized 2 of 5 repositories".into(),
            current: 2,
            total: 5,
        };

        assert_eq!(
            spinner_message(&event).as_deref(),
            Some("Syncing repositories 2/5")
        );
    }

    #[test]
    fn the_update_commands_are_exempt_from_the_check() {
        // `update` reports version state itself. `auto-update` must be exempt
        // for a sharper reason: `bbs auto-update off` with the setting on and
        // an update pending would otherwise install it and re-exec *before*
        // honouring the request to turn the feature off.
        for argv in [
            ["bbs", "update"].as_slice(),
            ["bbs", "update", "--check"].as_slice(),
            ["bbs", "auto-update", "off"].as_slice(),
            ["bbs", "auto-update", "on"].as_slice(),
        ] {
            let cli = Cli::try_parse_from(argv).unwrap();
            assert!(exempt_from_check(&cli), "{argv:?} must be exempt");
        }

        for argv in [
            ["bbs", "foo"].as_slice(),
            ["bbs", "serve"].as_slice(),
            ["bbs", "list", "repos"].as_slice(),
        ] {
            let cli = Cli::try_parse_from(argv).unwrap();
            assert!(!exempt_from_check(&cli), "{argv:?} must be checked");
        }
    }

    #[test]
    fn serve_is_never_auto_updated() {
        // AC5 carves out the web client: it is what runs under systemd or
        // nohup, where replacing the binary underneath is exactly the
        // surprise to avoid.
        let serve = Cli::try_parse_from(["bbs", "serve"]).unwrap();
        assert!(!should_auto_update(&serve, true));
        let search = Cli::try_parse_from(["bbs", "foo"]).unwrap();
        assert!(should_auto_update(&search, true));
        assert!(!should_auto_update(&search, false), "off means off");
    }

    #[test]
    fn the_guard_variable_stops_a_second_auto_update() {
        // The child of a re-exec must never auto-update again: a new binary that
        // still believed an update was pending would loop forever.
        assert!(!auto_update_allowed(Some(std::ffi::OsString::from("1"))));
        assert!(auto_update_allowed(None));
    }
}
