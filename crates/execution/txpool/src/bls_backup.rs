use std::{fs, io, path::PathBuf};

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::Bytes;
use reth_transaction_pool::{PoolTransaction, TransactionOrigin, TransactionPool};
use tracing::{debug, info, warn};

use crate::BasePooledTx;

/// Restart backup of the pool's local native BLS transfers (`0x7b`) in their pooled form.
///
/// Reth's local transaction backup stores the consensus form, which drops a transfer's BLS
/// witness, so a reloaded transfer could never be admitted again. This backup stores the
/// pooled form, witness included, and readmits it through ordinary validation.
#[derive(Debug, Clone)]
pub struct BlsTransferBackup {
    path: PathBuf,
}

impl BlsTransferBackup {
    /// Creates a backup stored at `path`.
    pub const fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Writes the pooled encodings of the pool's local native BLS transfers, replacing any
    /// previous backup. Returns the number saved.
    pub fn save<P>(&self, pool: &P) -> io::Result<usize>
    where
        P: TransactionPool<Transaction: BasePooledTx>,
    {
        let transfers: Vec<Bytes> = pool
            .get_local_transactions()
            .into_iter()
            .filter(|tx| tx.transaction.as_bls_transfer().is_some())
            .filter_map(|tx| tx.transaction.clone_into_pooled().ok())
            .map(|pooled| pooled.inner().encoded_2718().into())
            .collect();
        if transfers.is_empty() {
            return match fs::remove_file(&self.path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                _ => Ok(0),
            };
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.path, serde_json::to_vec(&transfers)?)?;
        info!(count = transfers.len(), "saved local native BLS transfers");
        Ok(transfers.len())
    }

    /// Readmits the backed-up transfers as local transactions and removes the backup. Returns
    /// the number readmitted; transfers that no longer validate are dropped.
    pub async fn restore<P>(&self, pool: &P) -> io::Result<usize>
    where
        P: TransactionPool<Transaction: BasePooledTx>,
    {
        let data = match fs::read(&self.path) {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let transfers: Vec<Bytes> = serde_json::from_slice(&data)?;
        let mut restored = 0;
        for raw in transfers {
            let transaction = match P::Transaction::recover_raw_transaction(&raw) {
                Ok(transaction) => transaction,
                Err(error) => {
                    warn!(error = %error, "dropping undecodable native BLS transfer backup");
                    continue;
                }
            };
            match pool.add_transaction(TransactionOrigin::Local, transaction).await {
                Ok(_) => restored += 1,
                Err(error) => debug!(error = %error, "dropping stale native BLS transfer backup"),
            }
        }
        fs::remove_file(&self.path)?;
        info!(count = restored, "restored local native BLS transfers");
        Ok(restored)
    }
}
