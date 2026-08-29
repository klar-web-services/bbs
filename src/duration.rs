//! Duration parsing for `--max-age`.
//!
//! A full-corpus search fetches every selected repository before scanning,
//! which is right by default but dominates latency when iterating on a query.
//! `--offline` is too blunt an escape: it marks everything stale and silently
//! misses any repository that was never synced. A freshness window is the
//! middle ground.

use anyhow::{Result, bail};

/// Parses `5m`, `1h30m`, `90s`, `2d`, or a bare number of seconds.
pub fn parse_duration_secs(text: &str) -> Result<u64> {
    let text = text.trim();
    if text.is_empty() {
        bail!("a duration cannot be empty; try `5m`, `1h` or `30s`");
    }
    let mut total: u64 = 0;
    let mut digits = String::new();
    let mut saw_unit = false;
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        if digits.is_empty() {
            bail!("`{text}` is not a duration; try `5m`, `1h30m` or `30s`");
        }
        let unit = match ch {
            's' => 1,
            'm' => 60,
            'h' => 60 * 60,
            'd' => 24 * 60 * 60,
            other => bail!("unknown duration unit `{other}` in `{text}`; use s, m, h or d"),
        };
        let value: u64 = digits
            .parse()
            .map_err(|_| anyhow::anyhow!("`{text}` is too large to be a duration"))?;
        total = total.saturating_add(value.saturating_mul(unit));
        digits.clear();
        saw_unit = true;
    }
    if !digits.is_empty() {
        // A bare number is seconds, which is what a script is most likely to
        // interpolate.
        let value: u64 = digits
            .parse()
            .map_err(|_| anyhow::anyhow!("`{text}` is too large to be a duration"))?;
        total = total.saturating_add(value);
    } else if !saw_unit {
        bail!("`{text}` is not a duration; try `5m`, `1h30m` or `30s`");
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units_and_combinations() {
        assert_eq!(parse_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_duration_secs("5m").unwrap(), 300);
        assert_eq!(parse_duration_secs("1h").unwrap(), 3600);
        assert_eq!(parse_duration_secs("2d").unwrap(), 172_800);
        assert_eq!(parse_duration_secs("1h30m").unwrap(), 5400);
        assert_eq!(parse_duration_secs("1d2h3m4s").unwrap(), 93_784);
        // a bare number is seconds
        assert_eq!(parse_duration_secs("90").unwrap(), 90);
        assert_eq!(parse_duration_secs("0").unwrap(), 0);
        assert_eq!(parse_duration_secs(" 5m ").unwrap(), 300);
    }

    #[test]
    fn refuses_what_is_not_a_duration() {
        for source in ["", "   ", "m", "5x", "abc", "-5m", "5 m"] {
            assert!(
                parse_duration_secs(source).is_err(),
                "`{source}` should be refused"
            );
        }
    }
}
