use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
};

use parking_lot::RwLock;
use reth_db_api::{Database, DatabaseError};

use crate::tx::{MemTx, MemTxMut};

/// Per-table row storage: `primary_key_bytes → sort_key_bytes → compressed_value_bytes`.
pub type TableData = BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, Vec<u8>>>;

/// Shared `MemDb` storage handle: `table_name → TableData`.
pub type SharedStore = Arc<RwLock<BTreeMap<&'static str, TableData>>>;

/// In-memory [`Database`] backed by `BTreeMap`.
#[derive(Clone, Debug, Default)]
pub struct MemDb {
    store: SharedStore,
}

impl MemDb {
    /// Create a new empty in-memory database.
    pub fn new() -> Self {
        Self { store: Arc::new(RwLock::new(BTreeMap::new())) }
    }
}

impl Database for MemDb {
    type TX = MemTx;
    type TXMut = MemTxMut;

    fn tx(&self) -> Result<Self::TX, DatabaseError> {
        Ok(MemTx::new(Arc::clone(&self.store)))
    }

    fn tx_mut(&self) -> Result<Self::TXMut, DatabaseError> {
        Ok(MemTxMut::new(Arc::clone(&self.store)))
    }

    fn path(&self) -> PathBuf {
        PathBuf::from(":memory:")
    }

    fn oldest_reader_txnid(&self) -> Option<u64> {
        None
    }

    fn last_txnid(&self) -> Option<u64> {
        None
    }
}
