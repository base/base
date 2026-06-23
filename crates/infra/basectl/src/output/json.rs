//! JSON output for non-TUI commands.

use std::io::{self, Write};

use anyhow::{Result, ensure};
use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::Serialize;

/// JSON writer for non-TUI command output.
#[derive(Debug, Default, Clone, Copy)]
pub struct JsonOutput;

impl JsonOutput {
    /// Pretty-prints `value` as JSON to stdout, terminating with a newline.
    pub fn print<T: Serialize>(value: &T) -> Result<()> {
        let mut stdout = io::stdout().lock();
        Self::write(&mut stdout, value)?;
        Ok(())
    }

    /// Pretty-prints `value` as JSON to `writer`, terminating with a newline.
    pub fn write<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<()> {
        serde_json::to_writer_pretty(&mut *writer, value)?;
        writer.write_all(b"\n")?;
        Ok(())
    }
}

/// Three-form timestamp object for `--json` command output: raw unix
/// seconds, UTC RFC 3339, and local RFC 3339 (operator's machine timezone
/// with offset suffix).
///
/// Shared by every non-TUI subcommand that surfaces a Unix timestamp in
/// its humanized JSON shape — saves consumers from re-deriving wall-clock
/// time from raw seconds.
#[derive(Debug, Clone, Serialize)]
pub struct TimestampJson {
    /// Raw Unix seconds since epoch.
    pub unix: u64,
    /// RFC 3339 UTC string (e.g. `2026-06-04T18:00:00Z`).
    pub utc: String,
    /// RFC 3339 string in the operator's local timezone with offset suffix.
    pub local: String,
}

impl TimestampJson {
    /// Builds a `TimestampJson` from raw Unix seconds.
    ///
    /// Values above `i64::MAX` (impossible for real chain timestamps —
    /// year 9999 is still ~3 orders of magnitude under) fall back to the
    /// raw decimal in both `utc` and `local`, instead of silently
    /// wrapping to a fabricated pre-epoch RFC 3339 string under an
    /// `as i64` cast.
    pub fn from_unix(secs: u64) -> Self {
        let dt = i64::try_from(secs).ok().and_then(|s| DateTime::from_timestamp(s, 0));
        let utc = dt
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
            .unwrap_or_else(|| secs.to_string());
        let local = dt
            .map(|t| t.with_timezone(&Local).to_rfc3339_opts(SecondsFormat::Secs, false))
            .unwrap_or_else(|| secs.to_string());
        Self { unix: secs, utc, local }
    }

    /// Tries to build a `TimestampJson` from a UTC timestamp.
    ///
    /// Pre-epoch timestamps are rejected instead of being coerced to Unix zero,
    /// which would make the raw timestamp disagree with the rendered dates.
    pub fn try_from_datetime_utc(dt: DateTime<Utc>) -> Result<Self> {
        let timestamp = dt.timestamp();
        ensure!(timestamp >= 0, "UTC timestamp {dt} is before the Unix epoch");
        Ok(Self::from_unix(timestamp as u64))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::{JsonOutput, TimestampJson};

    #[test]
    fn write_emits_pretty_json_with_trailing_newline() {
        let mut buf = Vec::new();
        JsonOutput::write(&mut buf, &json!({"k": 1})).unwrap();
        let rendered = String::from_utf8(buf).unwrap();

        assert_eq!(rendered, "{\n  \"k\": 1\n}\n");
    }

    #[test]
    fn timestamp_json_renders_three_forms() {
        let ts = TimestampJson::from_unix(1_780_614_804);

        assert_eq!(ts.unix, 1_780_614_804);
        assert!(ts.utc.ends_with('Z'), "expected UTC suffix Z, got {}", ts.utc);
        assert!(ts.utc.starts_with("2026-06-04"), "expected UTC date prefix, got {}", ts.utc);
        let local_has_offset =
            ts.local.contains('+') || ts.local.matches('-').count() >= 3 || ts.local.ends_with('Z');
        assert!(local_has_offset, "expected local RFC 3339 with offset, got {}", ts.local);
    }

    #[test]
    fn timestamp_json_falls_back_on_u64_overflow() {
        // u64 values above i64::MAX would silently wrap to a negative i64 under
        // an `as i64` cast, producing a misleading pre-epoch RFC 3339 string.
        // The try_from guard converts that case to None, triggering the raw
        // seconds fallback for both utc and local.
        let oversize = TimestampJson::from_unix(u64::MAX);

        assert_eq!(oversize.unix, u64::MAX);
        assert_eq!(oversize.utc, u64::MAX.to_string());
        assert_eq!(oversize.local, u64::MAX.to_string());
    }

    #[test]
    fn timestamp_json_renders_utc_datetime() {
        let dt = Utc.with_ymd_and_hms(2026, 6, 4, 18, 0, 4).single().unwrap();

        let ts = TimestampJson::try_from_datetime_utc(dt).unwrap();

        assert_eq!(ts.unix, 1_780_596_004);
        assert_eq!(ts.utc, "2026-06-04T18:00:04Z");
    }

    #[test]
    fn timestamp_json_rejects_pre_epoch_datetime() {
        let dt = Utc.with_ymd_and_hms(1969, 12, 31, 23, 59, 59).single().unwrap();

        let error = TimestampJson::try_from_datetime_utc(dt).unwrap_err();

        assert!(error.to_string().contains("before the Unix epoch"));
    }
}
