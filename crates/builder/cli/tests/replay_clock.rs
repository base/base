//! Isolated process-wide replay clock lifecycle coverage.

use std::time::{Duration, Instant, SystemTime};

use base_builder_cli::Args;
use base_clock::GlobalClock;
use base_execution_cli::{Cli, commands::Commands};
use clap::Parser;

// One lifecycle per process: GlobalClock deliberately has no reset API.
#[test]
fn replay_clock_is_installed_only_when_requested() {
    let Commands::Node(default) = Cli::<Args>::try_parse_from(["builder", "node"]).unwrap().command
    else {
        panic!("expected node command");
    };
    let before = SystemTime::now();

    default.ext.replay.install_clock().unwrap();

    let default_now = GlobalClock::get().now();
    assert!((before..=SystemTime::now()).contains(&default_now));

    let Commands::Node(replay) = Cli::<Args>::try_parse_from([
        "builder",
        "node",
        "--replay.clock-start-ms",
        "1700000000123",
    ])
    .unwrap()
    .command
    else {
        panic!("expected node command");
    };
    let start = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
    let before_installation = Instant::now();

    replay.ext.replay.install_clock().unwrap();

    let installed_now = GlobalClock::get().now();
    assert!((start..=start + before_installation.elapsed()).contains(&installed_now));

    let error = replay.ext.replay.install_clock().unwrap_err();

    assert!(error.chain().any(|cause| cause.is::<base_clock::SetClockError>()));
    let retained_now = GlobalClock::get().now();
    assert!((installed_now..=start + before_installation.elapsed()).contains(&retained_now));
}
