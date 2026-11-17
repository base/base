//! Explicit admission policy for starting a node at genesis.

use std::{fmt, sync::Once};

use clap::Args;

static GENESIS_START_WARNING: Once = Once::new();

/// Explicit opt-in for launching a new chain at block zero.
#[derive(Args, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GenesisStartArgs {
    /// Allow starting at genesis only for launching a new chain. Existing chains must restore a snapshot instead.
    #[arg(long = "allow-genesis-start")]
    pub allow_genesis_start: bool,
}

impl GenesisStartArgs {
    /// Refuses a genesis head without explicit launch permission and warns once when opted in.
    pub fn check(&self, head: u64, genesis: u64) -> Result<(), GenesisStartError> {
        if self.allow_genesis_start {
            GENESIS_START_WARNING.call_once(|| {
                tracing::warn!(
                    "--allow-genesis-start is for launching a new chain; existing chains should restore a snapshot instead"
                );
            });
        } else if head == genesis {
            return Err(GenesisStartError);
        }
        Ok(())
    }
}

/// Starting from genesis requires explicit permission to launch a new chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenesisStartError;

impl fmt::Display for GenesisStartError {
    /// Gives operators snapshot recovery instructions and the new-chain launch option.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "refusing to start from genesis; restore a snapshot for an existing chain; use --allow-genesis-start only for launching a new chain",
        )
    }
}

impl std::error::Error for GenesisStartError {}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

    use tracing::{Event, Subscriber};
    use tracing_subscriber::{Layer, layer::Context, prelude::*};

    use super::GenesisStartArgs;
    use clap::Parser;

    struct WarningCounter(Arc<AtomicUsize>);

    impl<S: Subscriber> Layer<S> for WarningCounter {
        /// Counts warnings emitted by the admission policy in an isolated process.
        fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Repeated EL and CL checks must emit one warning, even when the first head is nonzero.
    #[test]
    fn warns_once_for_repeated_checks() {
        const CHILD: &str = "BASE_GENESIS_WARNING_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().expect("test executable must exist"))
                .args(["--exact", "genesis::tests::warns_once_for_repeated_checks", "--nocapture"])
                .env(CHILD, "1")
                .output().expect("the isolated warning test must run");
            assert_eq!(output.status.success(), true, "the isolated warning test must pass: {}", String::from_utf8_lossy(&output.stdout));
            return;
        }
        let warnings = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry().with(WarningCounter(Arc::clone(&warnings)));
        tracing::subscriber::with_default(subscriber, || {
            let args = GenesisStartArgs { allow_genesis_start: true };
            for head in [1, 0, 2, 0] {
                assert_eq!(args.check(head, 0), Ok(()), "the launch flag must admit head {head}");
            }
        });
        assert_eq!(warnings.load(Ordering::Relaxed), 1, "the launch flag must warn exactly once per process");
    }

    #[derive(Parser)]
    struct Command {
        #[command(flatten)]
        genesis: GenesisStartArgs,
    }

    /// Genesis startup must fail with snapshot recovery instructions unless explicitly allowed.
    #[test]
    fn refuses_genesis_without_launch_flag() {
        let args = Command::parse_from(["node"]).genesis;
        let error = args.check(0, 0).expect_err("genesis must require the explicit launch flag");
        assert_eq!(
            error.to_string(),
            "refusing to start from genesis; restore a snapshot for an existing chain; use --allow-genesis-start only for launching a new chain",
            "the refusal must give exact snapshot recovery and new-chain launch instructions"
        );
    }

    /// Custom genesis heights require the same explicit launch permission.
    #[test]
    fn refuses_custom_genesis_without_launch_flag() {
        assert_eq!(GenesisStartArgs::default().check(1_000, 1_000), Err(super::GenesisStartError),
            "a custom nonzero genesis height must not bypass admission");
        assert_eq!(GenesisStartArgs::default().check(1_001, 1_000), Ok(()),
            "the first non-genesis head must start without the launch flag");
    }

    /// The launch flag must permit a new chain to start at block zero.
    #[test]
    fn permits_genesis_with_launch_flag() {
        let args = Command::parse_from(["node", "--allow-genesis-start"]).genesis;
        assert_eq!(args.check(0, 0), Ok(()), "the launch flag must allow block zero");
    }

    /// Existing heads must remain usable on both sides of the genesis boundary with either flag value.
    #[test]
    fn permits_non_genesis_with_or_without_launch_flag() {
        for allow_genesis_start in [false, true] {
            let args = GenesisStartArgs { allow_genesis_start };
            for head in [1, u64::MAX] {
                assert_eq!(
                    args.check(head, 0).is_ok(),
                    true,
                    "non-genesis head {head} must start unchanged with flag {allow_genesis_start}"
                );
            }
        }
    }
}
