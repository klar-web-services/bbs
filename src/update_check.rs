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
}
