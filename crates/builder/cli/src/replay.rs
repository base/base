use std::time::{Duration, SystemTime};

use base_clock::{GlobalClock, OffsetClock};
use eyre::{OptionExt, WrapErr};

/// Builder transaction replay clock configuration.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Replay")]
pub struct ReplayArgs {
    /// Start wall time at this Unix epoch in milliseconds, advancing at real speed.
    /// Omit to retain the system clock.
    #[arg(long = "replay.clock-start-ms", value_name = "U64")]
    pub clock_start_ms: Option<u64>,
}

impl ReplayArgs {
    /// Installs the replay clock before starting runtimes or node components.
    /// Does nothing when the flag is absent.
    ///
    /// # Errors
    /// Returns an error if the epoch is unrepresentable or a global clock was already installed.
    pub fn install_clock(&self) -> eyre::Result<()> {
        if let Some(ms) = self.clock_start_ms {
            let start = SystemTime::UNIX_EPOCH
                .checked_add(Duration::from_millis(ms))
                .ok_or_eyre("--replay.clock-start-ms is outside the supported system time range")?;
            GlobalClock::set(OffsetClock::new(start))
                .wrap_err("failed to install --replay.clock-start-ms")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::ReplayArgs;

    #[derive(Debug, Parser)]
    struct CommandParser {
        #[command(flatten)]
        replay: ReplayArgs,
    }

    #[test]
    fn clock_start_is_absent_when_flag_is_omitted() {
        let parsed = CommandParser::try_parse_from(["builder"]).unwrap();

        assert_eq!(parsed.replay.clock_start_ms, None);
    }

    #[test]
    fn clock_start_is_parsed_when_flag_is_present() {
        let parsed =
            CommandParser::try_parse_from(["builder", "--replay.clock-start-ms", "1700000000123"])
                .unwrap();

        assert_eq!(parsed.replay.clock_start_ms, Some(1_700_000_000_123));
    }

    #[test]
    fn clock_start_accepts_zero_when_epoch_is_requested() {
        let parsed =
            CommandParser::try_parse_from(["builder", "--replay.clock-start-ms", "0"]).unwrap();

        assert_eq!(parsed.replay.clock_start_ms, Some(0));
    }

    #[test]
    fn clock_start_is_rejected_when_value_is_not_u64() {
        for value in ["-1", "18446744073709551616", "invalid"] {
            let parsed =
                CommandParser::try_parse_from(["builder", "--replay.clock-start-ms", value]);

            assert!(parsed.is_err(), "accepted {value}");
        }
    }
}
