//! Safe MDBX environments, transactions, and cursors.

pub extern crate base_execution_state_mdbx_sys as ffi;

#[cfg(feature = "read-tx-timeouts")]
pub use crate::native_mdbx::environment::read_transactions::MaxReadTransactionDuration;
pub use crate::native_mdbx::{
    codec::*,
    cursor::{Cursor, Iter, IterDup},
    database::Database,
    environment::{
        Environment, EnvironmentBuilder, EnvironmentKind, Geometry, HandleSlowReadersCallback,
        HandleSlowReadersReturnCode, Info, PageSize, Stat,
    },
    error::{Error, Result},
    flags::*,
    transaction::{CommitLatency, RO, RW, Transaction, TransactionKind},
};

mod codec;
mod cursor;
pub use cursor::IntoIter as CursorIntoIter;
mod database;
mod environment;
pub use environment::{GeometryInfo, PageOps};
mod error;
mod flags;
mod transaction;
pub use transaction::Sealed;
mod txn_manager;
mod txn_pool;

#[cfg(test)]
mod tests {
    use byteorder::{ByteOrder, LittleEndian};
    use tempfile::tempdir;

    use super::*;

    /// Regression test for <https://github.com/danburkert/lmdb-rs/issues/21>.
    /// This test reliably segfaults when run against lmdb compiled with opt level -O3 and newer
    /// GCC compilers.
    #[test]
    fn issue_21_regression() {
        const HEIGHT_KEY: [u8; 1] = [0];

        let dir = tempdir().unwrap();

        let env = {
            let mut builder = Environment::builder();
            builder.set_max_dbs(2);
            builder
                .set_geometry(Geometry { size: Some(1_000_000..1_000_000), ..Default::default() });
            builder.open(dir.path()).expect("open mdbx env")
        };

        for height in 0..1000 {
            let mut value = [0u8; 8];
            LittleEndian::write_u64(&mut value, height);
            let tx = env.begin_rw_txn().expect("begin_rw_txn");
            let index = tx.create_db(None, DatabaseFlags::DUP_SORT).expect("open index db");
            tx.put(index.dbi(), HEIGHT_KEY, value, WriteFlags::empty()).expect("tx.put");
            tx.commit().expect("tx.commit");
        }
    }
}
