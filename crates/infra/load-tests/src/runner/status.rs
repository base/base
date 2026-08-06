use std::{
    io::{self, IsTerminal},
    time::Duration,
};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use tracing_indicatif::{IndicatifWriter, writer::Stderr};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::{BaselineError, Result};

/// Bounded lifecycle stage shown in progress output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LoadTestStage {
    /// Preparing accounts and chain state.
    Setup,
    /// Filling the initial transaction inventory.
    Prefill,
    /// Submitting measured load.
    #[default]
    Submitting,
    /// Waiting for canonical confirmations after submission stops.
    DrainingConfirmations,
    /// Recovering funds and payload state.
    Cleanup,
}

impl LoadTestStage {
    /// Stable label used by the footer and structured logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Prefill => "prefill",
            Self::Submitting => "submitting",
            Self::DrainingConfirmations => "draining_confirmations",
            Self::Cleanup => "cleanup",
        }
    }
}

/// Snapshot of live metrics for the status display.
#[derive(Debug, Clone, Default)]
pub struct DisplaySnapshot {
    /// Time elapsed since the run started.
    pub elapsed: Duration,
    /// Total run duration (`None` = continuous).
    pub duration: Option<Duration>,
    /// Current bounded lifecycle stage.
    pub stage: LoadTestStage,
    /// Total transactions submitted.
    pub submitted: u64,
    /// Total transactions confirmed.
    pub confirmed: usize,
    /// Total transactions failed.
    pub failed: u64,
    /// Total in-flight (unconfirmed) transactions.
    pub in_flight: u64,
    /// Number of senders at the in-flight limit.
    pub senders_blocked: usize,
    /// Total number of senders.
    pub total_senders: usize,
    /// Rolling 30s TPS.
    pub rolling_tps: f64,
    /// Rolling 30s GPS.
    pub rolling_gps: f64,
    /// Configured target GPS (`None` means unbounded).
    pub target_gps: Option<u64>,
    /// Rolling 30s p50 block landing latency.
    pub p50_latency: Duration,
    /// Rolling 30s p99 block landing latency.
    pub p99_latency: Duration,
    /// Rolling 30s flashblocks p50 latency.
    pub flashblocks_p50_latency: Duration,
    /// Rolling 30s flashblocks p99 latency.
    pub flashblocks_p99_latency: Duration,
    /// Current gas price in gwei.
    pub gas_price_gwei: f64,
}

/// Live progress-bar display for a running load test.
///
/// Uses `indicatif` for animated progress bars. Log output is routed through
/// an `IndicatifWriter` that calls `MultiProgress::suspend()` around each
/// write, preventing log lines from corrupting the progress bar display.
pub struct LoadTestDisplay {
    multi_progress: MultiProgress,
    header: ProgressBar,
    txs: ProgressBar,
    rate: ProgressBar,
    flight: ProgressBar,
    gas_lat: ProgressBar,
    flashblocks_lat: ProgressBar,
    duration: Option<Duration>,
}

impl LoadTestDisplay {
    /// Initialises the global tracing subscriber with progress-bar-aware log
    /// output.
    ///
    /// Returns the `MultiProgress` that manages the progress bars. Pass it to
    /// [`LoadTestDisplay::new`] after the run duration is known.
    pub fn init_tracing() -> Result<Option<MultiProgress>> {
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("warn,base_load_tests=info,base_load_tester=info"));
        let interactive = Self::terminal_supported(io::stderr().is_terminal());
        let multi_progress = interactive
            .then(|| MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(10)));

        if let Some(mp) = &multi_progress {
            let writer: IndicatifWriter<Stderr> = IndicatifWriter::new(mp.clone());
            tracing_subscriber::registry()
                .with(tracing_subscriber::fmt::layer().with_writer(writer).with_ansi(true))
                .with(filter)
                .try_init()
                .map_err(|error| {
                    BaselineError::Config(format!("failed to initialize tracing: {error}"))
                })?;
        } else {
            tracing_subscriber::registry()
                .with(tracing_subscriber::fmt::layer().with_ansi(false))
                .with(filter)
                .try_init()
                .map_err(|error| {
                    BaselineError::Config(format!("failed to initialize tracing: {error}"))
                })?;
        }

        Ok(multi_progress)
    }

    /// Returns whether an attended terminal can render the live footer.
    pub const fn terminal_supported(stderr_is_terminal: bool) -> bool {
        stderr_is_terminal
    }

    /// Creates a new display and attaches its bars to `mp`.
    ///
    /// `duration` controls whether the header shows a finite progress bar or a
    /// continuous spinner.
    pub fn new(mp: &MultiProgress, duration: Option<Duration>) -> Self {
        let header = duration.map_or_else(
            || {
                let pb = mp.add(ProgressBar::new_spinner());
                pb.set_style(
                    ProgressStyle::with_template("{spinner:.cyan} {msg}")
                        .expect("template is valid"),
                );
                pb
            },
            |d| {
                let pb = mp.add(ProgressBar::new(d.as_secs().max(1)));
                pb.set_style(
                    ProgressStyle::with_template(
                        "{spinner:.cyan} {msg}  [{bar:40.cyan/blue}] {percent}%",
                    )
                    .expect("template is valid")
                    .progress_chars("█░"),
                );
                pb
            },
        );
        header.set_message("Base Load Test  starting...");
        header.enable_steady_tick(Duration::from_millis(120));

        let stat_style = ProgressStyle::with_template("  {msg}").expect("stat template is valid");
        let make_stat = |mp: &MultiProgress| {
            let pb = mp.add(ProgressBar::new_spinner());
            pb.set_style(stat_style.clone());
            pb
        };

        Self {
            multi_progress: mp.clone(),
            header,
            txs: make_stat(mp),
            rate: make_stat(mp),
            flight: make_stat(mp),
            gas_lat: make_stat(mp),
            flashblocks_lat: make_stat(mp),
            duration,
        }
    }

    /// Returns `true` when the display is visible (i.e., stdout is a TTY).
    pub fn is_active(&self) -> bool {
        !self.header.is_hidden()
    }

    /// Updates the header for a bounded lifecycle stage.
    pub fn set_stage(&self, stage: LoadTestStage) {
        let style = if stage == LoadTestStage::Submitting {
            self.duration.map_or_else(
                || {
                    ProgressStyle::with_template("{spinner:.cyan} {msg}")
                        .expect("template is valid")
                },
                |_| {
                    ProgressStyle::with_template(
                        "{spinner:.cyan} {msg}  [{bar:40.cyan/blue}] {percent}%",
                    )
                    .expect("template is valid")
                    .progress_chars("█░")
                },
            )
        } else {
            ProgressStyle::with_template("{spinner:.cyan} {msg}").expect("template is valid")
        };
        self.header.set_style(style);
        self.header.set_message(format!("Base Load Test  {}", stage.as_str()));

        if stage == LoadTestStage::Cleanup {
            for bar in [&self.txs, &self.rate, &self.flight, &self.gas_lat, &self.flashblocks_lat] {
                bar.finish_and_clear();
            }
        }
    }

    /// Updates all bars with the latest snapshot.
    pub fn update(&self, snap: &DisplaySnapshot) {
        let elapsed_str = fmt_hms(snap.elapsed);

        if snap.stage != LoadTestStage::Submitting {
            self.header.set_message(format!("Base Load Test  {}", snap.stage.as_str()));
        } else if let Some(d) = self.duration {
            self.header.set_position(snap.elapsed.as_secs().min(d.as_secs()));
            self.header.set_message(format!(
                "Base Load Test  elapsed {}   remaining {}",
                elapsed_str,
                fmt_hms(d.saturating_sub(snap.elapsed)),
            ));
        } else {
            self.header.set_message(format!("Base Load Test  elapsed {elapsed_str}   continuous"));
        }

        self.txs.set_message(format!(
            "txs     sub {}   conf {}   failed {}",
            fmt_num(snap.submitted),
            fmt_num(snap.confirmed as u64),
            fmt_num(snap.failed),
        ));

        let success_rate = if snap.submitted > 0 {
            snap.confirmed as f64 / snap.submitted as f64 * 100.0
        } else {
            100.0
        };
        self.rate.set_message(format!(
            "rate    {:.2}% success   tps {:.1}   gps {} / {}   (30s window)",
            success_rate,
            snap.rolling_tps,
            fmt_num(snap.rolling_gps as u64),
            snap.target_gps.map_or_else(|| "unbounded".to_string(), fmt_num),
        ));

        let all_blocked = snap.total_senders > 0 && snap.senders_blocked >= snap.total_senders;
        self.flight.set_message(if all_blocked {
            format!(
                "flight  {} total   !! {}/{} senders ALL BLOCKED !!",
                fmt_num(snap.in_flight),
                snap.senders_blocked,
                snap.total_senders,
            )
        } else {
            format!(
                "flight  {} total   {}/{} senders blocked",
                fmt_num(snap.in_flight),
                snap.senders_blocked,
                snap.total_senders,
            )
        });

        self.gas_lat.set_message(format!(
            "gas     {:.2} gwei   block latency p50 {}   p99 {}",
            snap.gas_price_gwei,
            fmt_latency(snap.p50_latency),
            fmt_latency(snap.p99_latency),
        ));

        if snap.flashblocks_p50_latency > Duration::ZERO
            || snap.flashblocks_p99_latency > Duration::ZERO
        {
            self.flashblocks_lat.set_message(format!(
                "               fb latency p50 {}   p99 {}",
                fmt_latency(snap.flashblocks_p50_latency),
                fmt_latency(snap.flashblocks_p99_latency),
            ));
        } else {
            self.flashblocks_lat
                .set_message("               fb latency waiting for data...".to_string());
        }
    }

    /// Finishes all bars and clears the stat rows.
    pub fn finish(&self) {
        self.header.finish_and_clear();
        for bar in [&self.txs, &self.rate, &self.flight, &self.gas_lat, &self.flashblocks_lat] {
            bar.finish_and_clear();
        }
    }

    /// Temporarily clears the footer while another output stream writes.
    pub fn suspend<T>(&self, operation: impl FnOnce() -> T) -> T {
        self.multi_progress.suspend(operation)
    }

    /// Creates a setup progress bar managed by the same footer as tracing output.
    pub fn progress_bar(&self, total: u64, prefix: &str) -> ProgressBar {
        let progress = self.multi_progress.insert_before(&self.header, ProgressBar::new(total));
        progress.set_style(
            ProgressStyle::with_template("{prefix} [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
                .expect("valid template")
                .progress_chars("█▓░"),
        );
        progress.set_prefix(prefix.to_string());
        progress
    }
}

impl std::fmt::Debug for LoadTestDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadTestDisplay")
            .field("is_active", &self.is_active())
            .finish_non_exhaustive()
    }
}

fn fmt_hms(d: Duration) -> String {
    let s = d.as_secs();
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    if h > 0 { format!("{h:02}:{m:02}:{sec:02}") } else { format!("{m:02}:{sec:02}") }
}

fn fmt_latency(d: Duration) -> String {
    let ms = d.as_millis();
    if ms >= 10_000 {
        format!("{:.1}s", d.as_secs_f64())
    } else if ms >= 1_000 {
        format!("{:.2}s", d.as_secs_f64())
    } else {
        format!("{ms}ms")
    }
}

fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let mut result = Vec::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.into_iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::LoadTestDisplay;

    #[test]
    fn terminal_support_requires_a_real_terminal() {
        assert!(LoadTestDisplay::terminal_supported(true));
        assert!(!LoadTestDisplay::terminal_supported(false));
    }
}
