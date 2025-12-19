use crate::domain::events::MempoolEvent;
use async_trait::async_trait;

#[async_trait]
pub trait EventSource: Send + Sync {
    async fn receive(&self) -> anyhow::Result<MempoolEvent>;
}
