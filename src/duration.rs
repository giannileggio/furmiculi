//! Durations as furmi writes them: `5m` in config.toml, `1h 30m` in a message.
//!
//! This replaces the `humantime` and `humantime-serde` crates, which went
//! unmaintained when chrono did (RUSTSEC-2026-0542). The grammar is the one
//! furmi's config, flags and messages already use, and the output is the same
//! text humantime produced, so existing config.toml files and tooling around
//! them keep working.

use std::time::Duration;

/// Nanoseconds in a second, to keep the arithmetic in `u128`.
const NANOS: u128 = 1_000_000_000;

/// Units `parse` understands, spelled the way humantime spelled them: whole
/// words, case-sensitive (`m` is a minute and `M` is a month), singular and
/// plural. Ordered longest name first, so a lookup can stop early.
const UNITS: &[(&str, u128)] = &[
    ("nanoseconds", 1),
    ("nanosecond", 1),
    ("nsecs", 1),
    ("nsec", 1),
    ("ns", 1),
    ("microseconds", 1_000),
    ("microsecond", 1_000),
    ("usecs", 1_000),
    ("usec", 1_000),
    ("us", 1_000),
    ("µs", 1_000),
    ("milliseconds", 1_000_000),
    ("millisecond", 1_000_000),
    ("msecs", 1_000_000),
    ("msec", 1_000_000),
    ("ms", 1_000_000),
    ("seconds", NANOS),
    ("second", NANOS),
    ("secs", NANOS),
    ("sec", NANOS),
    ("s", NANOS),
    ("minutes", 60 * NANOS),
    ("minute", 60 * NANOS),
    ("mins", 60 * NANOS),
    ("min", 60 * NANOS),
    ("m", 60 * NANOS),
    ("hours", 3_600 * NANOS),
    ("hour", 3_600 * NANOS),
    ("hrs", 3_600 * NANOS),
    ("hr", 3_600 * NANOS),
    ("h", 3_600 * NANOS),
    ("days", 86_400 * NANOS),
    ("day", 86_400 * NANOS),
    ("d", 86_400 * NANOS),
    ("weeks", 604_800 * NANOS),
    ("week", 604_800 * NANOS),
    ("wk", 604_800 * NANOS),
    ("w", 604_800 * NANOS),
    ("months", 2_630_016 * NANOS),
    ("month", 2_630_016 * NANOS),
    ("M", 2_630_016 * NANOS),
    ("years", 31_557_600 * NANOS),
    ("year", 31_557_600 * NANOS),
    ("y", 31_557_600 * NANOS),
];

/// Units `format` writes, longest first. A year is 365.25 days and a month is
/// a twelfth of that, the way humantime counted them, so `1y` reads back the
/// same both ways.
const WRITTEN: &[(u128, &str, &str)] = &[
    (31_557_600 * NANOS, "year", "years"),
    (2_630_016 * NANOS, "month", "months"),
    (86_400 * NANOS, "day", "days"),
    (3_600 * NANOS, "h", "h"),
    (60 * NANOS, "m", "m"),
    (NANOS, "s", "s"),
    (1_000_000, "ms", "ms"),
    (1_000, "us", "us"),
    (1, "ns", "ns"),
];

/// Reads `5m`, `1h 30m`, `1.5h`, `2 days`, `1d 4h 30m 15s`. A bare `0` is
/// zero; any other number needs its unit.
pub fn parse(text: &str) -> Result<Duration, String> {
    let s = text.trim();
    if s.is_empty() {
        return Err("empty duration: give one like `5m` or `1h 30m`".to_string());
    }
    let bytes = s.as_bytes();
    let mut at = 0;
    let mut nanos: u128 = 0;
    let mut groups = 0;
    while at < bytes.len() {
        while at < bytes.len() && bytes[at] == b' ' {
            at += 1;
        }
        if at == bytes.len() {
            break;
        }
        // The number: digits, and a fractional part after a dot.
        let number_start = at;
        while at < bytes.len() && (bytes[at].is_ascii_digit() || bytes[at] == b'.') {
            at += 1;
        }
        let number = &s[number_start..at];
        if number.is_empty() {
            return Err(format!("expected a number at `{}`", &s[at..]));
        }
        let value: f64 = number
            .parse()
            .map_err(|_| format!("`{number}` isn't a number"))?;
        // The unit: everything up to the next number or space.
        while at < bytes.len() && bytes[at] == b' ' {
            at += 1;
        }
        let unit_start = at;
        while at < bytes.len() && !bytes[at].is_ascii_digit() && bytes[at] != b' ' {
            at += 1;
        }
        let unit = &s[unit_start..at];
        let size = match unit_size(unit) {
            Some(size) => size,
            // `0` is zero seconds, the one unitless duration humantime took.
            None if unit.is_empty() && value == 0.0 => 1,
            None => {
                return Err(format!(
                    "unknown time unit `{unit}`: use ns, us, ms, s, m, h, d, w, M or y"
                ));
            }
        };
        nanos = nanos
            .checked_add((value * size as f64) as u128)
            .ok_or("that duration is too large")?;
        groups += 1;
    }
    if groups == 0 {
        return Err("empty duration: give one like `5m` or `1h 30m`".to_string());
    }
    let secs = u64::try_from(nanos / NANOS).map_err(|_| "that duration is too large")?;
    Ok(Duration::new(secs, (nanos % NANOS) as u32))
}

fn unit_size(unit: &str) -> Option<u128> {
    UNITS
        .iter()
        .find(|(name, _)| *name == unit)
        .map(|(_, size)| *size)
}

/// Writes a duration the way a person reads it: `1h 30m`, `2days`, `0s`.
pub fn format(d: Duration) -> String {
    let mut left = d.as_nanos();
    let mut parts: Vec<String> = Vec::new();
    for (size, one, many) in WRITTEN {
        let count = left / size;
        if count == 0 {
            continue;
        }
        left %= size;
        let name = if count == 1 { one } else { many };
        parts.push(format!("{count}{name}"));
    }
    if parts.is_empty() {
        "0s".to_string()
    } else {
        parts.join(" ")
    }
}

/// `Duration` as a string in a config file: `poll_interval = "2s"`.
pub mod serde_duration {
    use super::{format, parse};
    use serde::{Deserialize, Deserializer, Serializer, de};
    use std::time::Duration;

    /// Only ever read today: furmi writes config.toml by editing the text, not
    /// by serialising these structs. Kept beside `deserialize` so the pair is
    /// there if that changes.
    #[allow(dead_code)]
    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format(*d))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let text = String::deserialize(d)?;
        parse(&text).map_err(de::Error::custom)
    }
}

/// The same, for a field that may be absent: `timeout = "30m"` or nothing.
pub mod serde_optional {
    use super::{format, parse};
    use serde::{Deserialize, Deserializer, Serializer, de};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(d) => s.serialize_str(&format(*d)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(text) => parse(&text).map(Some).map_err(de::Error::custom),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shapes_furmi_writes_parse() {
        for (text, secs) in [
            ("0", 0.0),
            ("0s", 0.0),
            ("30s", 30.0),
            ("5m", 300.0),
            ("5 min", 300.0),
            ("1h", 3600.0),
            ("1 h", 3600.0),
            ("1hour", 3600.0),
            ("1h30m", 5400.0),
            ("1h 30m", 5400.0),
            ("2 days", 172_800.0),
            ("2days", 172_800.0),
            ("1d 4h 30m 15s", 102_615.0),
            ("1.5h", 5400.0),
            ("3m 05s", 185.0),
            ("1w", 604_800.0),
            ("1M", 2_630_016.0),
            ("1y", 31_557_600.0),
            ("500ms", 0.5),
            ("250us", 0.000_25),
        ] {
            let got = parse(text).unwrap_or_else(|e| panic!("{text:?}: {e}"));
            assert!(
                (got.as_secs_f64() - secs).abs() < 1e-9,
                "{text:?} is {}s, wanted {secs}s",
                got.as_secs_f64()
            );
        }
    }

    #[test]
    fn what_isnt_a_duration_says_why() {
        for text in [
            "", "   ", "1", "1h30", "abc", "-1h", "1.5.5h", "1h, 30m", "1H",
        ] {
            assert!(parse(text).is_err(), "{text:?} should not parse");
        }
        assert!(parse("60x").unwrap_err().contains("unknown time unit `x`"));
    }

    #[test]
    fn it_reads_back_what_it_writes() {
        for secs in [
            0u64, 1, 45, 60, 90, 125, 3600, 3660, 5400, 86_400, 90_061, 604_800,
        ] {
            let d = Duration::from_secs(secs);
            let text = format(d);
            assert_eq!(parse(&text).unwrap(), d, "{secs}s wrote {text:?}");
        }
        assert_eq!(format(Duration::from_millis(1500)), "1s 500ms");
        assert_eq!(parse("1s 500ms").unwrap(), Duration::from_millis(1500));
    }

    #[test]
    fn it_writes_them_the_way_humantime_did() {
        // The texts furmi's messages already show; changing one changes output.
        for (secs, text) in [
            (0, "0s"),
            (1, "1s"),
            (59, "59s"),
            (60, "1m"),
            (61, "1m 1s"),
            (125, "2m 5s"),
            (3600, "1h"),
            (3661, "1h 1m 1s"),
            (5400, "1h 30m"),
            (86_400, "1day"),
            (90_061, "1day 1h 1m 1s"),
            (604_800, "7days"),
            (2_592_000, "30days"),
            (31_557_600, "1year"),
        ] {
            assert_eq!(format(Duration::from_secs(secs)), text, "for {secs}s");
        }
    }

    #[test]
    fn a_config_field_reads_and_writes_the_same_text() {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct T {
            #[serde(with = "serde_duration")]
            delay: Duration,
            #[serde(
                default,
                with = "serde_optional",
                skip_serializing_if = "Option::is_none"
            )]
            timeout: Option<Duration>,
        }
        let read: T = toml::from_str("delay = \"5m\"\ntimeout = \"30m\"").unwrap();
        assert_eq!(read.delay, Duration::from_secs(300));
        assert_eq!(read.timeout, Some(Duration::from_secs(1800)));
        let written = toml::to_string(&read).unwrap();
        assert!(written.contains("delay = \"5m\""), "{written}");
        assert!(written.contains("timeout = \"30m\""), "{written}");
        let absent: T = toml::from_str("delay = \"5m\"").unwrap();
        assert_eq!(absent.timeout, None);
        assert!(!toml::to_string(&absent).unwrap().contains("timeout"));
    }
}
