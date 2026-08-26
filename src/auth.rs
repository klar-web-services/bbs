use anyhow::{Context, Result, bail};

const SERVICE: &str = "better-bitbucket-search";
const ACCOUNT: &str = "default";

pub fn token() -> Result<String> {
    if let Ok(value) = std::env::var("BB_TOKEN") {
        let value = value.trim().to_owned();
        if !value.is_empty() {
            return Ok(value);
        }
    }
    let entry =
        keyring::Entry::new(SERVICE, ACCOUNT).context("cannot access system credential store")?;
    match entry.get_password() {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => bail!("not logged in; run `bbs login` or set BB_TOKEN"),
    }
}

pub fn store_token(value: &str) -> Result<()> {
    let entry =
        keyring::Entry::new(SERVICE, ACCOUNT).context("cannot access system credential store")?;
    entry
        .set_password(value)
        .context("failed to save token in the system credential store")
}

pub fn delete_token() -> Result<()> {
    let entry =
        keyring::Entry::new(SERVICE, ACCOUNT).context("cannot access system credential store")?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error).context("failed to remove credential"),
    }
}
