use anyhow::{Result, bail};

const SERVICE: &str = "better-bitbucket-search";
const ACCOUNT: &str = "default";
const ENV_VAR: &str = "BB_TOKEN";

/// Where a credential came from, so a rejection can name the thing to fix
/// rather than leaving the user to guess which of the two was presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Saved by `bbs login` in the system credential store.
    Saved,
    /// Read from `BB_TOKEN`.
    Environment,
    /// Handed to the process directly: `bbs login` validating what was just
    /// typed, and the tests.
    Supplied,
}

impl Source {
    pub fn describe(self) -> &'static str {
        match self {
            Source::Saved => "the saved credential",
            Source::Environment => ENV_VAR,
            Source::Supplied => "the supplied token",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    pub source: Source,
    pub token: String,
}

/// The credentials to present, in order.
///
/// `BB_TOKEN` used to win outright, so a variable left behind in a shell
/// profile silently shadowed the credential `bbs login` had just saved, and
/// nothing in the output said which of the two a search had actually used. The
/// saved credential now comes first and `BB_TOKEN` is a fallback: for an
/// account that has never logged in, and for one whose saved credential
/// Bitbucket has since rejected with a 401. `--env-token` asks for the old
/// precedence explicitly, for the one run that wants it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials(Vec<Credential>);

impl Credentials {
    /// A single credential the caller already holds.
    pub fn supplied(token: impl Into<String>) -> Self {
        Self(vec![Credential {
            source: Source::Supplied,
            token: token.into(),
        }])
    }

    /// An explicit ordering. Test-only: a real one comes from [`credentials`],
    /// which reads the credential store and the environment, both of which are
    /// process-wide and cannot be set safely from a parallel test.
    #[cfg(test)]
    pub fn ordered(credentials: Vec<Credential>) -> Self {
        assert!(!credentials.is_empty());
        Self(credentials)
    }

    /// Every credential to try, in order. Never empty: [`credentials`] fails
    /// rather than returning nothing to present.
    pub fn all(&self) -> &[Credential] {
        &self.0
    }

    pub fn primary(&self) -> &Credential {
        &self.0[0]
    }

    /// Names every credential that would be presented, for the message after
    /// they have all been rejected.
    pub fn describe_all(&self) -> String {
        let names: Vec<&str> = self.0.iter().map(|c| c.source.describe()).collect();
        match names.split_last() {
            Some((last, [])) => (*last).to_owned(),
            Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
            None => String::new(),
        }
    }
}

fn stored_token() -> Option<String> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT).ok()?;
    entry.get_password().ok().filter(|value| !value.is_empty())
}

fn env_token() -> Option<String> {
    std::env::var(ENV_VAR)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Orders what was found. Split from the lookups so the precedence rule can be
/// tested without a credential store or a process-wide environment variable.
///
/// `Ok(None)` is "nothing to present", which `bbs auth status` reports as a
/// state rather than a failure; an `Err` is a real fault, such as
/// `--env-token` with nothing in the environment.
fn order(
    stored: Option<String>,
    env: Option<String>,
    prefer_env: bool,
) -> Result<Option<Credentials>> {
    if prefer_env && env.is_none() {
        bail!("--env-token was passed but {ENV_VAR} is unset or empty");
    }
    let saved = stored.map(|token| Credential {
        source: Source::Saved,
        token,
    });
    let environment = env.map(|token| Credential {
        source: Source::Environment,
        token,
    });
    let ordered = if prefer_env {
        environment.into_iter().chain(saved)
    } else {
        saved.into_iter().chain(environment)
    };
    // The two are often the same string. Presenting it twice would only double
    // the round trips before the identical rejection.
    let mut candidates: Vec<Credential> = Vec::with_capacity(2);
    for candidate in ordered {
        if !candidates.iter().any(|kept| kept.token == candidate.token) {
            candidates.push(candidate);
        }
    }
    if candidates.is_empty() {
        return Ok(None);
    }
    Ok(Some(Credentials(candidates)))
}

/// The credentials for this run, or `None` when there are none to present.
/// `prefer_env` is `--env-token`.
pub fn lookup(prefer_env: bool) -> Result<Option<Credentials>> {
    order(stored_token(), env_token(), prefer_env)
}

/// The credentials for this run, refusing to continue without one.
pub fn credentials(prefer_env: bool) -> Result<Credentials> {
    lookup(prefer_env)?.ok_or_else(|| anyhow::anyhow!(NOT_LOGGED_IN))
}

/// Named because `bbs auth status` prints it too, and the two must not drift.
pub const NOT_LOGGED_IN: &str = "not logged in; run `bbs login` or set BB_TOKEN";

pub fn store_token(value: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT)
        .map_err(|error| anyhow::anyhow!("cannot access system credential store: {error}"))?;
    entry
        .set_password(value)
        .map_err(|error| anyhow::anyhow!("failed to save token in the credential store: {error}"))
}

pub fn delete_token() -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT)
        .map_err(|error| anyhow::anyhow!("cannot access system credential store: {error}"))?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(anyhow::anyhow!("failed to remove credential: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(credentials: &Credentials) -> Vec<(Source, &str)> {
        credentials
            .all()
            .iter()
            .map(|c| (c.source, c.token.as_str()))
            .collect()
    }

    /// The saved credential wins. A `BB_TOKEN` left in a shell profile used to
    /// shadow it outright, so `bbs login` appeared to do nothing.
    #[test]
    fn a_saved_credential_outranks_the_environment() {
        let credentials = order(Some("saved".into()), Some("env".into()), false)
            .unwrap()
            .unwrap();
        assert_eq!(
            tokens(&credentials),
            [(Source::Saved, "saved"), (Source::Environment, "env")]
        );
        assert_eq!(credentials.primary().token, "saved");
    }

    /// ...but it is a fallback, not a rival: it is what an account that has
    /// never run `bbs login` searches with, and what a saved credential that
    /// Bitbucket has started rejecting falls through to.
    #[test]
    fn the_environment_is_the_fallback_and_the_only_credential_when_alone() {
        assert_eq!(
            tokens(&order(None, Some("env".into()), false).unwrap().unwrap()),
            [(Source::Environment, "env")]
        );
        assert_eq!(
            tokens(&order(Some("saved".into()), None, false).unwrap().unwrap()),
            [(Source::Saved, "saved")]
        );
    }

    #[test]
    fn env_token_asks_for_the_environment_first() {
        assert_eq!(
            tokens(
                &order(Some("saved".into()), Some("env".into()), true)
                    .unwrap()
                    .unwrap()
            ),
            [(Source::Environment, "env"), (Source::Saved, "saved")]
        );
    }

    /// `--env-token` with nothing in the environment must say so rather than
    /// quietly searching with the saved credential it was asked to bypass.
    #[test]
    fn env_token_without_the_variable_is_an_error() {
        let error = order(Some("saved".into()), None, true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("BB_TOKEN"), "{error}");
    }

    #[test]
    fn one_token_in_both_places_is_presented_once() {
        let credentials = order(Some("same".into()), Some("same".into()), false)
            .unwrap()
            .unwrap();
        assert_eq!(tokens(&credentials), [(Source::Saved, "same")]);
    }

    /// Nothing saved is a *state*, not a fault: `bbs auth status` has to be
    /// able to report it without an error, while a search still refuses to run.
    #[test]
    fn no_credential_at_all_is_absence_rather_than_failure() {
        assert!(order(None, None, false).unwrap().is_none());
        assert!(
            NOT_LOGGED_IN.contains("bbs login") && NOT_LOGGED_IN.contains(ENV_VAR),
            "{NOT_LOGGED_IN}"
        );
    }

    #[test]
    fn a_rejection_can_name_everything_it_presented() {
        assert_eq!(
            order(Some("saved".into()), Some("env".into()), false)
                .unwrap()
                .unwrap()
                .describe_all(),
            "the saved credential and BB_TOKEN"
        );
        assert_eq!(
            Credentials::supplied("t").describe_all(),
            "the supplied token"
        );
    }
}
