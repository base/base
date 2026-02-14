use std::{path::Path, time::Duration};

use alloy_primitives::B256;
use anyhow::{Context, Result};
use base_primitives::BundleExtensions;
use rdkafka::{
    Message,
    config::ClientConfig,
    consumer::{Consumer, StreamConsumer},
    message::BorrowedMessage,
};
use tips_audit_lib::{BundleEvent, load_kafka_config_from_file};
use tokio::time::{Instant, timeout};
use uuid::Uuid;

const DEFAULT_AUDIT_TOPIC: &str = "tips-audit";
const DEFAULT_AUDIT_PROPERTIES: &str = "../../docker/host-ingress-audit-kafka-properties";
const KAFKA_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

fn resolve_properties_path(env_key: &str, default_path: &str) -> Result<String> {
    match std::env::var(env_key) {
        Ok(value) => Ok(value),
        Err(_) => {
            if Path::new(default_path).exists() {
                Ok(default_path.to_string())
            } else {
                anyhow::bail!(
                    "Environment variable {env_key} must be set (default path '{default_path}' not found). \
                     Run `just sync` or export {env_key} before running tests."
                );
            }
        }
    }
}

fn build_kafka_consumer(properties_env: &str, default_path: &str) -> Result<StreamConsumer> {
    let props_file = resolve_properties_path(properties_env, default_path)?;

    let mut client_config = ClientConfig::from_iter(load_kafka_config_from_file(&props_file)?);

    client_config
        .set(
            "group.id",
            format!(
                "tips-system-tests-{}",
                Uuid::new_v5(&Uuid::NAMESPACE_OID, props_file.as_bytes())
            ),
        )
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest");

    client_config.create().context("Failed to create Kafka consumer")
}

async fn wait_for_kafka_message<T>(
    properties_env: &str,
    default_properties: &str,
    topic_env: &str,
    default_topic: &str,
    timeout_duration: Duration,
    mut matcher: impl FnMut(BorrowedMessage<'_>) -> Option<T>,
) -> Result<T> {
    let consumer = build_kafka_consumer(properties_env, default_properties)?;
    let topic = std::env::var(topic_env).unwrap_or_else(|_| default_topic.to_string());
    consumer.subscribe(&[&topic])?;

    let deadline = Instant::now() + timeout_duration;

    loop {
        let now = Instant::now();
        if now >= deadline {
            anyhow::bail!(
                "Timed out waiting for Kafka message on topic {topic} after {timeout_duration:?}"
            );
        }

        let remaining = deadline - now;
        match timeout(remaining, consumer.recv()).await {
            Ok(Ok(message)) => {
                if let Some(value) = matcher(message) {
                    return Ok(value);
                }
            }
            Ok(Err(err)) => {
                return Err(err.into());
            }
            Err(_) => {
                // Timeout for this iteration, continue looping
            }
        }
    }
}

pub(crate) async fn wait_for_audit_event_by_hash(
    expected_bundle_hash: &B256,
    mut matcher: impl FnMut(&BundleEvent) -> bool,
) -> Result<BundleEvent> {
    let expected_hash = *expected_bundle_hash;
    wait_for_kafka_message(
        "TIPS_INGRESS_KAFKA_AUDIT_PROPERTIES_FILE",
        DEFAULT_AUDIT_PROPERTIES,
        "TIPS_INGRESS_KAFKA_AUDIT_TOPIC",
        DEFAULT_AUDIT_TOPIC,
        KAFKA_WAIT_TIMEOUT,
        |message| {
            let payload = message.payload()?;
            let event: BundleEvent = serde_json::from_slice(payload).ok()?;
            // Match by bundle hash from the Received event
            if let BundleEvent::Received { bundle, .. } = &event
                && bundle.bundle_hash() == expected_hash
                && matcher(&event)
            {
                return Some(event);
            }
            None
        },
    )
    .await
}
