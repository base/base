use super::metrics::TestResults;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

pub fn print_results(results: &TestResults) {
    println!("\n");
    println!("Load Test Results");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    println!("Configuration:");
    println!("  Target:              {}", results.config.target);
    println!("  Sequencer:           {}", results.config.sequencer);
    println!("  Wallets:             {}", results.config.wallets);
    println!("  Target Rate:         {} tx/s", results.config.target_rate);
    println!("  Duration:            {}s", results.config.duration_secs);
    println!("  TX Timeout:          {}s", results.config.tx_timeout_secs);
    if let Some(seed) = results.config.seed {
        println!("  Seed:                {seed}");
    }

    println!("\nThroughput:");
    println!(
        "  Sent:                {:.1} tx/s ({} total)",
        results.results.sent_rate, results.results.total_sent
    );
    println!(
        "  Included:            {:.1} tx/s ({} total)",
        results.results.included_rate, results.results.total_included
    );
    println!(
        "  Success Rate:        {:.1}%",
        results.results.success_rate * 100.0
    );

    println!("\nTransaction Results:");
    println!(
        "  Included:            {} ({:.1}%)",
        results.results.total_included,
        (results.results.total_included as f64 / results.results.total_sent as f64) * 100.0
    );
    if results.results.total_reverted > 0 {
        println!(
            "  Reverted:            {} ({:.1}%)",
            results.results.total_reverted,
            (results.results.total_reverted as f64 / results.results.total_sent as f64) * 100.0
        );
    }
    println!(
        "  Timed Out:           {} ({:.1}%)",
        results.results.total_timed_out,
        (results.results.total_timed_out as f64 / results.results.total_sent as f64) * 100.0
    );
    println!("  Send Errors:         {}", results.errors.send_errors);
    if results.results.total_pending > 0 {
        println!("  Still Pending:       {}", results.results.total_pending);
    }

    println!("\n");
}

pub fn save_results(results: &TestResults, path: &Path) -> Result<()> {
    let json = serde_json::to_string_pretty(results).context("Failed to serialize results")?;
    fs::write(path, json).context("Failed to write results file")?;
    println!("💾 Metrics saved to: {}", path.display());
    Ok(())
}
