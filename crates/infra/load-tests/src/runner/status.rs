use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tracing::level_filters::LevelFilter;
use tracing_indicatif::{IndicatifWriter, writer::Stderr};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Snapshot of live metrics for the status display.
#[derive(Debug, Clone, Default)]
pub struct DisplaySnapshot {
    /// Time elapsed since the run started.
    pub elapsed: Duration,
    /// Total run duration (`None` = continuous).
    pub duration: Option<Duration>,
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
    /// Rolling 30s p50 latency.
    pub p50_latency: Duration,
    /// Rolling 30s p99 latency.
    pub p99_latency: Duration,
    /// Rolling 30s flashblocks p50 latency.
    pub flashblocks_p50_latency: Duration,
    /// Rolling 30s flashblocks p99 latency.
    pub flashblocks_p99_latency: Duration,
    /// Current gas price in gwei.
    pub gas_price_gwei: f64,
    /// Total ETH across all sender accounts (formatted).
    pub total_eth: Option<String>,
    /// Minimum ETH in any single sender account (formatted).
    pub min_eth: Option<String>,
    /// Whether any account is below the low-balance threshold.
    pub funds_low: bool,
    /// Checksummed address of the funder wallet (set after `fund_accounts` runs).
    pub funder_address: Option<String>,
    /// Checksummed addresses of all sender accounts.
    pub sender_addresses: Vec<String>,
}

/// Live progress-bar display for a running load test.
///
/// Uses `indicatif` for animated progress bars. Log output is routed through
/// an `IndicatifWriter` that calls `MultiProgress::suspend()` around each
/// write, preventing log lines from corrupting the progress bar display.
pub struct LoadTestDisplay {
    header: ProgressBar,
    txs: ProgressBar,
    rate: ProgressBar,
    flight: ProgressBar,
    funding: ProgressBar,
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
    pub fn init_tracing() -> MultiProgress {
        let mp = MultiProgress::new();
        // IndicatifWriter wraps `mp` and calls `mp.suspend()` around every
        // tracing log write, so log lines never corrupt the rendered bars.
        let writer: IndicatifWriter<Stderr> = IndicatifWriter::new(mp.clone());

        let filter =
            EnvFilter::builder().with_default_directive(LevelFilter::WARN.into()).from_env_lossy();

        let _ = tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_writer(writer).with_ansi(true))
            .with(filter)
            .try_init();

        mp
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
            header,
            txs: make_stat(mp),
            rate: make_stat(mp),
            flight: make_stat(mp),
            funding: make_stat(mp),
            gas_lat: make_stat(mp),
            flashblocks_lat: make_stat(mp),
            duration,
        }
    }

    /// Returns `true` when the display is visible (i.e., stdout is a TTY).
    pub fn is_active(&self) -> bool {
        !self.header.is_hidden()
    }

    /// Updates all bars with the latest snapshot.
    pub fn update(&self, snap: &DisplaySnapshot) {
        let elapsed_str = fmt_hms(snap.elapsed);

        if let Some(d) = self.duration {
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
            "rate    {:.2}% success   tps {:.1}   gps {}   (30s window)",
            success_rate,
            snap.rolling_tps,
            fmt_num(snap.rolling_gps as u64),
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

        self.funding.set_message(match (&snap.total_eth, &snap.min_eth) {
            (Some(total), Some(min)) if snap.funds_low => {
                format!("funding !! total {total} ETH   min/acct {min} ETH   LOW !!")
            }
            (Some(total), Some(min)) => {
                format!("funding total {total} ETH   min/acct {min} ETH")
            }
            _ => "funding fetching...".to_string(),
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
        if let Some(d) = self.duration {
            self.header.set_position(d.as_secs());
        }
        self.header.finish_with_message("Base Load Test  complete");
        for bar in [
            &self.txs,
            &self.rate,
            &self.flight,
            &self.funding,
            &self.gas_lat,
            &self.flashblocks_lat,
        ] {
            bar.finish_and_clear();
        }
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
