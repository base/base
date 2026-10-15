use alloy_primitives::{Address, B256, U256, uint};
use alloy_sol_types::SolCall;
use async_trait::async_trait;
use base_tx_manager::{SendResponse, TxCandidate, TxManagerError};
use thiserror::Error;
use tracing::info;

use crate::{BalanceTracker, FeeDisburser, RegistrarController};

/// Fee collection action and its precondition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeJobKind {
    /// Disburse L2 fees after the contract's interval has elapsed.
    Disburse,
    /// Process L1 fees once the tracker holds at least 1 ETH.
    Process,
    /// Sweep L2 registrar revenue once it holds at least 0.01 ETH.
    Sweep,
}

/// Result of a completed one-shot job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeJobOutcome {
    /// The precondition was false; no transaction was sent.
    Skipped,
    /// A transaction executed successfully at the configured confirmation depth.
    Confirmed(B256),
}

/// A failed job, including execution failure in an otherwise mined transaction.
#[derive(Debug, Error)]
pub enum FeeJobError {
    /// A precondition could not be read.
    #[error("failed to read fee job precondition: {0:#}")]
    Read(eyre::Report),
    /// Transaction submission or confirmation failed.
    #[error(transparent)]
    Send(#[from] TxManagerError),
    /// A confirmed transaction reverted during execution.
    #[error("fee job transaction reverted: {0}")]
    Reverted(B256),
}

/// Reads preconditions at the latest block, not the pending block.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait FeeReader: Send + Sync {
    /// Reads the last disbursement time and interval from the contract.
    async fn disbursement_window(&self, contract: Address) -> eyre::Result<(U256, U256)>;
    /// Reads the latest block timestamp.
    async fn latest_timestamp(&self) -> eyre::Result<u64>;
    /// Reads a contract's ETH balance at the latest block.
    async fn balance(&self, contract: Address) -> eyre::Result<U256>;
}

/// Sends a candidate and waits for a receipt without interpreting execution status.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait FeeSender: Send + Sync {
    /// Submits a transaction and waits for its configured confirmation depth.
    async fn send(&self, candidate: TxCandidate) -> SendResponse;
}

/// One fee collection job targeting a single contract.
#[derive(Debug, Clone, Copy)]
pub struct FeeJob {
    /// Action to run.
    pub kind: FeeJobKind,
    /// Contract receiving the transaction.
    pub contract: Address,
}

impl FeeJob {
    /// Minimum balance for L1 fee processing, in wei.
    pub const MIN_PROCESS_BALANCE: U256 = uint!(1_000_000_000_000_000_000_U256);
    /// Minimum registrar balance for sweeping, in wei.
    pub const MIN_SWEEP_BALANCE: U256 = uint!(10_000_000_000_000_000_U256);

    /// Checks the precondition and, when eligible, sends and checks the receipt.
    pub async fn run<R: FeeReader, S: FeeSender>(
        &self,
        reader: &R,
        sender: &S,
    ) -> Result<FeeJobOutcome, FeeJobError> {
        let ready = match self.kind {
            FeeJobKind::Disburse => {
                let (last, interval) =
                    reader.disbursement_window(self.contract).await.map_err(FeeJobError::Read)?;
                let timestamp = reader.latest_timestamp().await.map_err(FeeJobError::Read)?;
                let next = last.checked_add(interval);
                let ready = next.is_some_and(|next| U256::from(timestamp) >= next);
                if !ready {
                    info!(contract = %self.contract, timestamp, last = %last, interval = %interval, next = ?next,
                        "fee disbursement interval has not elapsed");
                }
                ready
            }
            FeeJobKind::Process | FeeJobKind::Sweep => {
                let balance = reader.balance(self.contract).await.map_err(FeeJobError::Read)?;
                let minimum_balance = if self.kind == FeeJobKind::Process {
                    Self::MIN_PROCESS_BALANCE
                } else {
                    Self::MIN_SWEEP_BALANCE
                };
                let ready = balance >= minimum_balance;
                if !ready {
                    info!(job = ?self.kind, contract = %self.contract, balance = %balance, minimum_balance = %minimum_balance,
                        "contract balance is below fee job minimum");
                }
                ready
            }
        };
        if !ready {
            return Ok(FeeJobOutcome::Skipped);
        }
        let tx_data = match self.kind {
            FeeJobKind::Disburse => FeeDisburser::disburseFeesCall {}.abi_encode(),
            FeeJobKind::Process => BalanceTracker::processFeesCall {}.abi_encode(),
            FeeJobKind::Sweep => RegistrarController::withdrawETHCall {}.abi_encode(),
        };
        let receipt = sender
            .send(TxCandidate {
                tx_data: tx_data.into(),
                to: Some(self.contract),
                ..TxCandidate::default()
            })
            .await?;
        let tx_hash = receipt.transaction_hash;
        info!(job = ?self.kind, tx_hash = %tx_hash, "fee job transaction sent");
        let success = receipt.inner.status();
        info!(job = ?self.kind, tx_hash = %tx_hash, block = ?receipt.block_number, success,
            "fee job transaction confirmed");
        if !success {
            return Err(FeeJobError::Reverted(tx_hash));
        }
        Ok(FeeJobOutcome::Confirmed(tx_hash))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256};
    use alloy_rpc_types_eth::TransactionReceipt;
    use base_tx_manager::TxManagerError;
    use rstest::rstest;

    use super::{FeeJob, FeeJobError, FeeJobKind, FeeJobOutcome, MockFeeReader, MockFeeSender};

    #[derive(Debug)]
    struct TestReceipt;

    impl TestReceipt {
        fn build(success: bool, hash: B256) -> TransactionReceipt {
            serde_json::from_value(serde_json::json!({
                "type": "0x2", "status": if success { "0x1" } else { "0x0" },
                "cumulativeGasUsed": "0x5208", "logs": [], "logsBloom": format!("0x{}", "00".repeat(256)),
                "transactionHash": hash, "transactionIndex": "0x0",
                "blockHash": B256::repeat_byte(1), "blockNumber": "0x1",
                "gasUsed": "0x5208", "effectiveGasPrice": "0x1", "from": Address::ZERO,
                "to": Address::ZERO, "contractAddress": null
            })).unwrap()
        }
    }

    #[rstest]
    #[case::before_interval(129, false)]
    #[case::at_interval(130, true)]
    #[tokio::test]
    async fn disburse_boundary(#[case] timestamp: u64, #[case] eligible: bool) {
        let mut reader = MockFeeReader::new();
        reader.expect_disbursement_window().returning(|_| Ok((U256::from(100), U256::from(30))));
        reader.expect_latest_timestamp().returning(move || Ok(timestamp));
        let mut sender = MockFeeSender::new();
        sender
            .expect_send()
            .withf(|candidate| {
                candidate.to == Some(Address::ZERO)
                    && candidate.tx_data.as_ref() == [0xb8, 0x7e, 0xa8, 0xd4]
                    && candidate.value == U256::ZERO
            })
            .times(usize::from(eligible))
            .returning(|_| Ok(TestReceipt::build(true, B256::ZERO)));
        let job = FeeJob { kind: FeeJobKind::Disburse, contract: Address::ZERO };
        let outcome = job.run(&reader, &sender).await.unwrap();
        assert_eq!(
            outcome,
            if eligible { FeeJobOutcome::Confirmed(B256::ZERO) } else { FeeJobOutcome::Skipped }
        );
    }

    #[tokio::test]
    async fn disburse_overflow_does_not_send() {
        let mut reader = MockFeeReader::new();
        reader.expect_disbursement_window().returning(|_| Ok((U256::MAX, U256::from(1))));
        reader.expect_latest_timestamp().returning(|| Ok(u64::MAX));
        let sender = MockFeeSender::new();
        let job = FeeJob { kind: FeeJobKind::Disburse, contract: Address::ZERO };
        assert_eq!(job.run(&reader, &sender).await.unwrap(), FeeJobOutcome::Skipped);
    }

    #[rstest]
    #[case::process_below(FeeJobKind::Process, 999_999_999_999_999_999, false)]
    #[case::process_at(FeeJobKind::Process, 1_000_000_000_000_000_000, true)]
    #[case::sweep_below(FeeJobKind::Sweep, 9_999_999_999_999_999, false)]
    #[case::sweep_at(FeeJobKind::Sweep, 10_000_000_000_000_000, true)]
    #[tokio::test]
    async fn balance_boundary(
        #[case] kind: FeeJobKind,
        #[case] balance: u64,
        #[case] eligible: bool,
    ) {
        let mut reader = MockFeeReader::new();
        reader.expect_balance().returning(move |_| Ok(U256::from(balance)));
        let mut sender = MockFeeSender::new();
        sender
            .expect_send()
            .withf(move |candidate| {
                let selector = if kind == FeeJobKind::Process {
                    [0xba, 0x69, 0xeb, 0xed]
                } else {
                    [0xe0, 0x86, 0xe5, 0xec]
                };
                candidate.to == Some(Address::ZERO)
                    && candidate.tx_data.as_ref() == selector
                    && candidate.value == U256::ZERO
            })
            .times(usize::from(eligible))
            .returning(|_| Ok(TestReceipt::build(true, B256::ZERO)));
        let job = FeeJob { kind, contract: Address::ZERO };
        let outcome = job.run(&reader, &sender).await.unwrap();
        assert_eq!(
            outcome,
            if eligible { FeeJobOutcome::Confirmed(B256::ZERO) } else { FeeJobOutcome::Skipped }
        );
    }

    #[rstest]
    #[case::success(true)]
    #[case::revert(false)]
    #[tokio::test]
    async fn receipt_execution_controls_job_result(#[case] success: bool) {
        let receipt = TestReceipt::build(success, B256::repeat_byte(7));
        let mut reader = MockFeeReader::new();
        reader.expect_balance().returning(|_| Ok(U256::MAX));
        let mut sender = MockFeeSender::new();
        sender.expect_send().once().return_once(|_| Ok(receipt));
        let job = FeeJob { kind: FeeJobKind::Process, contract: Address::ZERO };
        let result = job.run(&reader, &sender).await;
        if success {
            assert_eq!(result.unwrap(), FeeJobOutcome::Confirmed(B256::repeat_byte(7)));
        } else {
            assert!(
                matches!(result, Err(FeeJobError::Reverted(hash)) if hash == B256::repeat_byte(7))
            );
        }
    }

    #[rstest]
    #[case::send_timeout(TxManagerError::SendTimeout)]
    #[case::send_revert(TxManagerError::ExecutionReverted { reason: Some("not authorized".to_string()), data: None })]
    #[tokio::test]
    async fn failed_send_fails_job(#[case] error: TxManagerError) {
        let mut reader = MockFeeReader::new();
        reader.expect_balance().returning(|_| Ok(U256::MAX));
        let mut sender = MockFeeSender::new();
        let expected = error.clone();
        sender.expect_send().once().return_once(move |_| Err(error));
        let job = FeeJob { kind: FeeJobKind::Process, contract: Address::ZERO };
        assert!(
            matches!(job.run(&reader, &sender).await, Err(FeeJobError::Send(error)) if error == expected)
        );
    }

    #[tokio::test]
    async fn failed_precondition_read_fails_without_send() {
        let mut reader = MockFeeReader::new();
        reader.expect_balance().returning(|_| {
            Err(eyre::eyre!("RPC unavailable").wrap_err("failed to read contract balance"))
        });
        let sender = MockFeeSender::new();
        let job = FeeJob { kind: FeeJobKind::Sweep, contract: Address::ZERO };
        let error = job.run(&reader, &sender).await.unwrap_err();
        assert!(matches!(&error, FeeJobError::Read(_)));
        let output = format!("Error: {:?}", eyre::Report::new(error));
        assert!(output.contains("failed to read contract balance"));
        assert!(output.contains("RPC unavailable"));
    }
}
