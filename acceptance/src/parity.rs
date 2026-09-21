//! Seeded transaction generation and sustained external-node parity checks.

use std::{env, time::Duration};

use alloy_consensus::SignableTransaction;
use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718};
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::Provider;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_rpc_types::BaseTransactionRequest;
use base_load_tests::{CalldataPayload, TransferPayload, WorkloadConfig, WorkloadGenerator};
use eyre::{Result, WrapErr, ensure, eyre};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout_at};

use crate::WorkloadContext;

const DEFAULT_SEED: u64 = 0x_BA5E_F0FF;
const NUM_TRANSACTIONS: u64 = 200;
const MINIMUM_ACCEPTED: u64 = 100;
const MAX_SEND_ATTEMPTS: u32 = 20;
const MAX_LAG: u64 = 2;
const PARITY_WINDOW: u32 = 5;
const SEND_BACKOFF: Duration = Duration::from_millis(250);
const PARITY_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Verdict for one builder/validator parity sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parity {
    /// Validator is within the lag allowance and agrees on the shared hash.
    InSync,
    /// Validator is absent, missing the shared block, or too far behind.
    Lagging,
    /// Validator has the shared height with a different canonical hash.
    Forked,
}

/// Generated transaction fields completed before signing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzedTransaction {
    /// Recipient selected by the workload.
    pub to: Address,
    /// Native value selected by the workload.
    pub value: U256,
    /// Seeded calldata selected by the workload.
    pub input: Bytes,
}

/// Seeded transfer/calldata transaction generator.
#[derive(Debug)]
pub struct FuzzTransactionGenerator {
    /// Weighted paved-road payload generator.
    pub generator: WorkloadGenerator,
}

impl FuzzTransactionGenerator {
    /// Creates the required 70% transfer and 30% calldata workload.
    pub fn new(seed: u64) -> Self {
        let generator =
            WorkloadGenerator::new(WorkloadConfig::new("sync-parity-fuzz").with_seed(seed))
                .with_payload(TransferPayload::default(), 0.7)
                .with_payload(CalldataPayload::new(256).with_min_size(0), 0.3);
        Self { generator }
    }

    /// Produces the next deterministic transaction shape.
    pub fn next(&mut self, from: Address, to: Address) -> Result<FuzzedTransaction> {
        let request = self
            .generator
            .generate_payload(from, to)
            .map_err(|error| eyre!("workload generation failed: {error}"))?;
        Ok(FuzzedTransaction {
            to,
            value: request.value.unwrap_or(U256::ZERO),
            input: request.input.input().cloned().unwrap_or_default(),
        })
    }
}

/// Implements the seeded fuzz and sync-parity acceptance workload.
#[derive(Debug)]
pub struct ParityWorkload;

impl ParityWorkload {
    /// Runs the full workload against externally provisioned builder and validator RPCs.
    pub async fn execute(context: &WorkloadContext<'_>) -> Result<Value> {
        let seed = Self::seed()?;
        let accepted = Self::submit(context, seed)
            .await
            .wrap_err_with(|| format!("fuzz submission failed (seed={seed:#x})"))?;
        ensure!(
            accepted >= MINIMUM_ACCEPTED,
            "too few transactions accepted (seed={seed:#x}): {accepted}/{NUM_TRANSACTIONS}"
        );
        let head = Self::assert_parity(context, seed).await?;
        Ok(
            json!({"seed": seed, "attempted": NUM_TRANSACTIONS, "accepted": accepted, "parity_head": head, "parity_samples": PARITY_WINDOW}),
        )
    }

    /// Reads a decimal or hexadecimal seed, using the reproducible default when unset.
    pub fn seed() -> Result<u64> {
        match env::var("FUZZ_SEED") {
            Ok(value) if !value.is_empty() => value
                .strip_prefix("0x")
                .map_or_else(|| value.parse::<u64>(), |hex| u64::from_str_radix(hex, 16))
                .map_err(eyre::Report::from),
            _ => Ok(DEFAULT_SEED),
        }
        .wrap_err("FUZZ_SEED is not a valid u64")
    }

    /// Completes and signs one EIP-1559 fuzz transaction.
    pub fn sign(
        signer: &PrivateKeySigner,
        transaction: &FuzzedTransaction,
        nonce: u64,
        chain_id: u64,
    ) -> Result<(Bytes, B256)> {
        let typed = BaseTransactionRequest::default()
            .from(signer.address())
            .to(transaction.to)
            .value(transaction.value)
            .with_input(transaction.input.clone())
            .transaction_type(2)
            .with_gas_limit(200_000)
            .with_max_fee_per_gas(1_000_000_000)
            .with_max_priority_fee_per_gas(0)
            .with_chain_id(chain_id)
            .with_nonce(nonce)
            .build_typed_tx()
            .map_err(|request| eyre!("invalid transaction request: {request:?}"))?;
        let signature = signer.sign_hash_sync(&typed.signature_hash())?;
        let signed = typed.into_signed(signature);
        Ok((signed.encoded_2718().into(), *signed.hash()))
    }

    /// Classifies one head and shared-hash sample.
    pub fn classify(
        builder_head: u64,
        validator_head: u64,
        builder_hash: B256,
        validator_hash: Option<B256>,
        max_lag: u64,
    ) -> Parity {
        if builder_head.saturating_sub(validator_head) > max_lag || validator_head == 0 {
            return Parity::Lagging;
        }
        match validator_hash {
            Some(hash) if hash == builder_hash => Parity::InSync,
            Some(_) => Parity::Forked,
            None => Parity::Lagging,
        }
    }

    /// Generates and submits the 200-transaction workload, returning accepted count.
    pub async fn submit(context: &WorkloadContext<'_>, seed: u64) -> Result<u64> {
        let provider = context.provider("builder")?;
        let mut accounts = Vec::new();
        for index in 1..=4 {
            let signer = WorkloadContext::signer(index)?;
            crate::TransactionWorkload::wait_funded(context, "validator", signer.address()).await?;
            let nonce = provider.get_transaction_count(signer.address()).await?;
            accounts.push((signer, nonce));
        }
        let mut generator = FuzzTransactionGenerator::new(seed);
        let mut accepted = 0;
        for index in 0..NUM_TRANSACTIONS {
            let slot = index as usize % accounts.len();
            let recipient = accounts[(slot + 1) % accounts.len()].0.address();
            let nonce = accounts[slot].1;
            let shape = generator.next(accounts[slot].0.address(), recipient)?;
            let (raw, expected) =
                Self::sign(&accounts[slot].0, &shape, nonce, context.config.devnet.l2.chain_id)?;
            if Self::send(context, &raw, expected).await? {
                accounts[slot].1 = nonce + 1;
                accepted += 1;
            } else {
                // A structured JSON-RPC rejection confirms the bytes were not ambiguously accepted.
                accounts[slot].1 = nonce;
            }
        }
        Ok(accepted)
    }

    /// Submits bytes, retrying only explicit pool-capacity rejections.
    pub async fn send(context: &WorkloadContext<'_>, raw: &Bytes, expected: B256) -> Result<bool> {
        for _ in 0..MAX_SEND_ATTEMPTS {
            // Transport failures escape immediately: retrying an ambiguous submission could
            // duplicate a transaction. Only an explicit full-pool RPC response is retried.
            let response = context
                .rpc
                .response(context.endpoint("builder")?, "eth_sendRawTransaction", json!([raw]))
                .await?;
            if let Some(error) = response.get("error") {
                if Self::is_full_pool(error) {
                    sleep(SEND_BACKOFF).await;
                    continue;
                }
                return Ok(false);
            }
            let returned: B256 = serde_json::from_value(
                response.get("result").cloned().ok_or_else(|| eyre!("missing send result"))?,
            )?;
            ensure!(returned == expected, "RPC hash differs from signed hash");
            return Ok(true);
        }
        Ok(false)
    }

    /// Recognizes only explicit transaction-pool capacity responses.
    pub fn is_full_pool(error: &Value) -> bool {
        error.get("message").and_then(Value::as_str).is_some_and(|message| {
            let message = message.to_ascii_lowercase();
            message.contains("txpool is full") || message.contains("transaction pool is full")
        })
    }

    /// Requires five consecutive matching samples within two blocks of lag.
    pub async fn assert_parity(context: &WorkloadContext<'_>, seed: u64) -> Result<u64> {
        let builder = context.provider("builder")?;
        let validator = context.provider("validator")?;
        timeout_at(context.deadline, async {
            let mut streak = 0;
            loop {
                let builder_head = builder.get_block_number().await?;
                let validator_head = validator.get_block_number().await?;
                let shared = builder_head.saturating_sub(MAX_LAG);
                let builder_hash = Self::block_hash(&builder, shared).await?;
                let validator_hash = if validator_head >= shared && validator_head > 0 {
                    Some(Self::block_hash(&validator, shared).await?)
                } else {
                    None
                };
                match Self::classify(
                    builder_head,
                    validator_head,
                    builder_hash,
                    validator_hash,
                    MAX_LAG,
                ) {
                    Parity::InSync => {
                        streak += 1;
                        if streak == PARITY_WINDOW {
                            return Ok(builder_head);
                        }
                    }
                    Parity::Lagging => streak = 0,
                    Parity::Forked => {
                        return Err(eyre!(
                            "divergence detected (seed={seed:#x}) at block {shared}: builder {builder_hash}, validator {validator_hash:?}"
                        ));
                    }
                }
                sleep(PARITY_POLL_INTERVAL).await;
            }
        })
        .await
        .wrap_err_with(|| format!("validator parity deadline elapsed (seed={seed:#x})"))?
    }

    /// Reads the canonical hash at an exact block number.
    pub async fn block_hash(
        provider: &alloy_provider::RootProvider<base_common_network::Base>,
        number: u64,
    ) -> Result<B256> {
        provider
            .get_block_by_number(BlockNumberOrTag::Number(number))
            .await?
            .map(|block| block.header.hash)
            .ok_or_else(|| eyre!("block {number} missing"))
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Transaction;
    use alloy_eips::eip2718::Decodable2718;
    use base_common_consensus::BaseTxEnvelope;

    use super::*;

    const H1: B256 = B256::repeat_byte(0x11);
    const H2: B256 = B256::repeat_byte(0x22);

    #[test]
    fn in_sync_when_within_lag_and_hashes_agree() {
        assert_eq!(ParityWorkload::classify(10, 9, H1, Some(H1), MAX_LAG), Parity::InSync);
        assert_eq!(ParityWorkload::classify(10, 10, H1, Some(H1), MAX_LAG), Parity::InSync);
    }

    #[test]
    fn lagging_when_validator_falls_behind() {
        assert_eq!(ParityWorkload::classify(50, 10, H1, Some(H1), MAX_LAG), Parity::Lagging);
        assert_eq!(ParityWorkload::classify(10, 0, H1, None, MAX_LAG), Parity::Lagging);
        assert_eq!(ParityWorkload::classify(10, 9, H1, None, MAX_LAG), Parity::Lagging);
    }

    #[test]
    fn forked_when_heights_match_but_hashes_differ() {
        assert_eq!(ParityWorkload::classify(10, 9, H1, Some(H2), MAX_LAG), Parity::Forked);
    }

    #[test]
    fn seeded_generation_is_repeatable_and_varied() {
        let from = Address::repeat_byte(0x11);
        let to = Address::repeat_byte(0x22);
        let mut first = FuzzTransactionGenerator::new(DEFAULT_SEED);
        let mut second = FuzzTransactionGenerator::new(DEFAULT_SEED);
        let left = (0..64).map(|_| first.next(from, to).unwrap()).collect::<Vec<_>>();
        let right = (0..64).map(|_| second.next(from, to).unwrap()).collect::<Vec<_>>();
        assert_eq!(left, right);
        assert!(left.iter().any(|transaction| transaction.input.is_empty()));
        assert!(left.iter().any(|transaction| !transaction.input.is_empty()));
        assert!(left.iter().all(|transaction| transaction.input.len() <= 256));
    }

    #[test]
    fn signing_binds_nonce_chain_and_payload() {
        let signer = WorkloadContext::signer(1).unwrap();
        let transaction = FuzzedTransaction {
            to: Address::repeat_byte(0x42),
            value: U256::from(7),
            input: Bytes::from_static(b"seeded"),
        };
        let first = ParityWorkload::sign(&signer, &transaction, 3, 99).unwrap();
        let decoded = BaseTxEnvelope::decode_2718(&mut first.0.as_ref()).unwrap();
        assert_eq!(decoded.chain_id(), Some(99));
        assert_eq!(decoded.nonce(), 3);
        assert_eq!(decoded.to(), Some(Address::repeat_byte(0x42)));
        assert_eq!(decoded.value(), U256::from(7));
        assert_eq!(decoded.input().as_ref(), b"seeded");
        assert_eq!(first, ParityWorkload::sign(&signer, &transaction, 3, 99).unwrap());
        assert_ne!(first, ParityWorkload::sign(&signer, &transaction, 4, 99).unwrap());
        assert_ne!(first, ParityWorkload::sign(&signer, &transaction, 3, 100).unwrap());
        let mut changed = transaction;
        changed.input = Bytes::from_static(b"different");
        assert_ne!(first, ParityWorkload::sign(&signer, &changed, 3, 99).unwrap());
    }

    #[test]
    fn retries_only_explicit_pool_capacity_errors() {
        assert!(ParityWorkload::is_full_pool(
            &json!({"code": -32000, "message": "txpool is full"})
        ));
        assert!(!ParityWorkload::is_full_pool(
            &json!({"code": -32000, "message": "nonce too low"})
        ));
        assert!(!ParityWorkload::is_full_pool(
            &json!({"code": -32603, "message": "upstream timed out"})
        ));
        assert!(!ParityWorkload::is_full_pool(&json!({"message": null})));
    }
}
