use anyhow::{Context, Result, bail};
use better_bitbucket_search::{
    app::{BbsApp, Progress},
    auth, bitbucket, cache,
    cli::{CacheCommand, Cli, Command, ListCommand},
    config::Config,
    model::{RepositoryCatalog, SearchEvent},
    output, server, update,
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
    match &cli.command {
        Some(Command::Login(args)) => {
            eprintln!(
                "Create a scoped Bitbucket API token with read:workspace:bitbucket and read:repository:bitbucket."
            );
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
        Some(Command::Serve(args)) => {
            server::serve(app, args.port.unwrap_or(config.default_port), !args.no_open).await?;
            Ok(0)
        }
        Some(Command::Update(args)) => update_command(args.check).await,
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
            let spinner = search_spinner();
            let spinner_for_progress = spinner.clone();
            let progress: Progress = Arc::new(move |event| {
                if let SearchEvent::Warning { message } = &event {
                    spinner_for_progress.suspend(|| eprintln!("warning: {message}"));
                    return;
                }
                if let Some(message) = spinner_message(&event) {
                    spinner_for_progress.set_message(message);
                }
            });
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

async fn update_command(check_only: bool) -> Result<u8> {
    let http = update::client()?;
    let repository = update::repository();
    let current = update::Version::current()?;
    let latest = update::latest_version(&http, update::API_BASE, &repository).await?;
    if latest <= current {
        println!("bbs {current} is already the latest release.");
        return Ok(0);
    }
    if check_only {
        println!("bbs {current} (latest {latest})");
        return Ok(1);
    }
    let target = std::env::current_exe().context("cannot locate the running bbs binary")?;
    eprintln!("Updating bbs {current} -> {latest}");
    let archive = update::download(&http, update::DOWNLOAD_BASE, &repository, latest).await?;
    let binary = update::extract(&archive)?;
    update::replace(&target, &binary)?;
    println!("Updated bbs to {latest} at {}", target.display());
    Ok(0)
}

fn search_spinner() -> ProgressBar {
    if !io::stderr().is_terminal() {
        return ProgressBar::hidden();
    }
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg:.dim}")
            .expect("static spinner template is valid")
            .tick_strings(SPINNER_TICKS),
    );
    spinner.set_message("Preparing search");
    spinner.enable_steady_tick(Duration::from_millis(80));
    spinner
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
}
