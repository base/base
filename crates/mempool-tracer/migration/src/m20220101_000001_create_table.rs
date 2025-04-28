use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(MempoolEvent::Table)
                    .if_not_exists()
                    .col(pk_auto(MempoolEvent::Id))
                    .col(char_len(MempoolEvent::TxHash, 66))
                    .col(string_null(MempoolEvent::NodeId))
                    .col(string(MempoolEvent::EventType))
                    .col(date_time(MempoolEvent::OccurredAt))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx-mempool_event_tx_hash")
                    .table(MempoolEvent::Table)
                    .col(MempoolEvent::TxHash)
                    .to_owned(),
            )
            .await?;
    
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(Index::drop().name("idx-mempool_event_tx_hash").to_owned())
            .await?;

        manager
            .drop_table(Table::drop().table(MempoolEvent::Table).to_owned())
            .await?;

        Ok(())
    }
}

#[derive(DeriveIden)]
enum MempoolEvent {
    Table,
    Id,
    TxHash,
    NodeId,
    EventType,
    OccurredAt,
}