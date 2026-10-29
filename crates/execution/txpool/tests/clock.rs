//! Tests receipt timestamps with an isolated process-wide replay clock.

use std::time::{Duration, Instant, SystemTime};

use alloy_consensus::{SignableTransaction, TxEip1559, transaction::Recovered};
use alloy_primitives::{Address, Signature};
use base_clock::{GlobalClock, OffsetClock};
use base_common_consensus::BaseTransactionSigned;
use base_execution_txpool::{BasePooledTransaction, TimestampedTransaction};

#[test]
fn received_at_uses_installed_clock_when_transaction_is_created() {
    let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let before = Instant::now();
    GlobalClock::set(OffsetClock::new(start)).unwrap();
    let signed: BaseTransactionSigned =
        TxEip1559::default().into_signed(Signature::test_signature()).into();

    let transaction: BasePooledTransaction =
        BasePooledTransaction::new(Recovered::new_unchecked(signed, Address::ZERO), 0);

    assert!(
        (1_000_000..=1_000_000 + before.elapsed().as_millis()).contains(&transaction.received_at())
    );
}
