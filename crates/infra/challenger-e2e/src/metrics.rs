//! Minimal Prometheus text-exposition scraper for the challenger under test.

use std::{sync::LazyLock, time::Duration};

use eyre::{Context, Result};
use reqwest::Client;
use url::Url;

/// Client for every scrape, built once so the connection pool is reused across
/// the several hundred polls a run makes.
///
/// The timeout is the point of it. Every caller is either inside a poll loop
/// that retries, or is annotating a failure that has already happened, so a
/// stalled endpoint must surface as an error rather than parking the run —
/// and `reqwest`'s default client has no timeout at all.
static CLIENT: LazyLock<Client> = LazyLock::new(|| {
    // Not `unwrap_or_default`: the default client has no timeout, so a silent
    // fallback would reintroduce exactly the hang this exists to prevent.
    Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("the reqwest TLS backend failed to initialise")
});

/// One scrape of a Prometheus `/metrics` endpoint.
///
/// Series are kept as `(name, labels, value)` rather than parsed into a typed
/// model; the driver only ever sums counters and reads gauges.
#[derive(Debug, Default)]
pub struct Scrape {
    series: Vec<(String, String, f64)>,
}

impl Scrape {
    /// Fetches and parses the endpoint.
    pub async fn fetch(url: &Url) -> Result<Self> {
        let body = CLIENT
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("failed to scrape {url}"))?
            .error_for_status()
            .with_context(|| format!("{url} returned an error status"))?
            .text()
            .await
            .with_context(|| format!("failed to read the response body from {url}"))?;
        Ok(Self::parse(&body))
    }

    /// Sum of every series sharing `name`, or `0.0` when the metric is absent.
    ///
    /// Absent-as-zero is what makes the assertions readable: a counter that has
    /// never been incremented is not exported at all, and that is the same
    /// thing as "it did not happen".
    pub fn sum(&self, name: &str) -> f64 {
        // Every string contains "", so this matches the name alone.
        self.label_sum(name, "")
    }

    /// Sum of every series sharing `name` whose label set mentions `label`.
    ///
    /// A substring match keeps the caller free of label-ordering and quoting
    /// concerns; label values in this crate's metrics are distinct enough that
    /// it cannot alias.
    pub fn label_sum(&self, name: &str, label: &str) -> f64 {
        self.series
            .iter()
            .filter(|(series_name, labels, _)| series_name == name && labels.contains(label))
            .map(|(.., value)| value)
            .sum()
    }

    fn parse(body: &str) -> Self {
        let series = body
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(|line| {
                // Prometheus allows an optional timestamp after the value.
                let (key, rest) = line.split_once(' ')?;
                let value = rest.split_whitespace().next()?.parse::<f64>().ok()?;
                let (name, labels) = key
                    .split_once('{')
                    .map_or((key, ""), |(name, labels)| (name, labels.trim_end_matches('}')));
                Some((name.to_string(), labels.to_string(), value))
            })
            .collect();
        Self { series }
    }
}

#[cfg(test)]
mod tests {
    use super::Scrape;

    #[test]
    fn parses_counters_gauges_and_labels() {
        let scrape = Scrape::parse(
            r#"
            # HELP base_challenger_up Challenger is running
            # TYPE base_challenger_up gauge
            base_challenger_up 1
            base_challenger_games_scanned_total 42
            base_challenger_nullify_tx_outcome_total{status="success"} 3
            base_challenger_nullify_tx_outcome_total{status="reverted"} 2
            "#,
        );

        assert_eq!(scrape.sum("base_challenger_up"), 1.0);
        assert_eq!(scrape.sum("base_challenger_games_scanned_total"), 42.0);
        assert_eq!(scrape.sum("base_challenger_nullify_tx_outcome_total"), 5.0);
        assert_eq!(scrape.label_sum("base_challenger_nullify_tx_outcome_total", "reverted"), 2.0);
        // A counter that never fired is not exported, and reads as zero.
        assert_eq!(scrape.sum("base_challenger_challenge_tx_submitted_total"), 0.0);
    }

    #[test]
    fn ignores_optional_prometheus_timestamp() {
        let scrape = Scrape::parse("base_challenger_up 1 1710000000000\n");
        assert_eq!(scrape.sum("base_challenger_up"), 1.0);
    }
}
