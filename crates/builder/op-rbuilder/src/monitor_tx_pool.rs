use futures_util::StreamExt;
use reth_optimism_node::txpool::OpPooledTransaction;
use reth_transaction_pool::{AllTransactionsEvents, FullTransactionEvent};
use tracing::info;

pub async fn monitor_tx_pool(mut new_transactions: AllTransactionsEvents<OpPooledTransaction>) {
    while let Some(event) = new_transactions.next().await {
        transaction_event_log(event);
    }
}

fn transaction_event_log(event: FullTransactionEvent<OpPooledTransaction>) {
    match event {
        FullTransactionEvent::Pending(hash) => {
            info!(
                target = "monitoring",
                tx_hash = hash.to_string(),
                kind = "pending",
                "Transaction event received"
            )
        }
        FullTransactionEvent::Queued(hash) => {
            info!(
                target = "monitoring",
                tx_hash = hash.to_string(),
                kind = "queued",
                "Transaction event received"
            )
        }
        FullTransactionEvent::Mined {
            tx_hash,
            block_hash,
        } => info!(
            target = "monitoring",
            tx_hash = tx_hash.to_string(),
            kind = "mined",
            block_hash = block_hash.to_string(),
            "Transaction event received"
        ),
        FullTransactionEvent::Replaced {
            transaction,
            replaced_by,
        } => info!(
            target = "monitoring",
            tx_hash = transaction.hash().to_string(),
            kind = "replaced",
            replaced_by = replaced_by.to_string(),
            "Transaction event received"
        ),
        FullTransactionEvent::Discarded(hash) => {
            info!(
                target = "monitoring",
                tx_hash = hash.to_string(),
                kind = "discarded",
                "Transaction event received"
            )
        }
        FullTransactionEvent::Invalid(hash) => {
            info!(
                target = "monitoring",
                tx_hash = hash.to_string(),
                kind = "invalid",
                "Transaction event received"
            )
        }
        FullTransactionEvent::Propagated(_propagated) => {}
    }
}
