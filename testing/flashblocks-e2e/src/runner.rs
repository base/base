//! Test runner and result reporting.

use std::time::{Duration, Instant};

use alloy_eips::BlockNumberOrTag;
use colored::Colorize;
use eyre::Result;
use serde::Serialize;

use crate::{
    TestClient,
    tests::{Test, TestSuite},
};

/// Result of running a single test.
#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    /// Name of the test.
    pub name: String,
    /// Category the test belongs to.
    pub category: String,
    /// Whether the test passed.
    pub passed: bool,
    /// Duration in milliseconds.
    pub duration_ms: u64,
    /// Error message if the test failed.
    pub error: Option<String>,
    /// Whether the test was skipped.
    pub skipped: bool,
    /// Reason for skipping if applicable.
    pub skip_reason: Option<String>,
}

/// Summary of test run.
#[derive(Debug, Serialize)]
pub struct TestSummary {
    /// Total number of tests.
    pub total: usize,
    /// Number of passed tests.
    pub passed: usize,
    /// Number of failed tests.
    pub failed: usize,
    /// Number of skipped tests.
    pub skipped: usize,
    /// Total duration in milliseconds.
    pub duration_ms: u64,
}

/// List all tests in the suite.
pub fn list_tests(suite: &TestSuite) {
    println!("{}", "Available tests:".bold());
    println!();

    for category in &suite.categories {
        println!("  {} {}", "Category:".cyan(), category.name.bold());
        if let Some(desc) = &category.description {
            println!("    {}", desc.dimmed());
        }
        for test in &category.tests {
            println!("    - {}", test.name);
            if let Some(desc) = &test.description {
                println!("      {}", desc.dimmed());
            }
        }
        println!();
    }
}

/// Run tests matching the optional filter.
pub async fn run_tests(
    client: &TestClient,
    suite: &TestSuite,
    filter: Option<&str>,
    keep_going: bool,
) -> Vec<TestResult> {
    let mut results = Vec::new();
    let start = Instant::now();

    // Check connection first
    println!("{}", "Connecting to node...".dimmed());
    match client.get_block_by_number(BlockNumberOrTag::Latest).await {
        Ok(Some(block)) => {
            println!("{} Connected to node at block #{}", "OK".green(), block.header.number);
        }
        Ok(None) => {
            println!("{} Connected but no blocks found", "WARN".yellow());
        }
        Err(e) => {
            println!("{} Failed to connect: {}", "ERROR".red(), e);
            return vec![TestResult {
                name: "connection".to_string(),
                category: "setup".to_string(),
                passed: false,
                duration_ms: 0,
                error: Some(e.to_string()),
                skipped: false,
                skip_reason: None,
            }];
        }
    }

    println!();
    println!("{}", "Running tests...".bold());
    println!();

    for category in &suite.categories {
        let category_tests: Vec<&Test> =
            category.tests.iter().filter(|t| matches_filter(&t.name, filter)).collect();

        if category_tests.is_empty() {
            continue;
        }

        println!("  {} {}", "Category:".cyan(), category.name.bold());

        for test in category_tests {
            let result = run_single_test(client, &category.name, test).await;

            // Print result
            let status = if result.skipped {
                "SKIP".yellow()
            } else if result.passed {
                "PASS".green()
            } else {
                "FAIL".red()
            };

            println!("    {} {} ({}ms)", status, test.name, result.duration_ms);

            if let Some(ref err) = result.error {
                println!("      {}", err.red());
            }

            if let Some(ref reason) = result.skip_reason {
                println!("      {}", reason.dimmed());
            }

            let failed = !result.passed && !result.skipped;
            results.push(result);

            // Stop early if not keep_going and test failed
            if failed && !keep_going {
                println!();
                println!(
                    "{}",
                    "Stopping due to test failure (use --keep-going to continue)".yellow()
                );
                return results;
            }
        }

        println!();
    }

    let elapsed = start.elapsed();
    println!("Completed {} tests in {:.2}s", results.len(), elapsed.as_secs_f64());

    results
}

/// Run a single test.
async fn run_single_test(client: &TestClient, category: &str, test: &Test) -> TestResult {
    let start = Instant::now();

    // Check skip condition
    if let Some(ref skip_fn) = test.skip_if
        && let Some(reason) = skip_fn(client).await
    {
        return TestResult {
            name: test.name.clone(),
            category: category.to_string(),
            passed: true,
            duration_ms: start.elapsed().as_millis() as u64,
            error: None,
            skipped: true,
            skip_reason: Some(reason),
        };
    }

    // Run the test
    let result = tokio::time::timeout(Duration::from_secs(30), (test.run)(client)).await;

    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(Ok(())) => TestResult {
            name: test.name.clone(),
            category: category.to_string(),
            passed: true,
            duration_ms,
            error: None,
            skipped: false,
            skip_reason: None,
        },
        Ok(Err(e)) => TestResult {
            name: test.name.clone(),
            category: category.to_string(),
            passed: false,
            duration_ms,
            error: Some(format!("{:#}", e)),
            skipped: false,
            skip_reason: None,
        },
        Err(_) => TestResult {
            name: test.name.clone(),
            category: category.to_string(),
            passed: false,
            duration_ms,
            error: Some("Test timed out after 30s".to_string()),
            skipped: false,
            skip_reason: None,
        },
    }
}

/// Check if test name matches filter.
fn matches_filter(name: &str, filter: Option<&str>) -> bool {
    match filter {
        None => true,
        Some(f) => {
            // Simple glob matching
            if f.contains('*') {
                let parts: Vec<&str> = f.split('*').collect();
                let mut remaining = name;
                for (i, part) in parts.iter().enumerate() {
                    if part.is_empty() {
                        continue;
                    }
                    if i == 0 {
                        // Must start with this part
                        if !remaining.starts_with(part) {
                            return false;
                        }
                        remaining = &remaining[part.len()..];
                    } else if i == parts.len() - 1 {
                        // Must end with this part
                        if !remaining.ends_with(part) {
                            return false;
                        }
                    } else {
                        // Must contain this part
                        if let Some(pos) = remaining.find(part) {
                            remaining = &remaining[pos + part.len()..];
                        } else {
                            return false;
                        }
                    }
                }
                true
            } else {
                name.contains(f)
            }
        }
    }
}

/// Print results in text format.
pub fn print_results_text(results: &[TestResult]) {
    let summary = compute_summary(results);

    println!();
    println!("{}", "=".repeat(60));
    println!("{}", "Test Summary".bold());
    println!("{}", "=".repeat(60));
    println!();

    println!("  Total:   {}", summary.total);
    println!("  Passed:  {}", format!("{}", summary.passed).green());
    println!("  Failed:  {}", format!("{}", summary.failed).red());
    println!("  Skipped: {}", format!("{}", summary.skipped).yellow());
    println!("  Duration: {:.2}s", summary.duration_ms as f64 / 1000.0);
    println!();

    if summary.failed > 0 {
        println!("{}", "Failed tests:".red().bold());
        for result in results.iter().filter(|r| !r.passed && !r.skipped) {
            println!("  - {} ({})", result.name, result.category);
            if let Some(ref err) = result.error {
                println!("    {}", err.dimmed());
            }
        }
        println!();
    }

    if summary.failed == 0 {
        println!("{}", "All tests passed!".green().bold());
    } else {
        println!("{}", format!("{} test(s) failed", summary.failed).red().bold());
    }
}

/// Print results in JSON format.
pub fn print_results_json(results: &[TestResult]) -> Result<()> {
    let output = serde_json::json!({
        "results": results,
        "summary": compute_summary(results),
    });

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn compute_summary(results: &[TestResult]) -> TestSummary {
    let total = results.len();
    let passed = results.iter().filter(|r| r.passed && !r.skipped).count();
    let skipped = results.iter().filter(|r| r.skipped).count();
    let failed = results.iter().filter(|r| !r.passed && !r.skipped).count();
    let duration_ms: u64 = results.iter().map(|r| r.duration_ms).sum();

    TestSummary { total, passed, failed, skipped, duration_ms }
}
