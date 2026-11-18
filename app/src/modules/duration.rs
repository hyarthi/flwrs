//! Durations in config files: a whole number and a unit.

use std::time::Duration;

/// `90s`, `30m`, `6h`, `1d`, `2w`.
pub(crate) fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("{text:?} has no unit (s, m, h, d, w)"))?;
    let (number, unit) = text.split_at(split);
    let n: u64 = number
        .parse()
        .map_err(|_| format!("{text:?} doesn't start with a whole number"))?;
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(format!("{text:?}: unknown unit {unit:?} (s, m, h, d, w)")),
    };
    n.checked_mul(seconds)
        .filter(|&s| s > 0)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("{text:?} is out of range"))
}
