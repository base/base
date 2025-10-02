use crate::reader::EventReader;
use crate::storage::EventWriter;
use anyhow::Result;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info};

pub struct KafkaMempoolArchiver<R, W>
where
    R: EventReader,
    W: EventWriter,
{
    reader: R,
    writer: W,
}

impl<R, W> KafkaMempoolArchiver<R, W>
where
    R: EventReader,
    W: EventWriter,
{
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }

    pub async fn run(&mut self) -> Result<()> {
        info!("Starting Kafka bundle archiver");

        loop {
            match self.reader.read_event().await {
                Ok(event) => {
                    if let Err(e) = self.writer.archive_event(event).await {
                        error!(
                            error = %e,
                            "Failed to write event"
                        );
                    } else if let Err(e) = self.reader.commit().await {
                        error!(
                            error = %e,
                            "Failed to commit message"
                        );
                    }
                }
                Err(e) => {
                    error!(error = %e, "Error reading events");
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}
