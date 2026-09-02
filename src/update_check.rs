//! Policy for the automatic update check: when to ask GitHub, what to
//! remember, and what to say. The mechanism — the release lookup, the
//! download, the checksum, the in-place replace — lives in [`crate::update`].

use crate::{config::Config, update::Version};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How long a "nothing new" answer is reused before asking GitHub again.
///
/// The GitHub release endpoint is called unauthenticated, which is limited to
/// 60 requests per hour per IP. Checking on literally every command would
/// exhaust that during ordinary use and then fail on every command after. The
/// same interval is used by the server poll, so both paths share one budget.
pub const CHECK_INTERVAL_SECS: i64 = 300;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateState {
    pub last_checked: Option<DateTime<Utc>>,
    #[serde(with = "version_string")]
    pub available: Option<Version>,
}

/// `Version` has no serde derives, and should not gain any: it would put
/// `{"major":0,...}` in a user-visible file and in the HTTP response. It has
/// `Display` and `parse`, so those are used instead.
mod version_string {
    use super::Version;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        value: &Option<Version>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(version) => serializer.serialize_str(&version.to_string()),
            None => serializer.serialize_none(),
        }
    }

    /// An unparseable version reads as absent rather than as an error, so a
    /// hand-edited or partially written file degrades instead of failing.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Version>, D::Error> {
        Ok(
            Option::<String>::deserialize(deserializer)?
                .and_then(|text| Version::parse(&text).ok()),
        )
    }
}

pub fn state_path(config: &Config) -> PathBuf {
    config.cache_dir.join("update.json")
}

/// Never fails. Missing, unreadable, and malformed all read as empty state.
pub fn load(config: &Config) -> UpdateState {
    std::fs::read_to_string(state_path(config))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Best effort. A cache directory that cannot be written is not a reason to
/// fail the user's command.
pub fn save(config: &Config, state: &UpdateState) {
    let _ = std::fs::create_dir_all(&config.cache_dir);
    if let Ok(text) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(state_path(config), text);
    }
}

/// What the state file says should happen next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Rule 1: a known-newer release. Report it; make no network call.
    Cached(Version),
    /// Rule 2: asked recently enough. Make no network call, report nothing.
    Throttled,
    /// Ask GitHub.
    Check,
}

/// True when the file holds an `available` that is not actually newer than
/// what is running — the out-of-band-upgrade case. Callers use this to know
/// the file needs rewriting even when no check is made.
pub fn is_stale(state: &UpdateState, current: Version) -> bool {
    state
        .available
        .is_some_and(|available| available <= current)
}

pub fn decide(state: &UpdateState, current: Version, now: DateTime<Utc>) -> Decision {
    // Rule 1.
    if let Some(available) = state.available.filter(|available| *available > current) {
        return Decision::Cached(available);
    }
    // Rule 0 lands here: a cached value that is not newer is treated exactly
    // as if it were absent, and then rule 2 applies as normal.
    match state.last_checked {
        Some(last)
            if now.signed_duration_since(last) < chrono::Duration::seconds(CHECK_INTERVAL_SECS) =>
        {
            Decision::Throttled
        }
        _ => Decision::Check,
    }
}

/// The whole policy in one place, so the CLI and the server poll cannot
/// drift apart. Returns the version to tell the user about, if any.
///
/// Never returns an error: every failure mode — offline, DNS, a 403 from the
/// rate limiter, malformed JSON, timeout — is reported as "no update known"
/// and the caller carries on.
pub async fn resolve(config: &Config) -> Option<Version> {
    resolve_against(
        config,
        crate::update::API_BASE,
        &crate::update::repository(),
    )
    .await
}

async fn resolve_against(config: &Config, api_base: &str, repository: &str) -> Option<Version> {
    let current = Version::current().ok()?;
    let mut state = load(config);
    match decide(&state, current, Utc::now()) {
        Decision::Cached(available) => Some(available),
        Decision::Throttled => {
            if is_stale(&state, current) {
                state.available = None;
                save(config, &state);
            }
            None
        }
        Decision::Check => {
            let found = match crate::update::checking_client() {
                Ok(http) => crate::update::latest_version(&http, api_base, repository)
                    .await
                    .ok(),
                Err(_) => None,
            };
            // A failed lookup still stamps, so an offline machine does not
            // retry on every single command.
            state.last_checked = Some(Utc::now());
            state.available = found.filter(|latest| *latest > current);
            save(config, &state);
            state.available
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Both directories are redirected, so no test can touch the real
    /// `~/.config` or `~/.cache`.
    fn config_in(dir: &std::path::Path) -> crate::config::Config {
        crate::config::Config {
            cache_dir: dir.to_path_buf(),
            config_dir: dir.join("config"),
            ..Default::default()
        }
    }

    #[test]
    fn state_round_trips_through_the_file() {
        let dir = tempdir().unwrap();
        let config = config_in(dir.path());
        let stamp = chrono::Utc::now();
        let state = UpdateState {
            last_checked: Some(stamp),
            available: Some(crate::update::Version::parse("9.9.9").unwrap()),
        };

        save(&config, &state);
        let loaded = load(&config);

        assert_eq!(
            loaded.available,
            Some(crate::update::Version::parse("9.9.9").unwrap())
        );
        assert_eq!(loaded.last_checked.unwrap().timestamp(), stamp.timestamp());
    }

    /// The version must be a plain string in the file, not a struct. This is
    /// a user-visible artifact and is also served over HTTP.
    #[test]
    fn the_version_is_stored_as_a_string() {
        let dir = tempdir().unwrap();
        let config = config_in(dir.path());
        save(
            &config,
            &UpdateState {
                last_checked: None,
                available: Some(crate::update::Version::parse("1.2.3").unwrap()),
            },
        );

        let text = std::fs::read_to_string(state_path(&config)).unwrap();

        assert!(text.contains("\"1.2.3\""), "unexpected shape: {text}");
        assert!(!text.contains("major"), "must not be a struct: {text}");
    }

    /// Corrupt state must never be able to fail a search.
    #[test]
    fn unreadable_or_malformed_state_reads_as_empty() {
        let dir = tempdir().unwrap();
        let config = config_in(dir.path());

        assert!(load(&config).available.is_none(), "missing file");

        std::fs::write(state_path(&config), b"{not json").unwrap();
        assert!(load(&config).available.is_none(), "truncated file");

        std::fs::write(state_path(&config), b"{\"available\":\"not-a-version\"}").unwrap();
        assert!(load(&config).available.is_none(), "bad version string");
    }

    fn version(text: &str) -> crate::update::Version {
        crate::update::Version::parse(text).unwrap()
    }

    #[test]
    fn a_newer_cached_version_is_used_without_asking_github() {
        let state = UpdateState {
            last_checked: Some(Utc::now()),
            available: Some(version("9.9.9")),
        };
        assert_eq!(
            decide(&state, version("1.0.0"), Utc::now()),
            Decision::Cached(version("9.9.9"))
        );
    }

    #[test]
    fn a_recent_negative_answer_is_reused() {
        let now = Utc::now();
        let state = UpdateState {
            last_checked: Some(now - chrono::Duration::seconds(CHECK_INTERVAL_SECS - 1)),
            available: None,
        };
        assert_eq!(decide(&state, version("1.0.0"), now), Decision::Throttled);
    }

    #[test]
    fn a_stale_negative_answer_triggers_a_check() {
        let now = Utc::now();
        let state = UpdateState {
            last_checked: Some(now - chrono::Duration::seconds(CHECK_INTERVAL_SECS + 1)),
            available: None,
        };
        assert_eq!(decide(&state, version("1.0.0"), now), Decision::Check);
        assert_eq!(
            decide(&UpdateState::default(), version("1.0.0"), now),
            Decision::Check,
            "empty state must check"
        );
    }

    /// Rule 0. Without this the feature has a permanent failure mode: a user who
    /// upgrades with install.sh instead of `bbs update` leaves a cached
    /// `available` naming the version they are now running. Rule 1 would then
    /// refuse to ever call GitHub again, and every command would print
    /// "0.6.0 -> 0.6.0" forever.
    #[test]
    fn a_cached_version_that_is_not_newer_is_discarded() {
        let now = Utc::now();
        for cached in ["1.0.0", "0.9.0"] {
            let state = UpdateState {
                last_checked: Some(now - chrono::Duration::seconds(CHECK_INTERVAL_SECS + 1)),
                available: Some(version(cached)),
            };
            assert_eq!(
                decide(&state, version("1.0.0"), now),
                Decision::Check,
                "cached {cached} against running 1.0.0 must not be offered"
            );
            assert!(is_stale(&state, version("1.0.0")));
        }
    }

    /// A discarded value still respects the throttle rather than checking at once.
    #[test]
    fn a_discarded_cached_version_still_honours_the_throttle() {
        let now = Utc::now();
        let state = UpdateState {
            last_checked: Some(now - chrono::Duration::seconds(10)),
            available: Some(version("1.0.0")),
        };
        assert_eq!(decide(&state, version("1.0.0"), now), Decision::Throttled);
    }

    /// A minimal stand-in for the GitHub releases API, matching the mirror
    /// pattern already used in `update.rs`'s tests.
    async fn release_api(tag: &'static str) -> String {
        use axum::{Router, routing::get};
        let body = format!("{{\"tag_name\":\"{tag}\"}}");
        let router = Router::new().route(
            "/repos/team/repo/releases/latest",
            get(move || async move { body }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn a_newer_release_is_found_and_recorded() {
        let dir = tempdir().unwrap();
        let config = config_in(dir.path());
        let base = release_api("v9.9.9").await;

        let found = resolve_against(&config, &base, "team/repo").await;

        assert_eq!(found, Some(version("9.9.9")));
        let state = load(&config);
        assert_eq!(state.available, Some(version("9.9.9")));
        assert!(state.last_checked.is_some(), "the check must be stamped");
    }

    #[tokio::test]
    async fn an_older_release_records_a_negative_answer() {
        let dir = tempdir().unwrap();
        let config = config_in(dir.path());
        let base = release_api("v0.0.1").await;

        assert_eq!(resolve_against(&config, &base, "team/repo").await, None);

        let state = load(&config);
        assert_eq!(state.available, None);
        assert!(state.last_checked.is_some(), "a negative answer is stamped");
    }

    /// An unreachable or refusing GitHub must be silent, not fatal.
    #[tokio::test]
    async fn an_unreachable_github_yields_no_update_and_does_not_panic() {
        let dir = tempdir().unwrap();
        let config = config_in(dir.path());

        let found = resolve_against(&config, "http://127.0.0.1:1", "team/repo").await;

        assert_eq!(found, None);
    }

    /// Rule 0 end to end: a stale cached value is rewritten, and the next check
    /// is actually issued rather than being suppressed again.
    #[tokio::test]
    async fn a_stale_cached_version_is_repaired_on_disk() {
        let dir = tempdir().unwrap();
        let config = config_in(dir.path());
        let current = crate::update::Version::current().unwrap();
        save(
            &config,
            &UpdateState {
                last_checked: Some(Utc::now() - chrono::Duration::seconds(CHECK_INTERVAL_SECS + 1)),
                available: Some(current),
            },
        );
        let base = release_api("v0.0.1").await;

        assert_eq!(resolve_against(&config, &base, "team/repo").await, None);

        assert_eq!(
            load(&config).available,
            None,
            "the stale value must be gone"
        );
    }
}
