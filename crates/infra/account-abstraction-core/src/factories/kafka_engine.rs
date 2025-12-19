use crate::domain::mempool::PoolConfig;
use crate::infrastructure::in_memory::mempool::InMemoryMempool;
use crate::infrastructure::kafka::consumer::KafkaEventSource;
use crate::services::mempool_engine::MempoolEngine;
use rdkafka::{
    ClientConfig,
    consumer::{Consumer, StreamConsumer},
};
use std::sync::Arc;
use tips_core::kafka::load_kafka_config_from_file;
use tokio::sync::RwLock;

pub fn create_mempool_engine(
    properties_file: &str,
    topic: &str,
    consumer_group_id: &str,
    pool_config: Option<PoolConfig>,
) -> anyhow::Result<Arc<MempoolEngine<InMemoryMempool>>> {
    let mut client_config = ClientConfig::from_iter(load_kafka_config_from_file(properties_file)?);
    client_config.set("group.id", consumer_group_id);
    client_config.set("enable.auto.commit", "true");

    let consumer: StreamConsumer = client_config.create()?;
    consumer.subscribe(&[topic])?;

    let event_source = Arc::new(KafkaEventSource::new(Arc::new(consumer)));
    let mempool = Arc::new(RwLock::new(InMemoryMempool::new(
        pool_config.unwrap_or_default(),
    )));
    let engine = MempoolEngine::<InMemoryMempool>::new(mempool, event_source);

    Ok(Arc::new(engine))
}
