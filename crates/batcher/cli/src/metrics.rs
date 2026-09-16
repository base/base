use metrics_exporter_prometheus::{BuildError, Matcher, PrometheusBuilder};

const COMPRESSION_RATIO_BUCKETS: &[f64] =
    &[0.1, 0.2, 0.3, 0.35, 0.4, 0.45, 0.5, 0.55, 0.6, 0.65, 0.7, 0.75, 0.8, 0.85, 0.9, 0.95];
const BLOB_FILL_RATIO_BUCKETS: &[f64] = &[0.1, 0.25, 0.5, 0.75, 0.85, 0.9, 0.95, 0.99, 1.0];
const CHANNEL_DURATION_BUCKETS: &[f64] = &[1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 40.0, 50.0];
const L2_BLOCKS_PER_CHANNEL_BUCKETS: &[f64] =
    &[1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 200.0, 500.0, 1_000.0, 2_000.0];
const BLOBS_PER_TX_BUCKETS: &[f64] = &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0];

const HISTOGRAM_BUCKETS: &[(&str, &[f64])] = &[
    ("batcher.channel_compression_ratio", COMPRESSION_RATIO_BUCKETS),
    ("batcher.blob_fill_ratio", BLOB_FILL_RATIO_BUCKETS),
    ("batcher.channel_duration_blocks", CHANNEL_DURATION_BUCKETS),
    ("batcher.l2_blocks_per_channel", L2_BLOCKS_PER_CHANNEL_BUCKETS),
    ("batcher.blobs_per_tx", BLOBS_PER_TX_BUCKETS),
];

/// Configures Prometheus buckets for base batcher histograms.
pub fn configure_prometheus(builder: PrometheusBuilder) -> Result<PrometheusBuilder, BuildError> {
    HISTOGRAM_BUCKETS.iter().copied().try_fold(builder, |builder, (metric, buckets)| {
        builder.set_buckets_for_metric(Matcher::Full(metric.to_owned()), buckets)
    })
}

#[cfg(test)]
mod tests {
    use base_batcher_encoder::{BatcherMetrics, EncoderConfig};
    use metrics::with_local_recorder;

    use super::*;

    #[test]
    fn blobs_per_tx_buckets_cover_protocol_limit() {
        assert_eq!(
            BLOBS_PER_TX_BUCKETS.last().copied(),
            Some(EncoderConfig::MAX_BLOBS_PER_TX as f64)
        );
    }

    #[test]
    fn configured_histograms_render_as_prometheus_buckets() {
        let recorder = configure_prometheus(PrometheusBuilder::new()).unwrap().build_recorder();
        let handle = recorder.handle();

        with_local_recorder(&recorder, || {
            BatcherMetrics::channel_compression_ratio().record(0.42);
            BatcherMetrics::blob_fill_ratio().record(0.9);
            BatcherMetrics::channel_duration_blocks().record(40.0);
            BatcherMetrics::l2_blocks_per_channel().record(500.0);
            BatcherMetrics::blobs_per_tx().record(6.0);
        });

        let rendered = handle.render();
        for (metric, _) in HISTOGRAM_BUCKETS {
            let metric = metric.replace('.', "_");
            assert!(rendered.contains(&format!("# TYPE {metric} histogram")));
            assert!(rendered.contains(&format!("{metric}_bucket")));
            assert!(!rendered.contains(&format!("{metric}{{quantile=")));
        }
    }
}
