use alloy_consensus::{Transaction, TxType};
use alloy_primitives::B256;
use reth_transaction_pool::{CanonicalStateUpdate, PoolTransaction, TransactionOrigin, TransactionPool, TransactionPoolExt};
use reth_transaction_pool::test_utils::{testing_pool, MockTransaction};
use reth_transaction_pool::{FullTransactionEvent};
use reth_transaction_pool::test_utils::MockTransaction::Eip1559;
use ::entity::mempool_event::Entity;
use ::entity::prelude::MempoolEvent;
use migration::{Migrator, MigratorTrait};
use sea_orm::*;
use testcontainers_modules::{postgres, testcontainers::runners::AsyncRunner};
use tokio_stream::StreamExt;

#[tokio::test]
async fn test_with_postgres() -> Result<(), Box<dyn std::error::Error>> {
    // let container = postgres::Postgres::default().start().await?;
    //
    // let connection_string = &format!(
    //     "postgres://postgres:postgres@127.0.0.1:{}/postgres",
    //     container.get_host_port_ipv4(5432).await?
    // );
    //
    // let db: DatabaseConnection = Database::connect(connection_string).await?;
    // Migrator::up(&db, None).await?;

    let pool = testing_pool();
    let mut txns = pool.all_transactions_event_listener();

    tokio::spawn(async move {
        while let Some(event) = txns.next().await {
            println!("Transaction received: {event:?}");
        }
    });

    let tx = MockTransaction::new_from_type(TxType::Eip1559);
    pool.add_external_transaction(tx.clone()).await.unwrap();
    
    let replacement_tx = tx.clone()
        .set_hash(B256::random())
        .inc_price();

    let queued_tx = replacement_tx.clone()
        .set_hash(B256::random()).clone()
        .inc_nonce()
        .inc_price();

    pool.add_external_transaction(replacement_tx.clone()).await.unwrap();
    pool.remove_transactions(vec![replacement_tx.hash().clone()]);

    pool.add_external_transaction(queued_tx.clone()).await.unwrap();
    
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;



    // let paginator = MempoolEvent::find()
    //     .order_by_asc(<Entity as EntityTrait>::Column::Id)
    //     .paginate(&db, 10);
    //
    // let num_pages = paginator.num_pages().await?;
    // assert_eq!(num_pages, 0);

    Ok(())
}
