//! Size parsing and formatting for the file-size limit.
//!
//! `max_file_bytes` was only ever spellable as a raw byte count in
//! `config.toml`, which made the one knob that decides whether a file is
//! searched at all both invisible and awkward to change. A search that silently
//! skips the generated file you are looking for is worse than a slow one, so
//! the limit is a flag as well as a setting, and it is written the way people
//! say it: `--max-file-size 32M`.

use anyhow::{Result, bail};
use serde::Deserialize;

/// Parses `512k`, `4M`, `1.5G`, a bare byte count, or one of the words that
/// mean "do not skip anything".
///
/// Units are binary: `k` is 1024 bytes, and `kb`/`kib` are the same as `k`.
pub fn parse_size(text: &str) -> Result<u64> {
    let text = text.trim();
    if text.is_empty() {
        bail!("a size cannot be empty; try `4M`, `512k` or `none`");
    }
    let lowered = text.to_ascii_lowercase();
    // A limit of zero would skip every file, which nobody means by it. Spell
    // the escape hatch several ways rather than making people guess which.
    if matches!(lowered.as_str(), "0" | "none" | "unlimited" | "inf") {
        return Ok(u64::MAX);
    }
    let split = lowered
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .unwrap_or(lowered.len());
    let (number, unit) = lowered.split_at(split);
    let multiplier: u64 = match unit.trim() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        other => bail!("unknown size unit `{other}` in `{text}`; use b, k, m or g"),
    };
    let value: f64 = number
        .parse()
        .map_err(|_| anyhow::anyhow!("`{text}` is not a size; try `4M`, `512k` or `none`"))?;
    let bytes = value * multiplier as f64;
    if bytes >= u64::MAX as f64 {
        bail!("`{text}` is too large to be a size; use `none` for no limit");
    }
    let bytes = bytes as u64;
    if bytes == 0 {
        bail!("`{text}` rounds to zero bytes, which would skip every file");
    }
    Ok(bytes)
}

/// The same size read back, for a summary line that names the limit it applied.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} B"),
        _ => format!("{value:.1} {}", UNITS[unit]),
    }
}

/// Lets `config.toml` say `max_file_bytes = "32M"` as well as
/// `max_file_bytes = 33554432`. The byte count is what the code wants; the
/// unit suffix is what a person writing the file wants.
pub fn deserialize_size<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Bytes(u64),
        Text(String),
    }
    match Repr::deserialize(deserializer)? {
        // Zero means the same thing here as it does on the command line.
        Repr::Bytes(0) => Ok(u64::MAX),
        Repr::Bytes(bytes) => Ok(bytes),
        Repr::Text(text) => parse_size(&text).map_err(serde::de::Error::custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units_case_insensitively() {
        assert_eq!(parse_size("4M").unwrap(), 4 * 1024 * 1024);
        assert_eq!(parse_size("4mb").unwrap(), 4 * 1024 * 1024);
        assert_eq!(parse_size("4MiB").unwrap(), 4 * 1024 * 1024);
        assert_eq!(parse_size("512k").unwrap(), 512 * 1024);
        assert_eq!(parse_size("2G").unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn a_bare_number_is_bytes() {
        assert_eq!(parse_size("1048576").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("900b").unwrap(), 900);
    }

    #[test]
    fn fractions_are_allowed_because_people_write_them() {
        assert_eq!(parse_size("1.5M").unwrap(), 1024 * 1024 + 512 * 1024);
    }

    #[test]
    fn surrounding_whitespace_is_not_an_error() {
        assert_eq!(parse_size(" 4M ").unwrap(), 4 * 1024 * 1024);
    }

    /// Zero is the one number that cannot mean what it says: a zero-byte limit
    /// skips every file, so it is read as "no limit" instead.
    #[test]
    fn zero_and_its_synonyms_mean_no_limit() {
        for text in ["0", "none", "unlimited", "INF"] {
            assert_eq!(parse_size(text).unwrap(), u64::MAX, "{text}");
        }
    }

    #[test]
    fn rejects_nonsense() {
        for text in ["", "  ", "M", "4x", "-1", "4 M B", "0.0001b"] {
            assert!(parse_size(text).is_err(), "accepted `{text}`");
        }
    }

    #[test]
    fn a_setting_may_be_a_count_or_a_phrase() {
        #[derive(Deserialize)]
        struct Held {
            #[serde(deserialize_with = "deserialize_size")]
            size: u64,
        }

        let count: Held = toml::from_str("size = 1048576").unwrap();
        assert_eq!(count.size, 1024 * 1024);
        let phrase: Held = toml::from_str(r#"size = "32M""#).unwrap();
        assert_eq!(phrase.size, 32 * 1024 * 1024);
        let none: Held = toml::from_str(r#"size = "none""#).unwrap();
        assert_eq!(none.size, u64::MAX);
        let zero: Held = toml::from_str("size = 0").unwrap();
        assert_eq!(zero.size, u64::MAX);
        assert!(toml::from_str::<Held>(r#"size = "4 furlongs""#).is_err());
    }

    #[test]
    fn formats_bytes_for_people() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
