use std::error::Error;
use alloy_primitives::{TxHash, B256};
use sea_orm::{Database, DatabaseConnection, EntityTrait, IntoActiveValue};
use sea_orm::prelude::DateTime;
use sea_orm::sqlx::types::chrono::{NaiveDateTime, Utc};
use tracing::info;
use entity::mempool_event;
use entity::prelude::MempoolEvent;



pub trait MempoolTracer: Send + Sync {
    // todo add flashblocks

    /// Called when a transaction is added to the pending pool
    async fn pending_tx(&self, tx_hash: TxHash) -> Result<(), Box<dyn Error>>;
    /// Called when a transaction is added to the queued pool
    async fn queued_tx(&self, tx_hash: TxHash) -> Result<(), Box<dyn Error>>;
    /// Called when a transaction is mined
    async fn mined_tx(&self, tx_hash: TxHash, block_hash: B256) -> Result<(), Box<dyn Error>>;
    /// Called when a transaction is dropped from the mempool
    async fn dropped_tx(&self, tx_hash: TxHash) -> Result<(), Box<dyn Error>>;
    /// Called when a transaction is marked as invalid
    async fn invalid_tx(&self, tx_hash: TxHash) -> Result<(), Box<dyn Error>>;
    /// Called when a transaction is replaced by another transaction
    async fn replaced_tx(&self, tx_hash: TxHash, replaced_by: TxHash) -> Result<(), Box<dyn Error>>;
}

/// A simple implementation of MempoolTracer that logs transaction events
pub struct LoggingMempoolTracer;

impl Default for LoggingMempoolTracer {
    fn default() -> Self {
        info!(target: "mempool", "[MempoolTracer] Logging enabled");
        Self {}
    }
}

impl MempoolTracer for LoggingMempoolTracer {
    async fn pending_tx(&self, tx_hash: TxHash) -> Result<(), Box<dyn Error>> {
        info!(tx_hash = ?tx_hash, "Transaction added to pending pool");
        Ok(())
    }

    async fn queued_tx(&self, tx_hash: TxHash) -> Result<(), Box<dyn Error>> {
        info!(tx_hash = ?tx_hash, "Transaction added to queued pool");
        Ok(())
    }

    async fn mined_tx(&self, tx_hash: TxHash, block_hash: B256) -> Result<(), Box<dyn Error>> {
        info!(tx_hash = ?tx_hash, "Transaction mined");
        Ok(())
    }

    async fn dropped_tx(&self, tx_hash: TxHash) -> Result<(), Box<dyn Error>> {
        info!(tx_hash = ?tx_hash, "Transaction dropped from mempool");
        Ok(())
    }

    async fn invalid_tx(&self, tx_hash: TxHash) -> Result<(), Box<dyn Error>> {
        info!(tx_hash = ?tx_hash, "Transaction marked as invalid");
        Ok(())
    }

    async fn replaced_tx(&self, tx_hash: TxHash, replaced_by: TxHash) -> Result<(), Box<dyn Error>> {
        info!(tx_hash = ?tx_hash, "Transaction replaced by {}", replaced_by);
        Ok(())
    }
}

pub struct DatabaseMempoolTracer {
    db: DatabaseConnection,
    node_id: Option<String>,
}

enum EventType {
    Pending,
    Queued,
    Mined,
    Dropped,
    Invalid,
    Replaced,
}

impl EventType {
    fn to_string(&self) -> String {
        match self {
            EventType::Pending => "pending",
            EventType::Queued => "queued",
            EventType::Mined => "mined",
            EventType::Dropped => "dropped",
            EventType::Invalid => "invalid",
            EventType::Replaced => "replaced",
        }.to_string()
    }
}

impl MempoolTracer for DatabaseMempoolTracer {
    async fn pending_tx(&self, tx_hash: TxHash) -> Result<(), Box<dyn Error>> {
        self.add_to_database(tx_hash, EventType::Pending).await?;
        Ok(())
    }

    async fn queued_tx(&self, tx_hash: TxHash) -> Result<(), Box<dyn Error>> {
        self.add_to_database(tx_hash, EventType::Queued).await?;
        Ok(())
    }

    async fn mined_tx(&self, tx_hash: TxHash, _block_hash: B256) -> Result<(), Box<dyn Error>> {
        // TODO: Track block included in / block timestamp
        self.add_to_database(tx_hash, EventType::Dropped).await?;
        Ok(())
    }

    async fn dropped_tx(&self, tx_hash: TxHash) -> Result<(), Box<dyn Error>> {
        self.add_to_database(tx_hash, EventType::Dropped).await?;
        Ok(())
    }

    async fn invalid_tx(&self, tx_hash: TxHash) -> Result<(), Box<dyn Error>>{
        self.add_to_database(tx_hash, EventType::Invalid).await?;
        Ok(())
    }

    async fn replaced_tx(&self, tx_hash: TxHash, replaced_by: TxHash) -> Result<(), Box<dyn Error>> {
        // TODO: Track replaced by.
        self.add_to_database(tx_hash, EventType::Replaced).await?;
        Ok(())
    }
}

impl DatabaseMempoolTracer {
    async fn add_to_database(&self, tx_hash: TxHash, event_type: EventType) -> Result<(), Box<dyn Error>> {
        let event = mempool_event::ActiveModel {
            tx_hash: tx_hash.to_string().into_active_value(),
            node_id: self.node_id.clone().into_active_value(),
            event_type: event_type.to_string().into_active_value(),
            occurred_at: Utc::now().naive_utc().into_active_value(),
            ..Default::default()
        };

        MempoolEvent::insert(event)
            .exec(&self.db).await?;
        Ok(())
    }
}