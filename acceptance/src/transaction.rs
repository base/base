//! Signed transaction and forwarding workloads for externally provisioned nodes.

use std::time::Duration;

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::{ReceiptResponse, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::Provider;
use alloy_signer::SignerSync;
use base_common_consensus::{Call, Eip8130Signed, TxEip8130};
use base_common_rpc_types::BaseTransactionRequest;
use base_execution_txpool::{DEFAULT_MAX_VALIDITY_PREDICATES, ValidityOperator, ValidityPredicate};
use eyre::{Result, WrapErr, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout_at};

use crate::{ForkActivation, ParityWorkload, ScenarioConfig, WorkloadContext};

const RECIPIENT: Address =
    Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xde, 0xad]);
const VALUE: u64 = 1_000_000_000;

/// Transaction cases migrated from the original system-test suite.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionCase {
    /// L1/L2 production and client-to-builder forwarding smoke test.
    SmokeBlockProductionAndTransactions,
    /// Direct insertion of a prevalidated transaction.
    InsertValidatedTransactionSingle,
    /// Normal client ingress and forwarding.
    ForwardingPipeline,
    /// Every validity predicate kind on a successful transaction.
    MatchingValidityPredicates,
    /// Direct builder validity ingress.
    DirectBuilderValidity,
    /// EIP-8130 with validity metadata through forwarding.
    Eip8130Validity,
    /// A transaction parked until a balance predicate becomes true.
    BalancePredicateTrigger,
    /// Future, expired, and recoverably false predicates.
    BlockPredicateLifecycle,
    /// Invalid predicate lists are rejected at ingress.
    InvalidValidityBatches,
    /// Forty interleaved transactions under deliberately constrained forwarding.
    ForwardingHighLoad,
    /// Seeded mixed transaction workload followed by sustained builder/validator parity.
    FuzzSyncParity,
    /// Minimal successful, self-paying EIP-8130 transaction.
    Eip8130Mined,
}

impl TransactionCase {
    /// Validates fork prerequisites before Docker is contacted.
    pub fn validate(&self, config: &ScenarioConfig) -> Result<()> {
        if matches!(
            self,
            Self::MatchingValidityPredicates
                | Self::DirectBuilderValidity
                | Self::Eip8130Validity
                | Self::BalancePredicateTrigger
                | Self::BlockPredicateLifecycle
                | Self::InvalidValidityBatches
                | Self::Eip8130Mined
        ) {
            ensure!(
                matches!(config.devnet.l2.forks.get("denim"), Some(ForkActivation::AtBlock { .. })),
                "transaction case requires Denim"
            );
        }
        if matches!(self, Self::Eip8130Validity | Self::Eip8130Mined) {
            ensure!(
                matches!(
                    config.devnet.l2.forks.get("zenith"),
                    Some(ForkActivation::AtBlock { .. })
                ),
                "EIP-8130 transaction case requires Zenith"
            );
        }
        if matches!(self, Self::ForwardingHighLoad) {
            let forwarding =
                config.devnet.l2.forwarding.as_ref().ok_or_else(|| {
                    eyre::eyre!("forwarding high load requires forwarding settings")
                })?;
            ensure!(forwarding.max_rps == 1, "forwarding high load requires max_rps = 1");
            ensure!(
                forwarding.resend_after.0 == Duration::from_secs(30),
                "forwarding high load requires resend_after = 30s"
            );
        }
        Ok(())
    }

    /// Roles that must be backed by distinct externally provisioned services.
    pub const fn required_roles(&self) -> &'static [&'static str] {
        match self {
            Self::SmokeBlockProductionAndTransactions => &["l1", "builder", "rpc"],
            Self::InsertValidatedTransactionSingle
            | Self::DirectBuilderValidity
            | Self::Eip8130Mined => &["builder"],
            Self::FuzzSyncParity => &["builder", "validator"],
            _ => &["builder", "rpc"],
        }
    }

    /// Executes the selected workload exactly once against external RPC endpoints.
    pub async fn execute(&self, context: &WorkloadContext<'_>) -> Result<Value> {
        match self {
            Self::SmokeBlockProductionAndTransactions => TransactionWorkload::smoke(context).await,
            Self::InsertValidatedTransactionSingle => {
                TransactionWorkload::insert_validated(context).await
            }
            Self::ForwardingPipeline => TransactionWorkload::simple_forward(context).await,
            Self::MatchingValidityPredicates => {
                TransactionWorkload::matching_validity(context).await
            }
            Self::DirectBuilderValidity => TransactionWorkload::direct_validity(context).await,
            Self::Eip8130Validity => TransactionWorkload::eip8130(context, true).await,
            Self::BalancePredicateTrigger => TransactionWorkload::balance_trigger(context).await,
            Self::BlockPredicateLifecycle => TransactionWorkload::block_lifecycle(context).await,
            Self::InvalidValidityBatches => TransactionWorkload::invalid_validity(context).await,
            Self::ForwardingHighLoad => TransactionWorkload::high_load(context).await,
            Self::FuzzSyncParity => ParityWorkload::execute(context).await,
            Self::Eip8130Mined => TransactionWorkload::eip8130(context, false).await,
        }
    }
}

/// Implements the transaction workload operations used by [`TransactionCase`].
#[derive(Debug)]
pub struct TransactionWorkload;

impl TransactionWorkload {
    /// Waits for a role to reach a target canonical height.
    pub async fn wait_height(context: &WorkloadContext<'_>, role: &str, target: u64) -> Result<()> {
        let provider = context.provider(role)?;
        timeout_at(context.deadline, async {
            loop {
                if provider.get_block_number().await? >= target {
                    return Ok(());
                }
                sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .wrap_err_with(|| format!("{role} did not reach block {target}"))?
    }

    /// Waits for an account's funded state to propagate to a role.
    pub async fn wait_funded(
        context: &WorkloadContext<'_>,
        role: &str,
        address: Address,
    ) -> Result<()> {
        let provider = context.provider(role)?;
        timeout_at(context.deadline, async {
            loop {
                if provider.get_balance(address).await? > U256::ZERO {
                    return Ok(());
                }
                sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .wrap_err("funded account did not propagate")?
    }

    /// Creates one signed EIP-1559 transfer.
    pub async fn signed_1559(
        context: &WorkloadContext<'_>,
        role: &str,
        index: u32,
        nonce: Option<u64>,
        to: Address,
        input: Bytes,
    ) -> Result<(Address, Bytes, B256)> {
        let signer = WorkloadContext::signer(index)?;
        let provider = context.provider(role)?;
        let nonce = match nonce {
            Some(nonce) => nonce,
            None => provider.get_transaction_count(signer.address()).await?,
        };
        let tx = BaseTransactionRequest::default()
            .from(signer.address())
            .to(to)
            .value(U256::from(VALUE))
            .with_input(input)
            .transaction_type(2)
            .with_gas_limit(200_000)
            .with_max_fee_per_gas(1_000_000_000)
            .with_max_priority_fee_per_gas(0)
            .with_chain_id(context.config.devnet.l2.chain_id)
            .with_nonce(nonce)
            .build_typed_tx()
            .map_err(|request| eyre::eyre!("invalid transaction request: {request:?}"))?;
        let signature = signer.sign_hash_sync(&tx.signature_hash())?;
        let signed = tx.into_signed(signature);
        Ok((signer.address(), signed.encoded_2718().into(), *signed.hash()))
    }

    /// Submits a transaction once and verifies the returned hash.
    pub async fn submit(
        context: &WorkloadContext<'_>,
        role: &str,
        raw: &Bytes,
        expected: B256,
    ) -> Result<()> {
        let returned = context.provider(role)?.send_raw_transaction(raw).await?;
        ensure!(*returned.tx_hash() == expected, "RPC hash differs from signed hash");
        Ok(())
    }

    /// Waits for and validates the observable receipt fields.
    pub async fn assert_receipt(
        context: &WorkloadContext<'_>,
        hash: B256,
        sender: Address,
        to: Option<Address>,
    ) -> Result<Value> {
        let receipt = context.receipt("builder", hash).await?;
        ensure!(receipt.transaction_hash() == hash, "receipt hash mismatch");
        ensure!(receipt.status(), "transaction execution failed");
        ensure!(receipt.from() == sender && receipt.to() == to, "receipt address mismatch");
        ensure!(receipt.block_number().is_some(), "receipt is not mined");
        Ok(serde_json::to_value(receipt)?)
    }

    /// Runs block-production and RPC-to-builder forwarding smoke coverage.
    pub async fn smoke(context: &WorkloadContext<'_>) -> Result<Value> {
        let l1 = context.rpc.call(context.endpoint("l1")?, "eth_blockNumber", json!([])).await?;
        let l2 = context.provider("builder")?.get_block_number().await?;
        Self::wait_height(context, "builder", l2 + 1).await?;
        loop {
            let now =
                context.rpc.call(context.endpoint("l1")?, "eth_blockNumber", json!([])).await?;
            if crate::Rpc::quantity(&now)? > crate::Rpc::quantity(&l1)? {
                break;
            }
            sleep(Duration::from_millis(500)).await;
        }
        let signer = WorkloadContext::signer(1)?;
        Self::wait_funded(context, "builder", signer.address()).await?;
        Self::wait_funded(context, "rpc", signer.address()).await?;
        let (sender, raw, hash) =
            Self::signed_1559(context, "rpc", 1, None, RECIPIENT, Bytes::new()).await?;
        Self::submit(context, "rpc", &raw, hash).await?;
        Ok(
            json!({"hash": hash, "receipt": Self::assert_receipt(context, hash, sender, Some(RECIPIENT)).await?}),
        )
    }

    /// Runs direct prevalidated builder ingress coverage.
    pub async fn insert_validated(context: &WorkloadContext<'_>) -> Result<Value> {
        Self::wait_height(context, "builder", 2).await?;
        Self::wait_funded(context, "builder", WorkloadContext::signer(1)?.address()).await?;
        let (sender, raw, hash) =
            Self::signed_1559(context, "builder", 1, None, RECIPIENT, Bytes::new()).await?;
        context
            .rpc
            .call(
                context.endpoint("builder")?,
                "base_insertValidatedTransaction",
                json!([{"sender": sender, "raw": raw}]),
            )
            .await?;
        Ok(
            json!({"hash": hash, "receipt": Self::assert_receipt(context, hash, sender, Some(RECIPIENT)).await?}),
        )
    }

    /// Runs ordinary RPC-node to builder forwarding coverage.
    pub async fn simple_forward(context: &WorkloadContext<'_>) -> Result<Value> {
        Self::wait_height(context, "builder", 3).await?;
        Self::wait_height(context, "rpc", 3).await?;
        let signer = WorkloadContext::signer(1)?;
        Self::wait_funded(context, "rpc", signer.address()).await?;
        let (sender, raw, hash) =
            Self::signed_1559(context, "rpc", 1, None, RECIPIENT, Bytes::new()).await?;
        Self::submit(context, "rpc", &raw, hash).await?;
        Ok(
            json!({"hash": hash, "receipt": Self::assert_receipt(context, hash, sender, Some(RECIPIENT)).await?}),
        )
    }

    /// Constructs a balance predicate using the authoritative serializable type.
    pub const fn balance(address: Address, op: ValidityOperator, value: U256) -> ValidityPredicate {
        ValidityPredicate::Balance { address, op, value }
    }
    /// Constructs a block-number predicate using the authoritative serializable type.
    pub fn block(op: ValidityOperator, value: u64) -> ValidityPredicate {
        ValidityPredicate::BlockNumber { op, value: U256::from(value) }
    }
    /// Constructs the storage predicate shared by validity cases.
    pub fn storage(address: Address, value: U256) -> ValidityPredicate {
        ValidityPredicate::Storage {
            address,
            slot: U256::from(1),
            mask: U256::MAX,
            op: ValidityOperator::Equal,
            value,
        }
    }

    /// Submits a validity transaction once and decodes its hash.
    pub async fn validity_submit(
        context: &WorkloadContext<'_>,
        role: &str,
        raw: Bytes,
        predicates: Vec<ValidityPredicate>,
    ) -> Result<B256> {
        Ok(serde_json::from_value(
            context
                .rpc
                .call(
                    context.endpoint(role)?,
                    "base_sendRawTransactionValidity",
                    json!([raw, {"validity": predicates}]),
                )
                .await?,
        )?)
    }

    /// Verifies all native predicate kinds through the forwarding pipeline.
    pub async fn matching_validity(context: &WorkloadContext<'_>) -> Result<Value> {
        let (sender, raw, hash) =
            Self::signed_1559(context, "rpc", 1, None, RECIPIENT, Bytes::new()).await?;
        let before = context.provider("builder")?.get_balance(RECIPIENT).await?;
        let current = context.provider("builder")?.get_block_number().await?;
        let returned = Self::validity_submit(
            context,
            "rpc",
            raw,
            vec![
                Self::balance(sender, ValidityOperator::GreaterThan, U256::ZERO),
                Self::storage(RECIPIENT, U256::ZERO),
                Self::block(ValidityOperator::GreaterThan, current),
                Self::block(ValidityOperator::LessThanOrEqual, current + 300),
            ],
        )
        .await?;
        ensure!(returned == hash, "validity RPC hash mismatch");
        let receipt = Self::assert_receipt(context, hash, sender, Some(RECIPIENT)).await?;
        ensure!(
            context.provider("builder")?.get_balance(RECIPIENT).await?
                == before + U256::from(VALUE),
            "unexpected state transition"
        );
        Ok(json!({"hash": hash, "receipt": receipt}))
    }

    /// Verifies validity ingress directly on the builder.
    pub async fn direct_validity(context: &WorkloadContext<'_>) -> Result<Value> {
        let (sender, raw, hash) =
            Self::signed_1559(context, "builder", 3, None, RECIPIENT, Bytes::new()).await?;
        let current = context.provider("builder")?.get_block_number().await?;
        let before = context.provider("builder")?.get_balance(RECIPIENT).await?;
        ensure!(
            Self::validity_submit(
                context,
                "builder",
                raw,
                vec![
                    Self::balance(sender, ValidityOperator::GreaterThan, U256::ZERO),
                    Self::block(ValidityOperator::LessThanOrEqual, current + 300),
                ]
            )
            .await?
                == hash,
            "validity RPC hash mismatch"
        );
        let receipt = Self::assert_receipt(context, hash, sender, Some(RECIPIENT)).await?;
        ensure!(
            context.provider("builder")?.get_balance(RECIPIENT).await?
                == before + U256::from(VALUE),
            "direct validity state transition differs"
        );
        Ok(json!({"hash": hash, "receipt": receipt}))
    }

    /// Verifies plain or validity-carrying EIP-8130 inclusion.
    pub async fn eip8130(context: &WorkloadContext<'_>, validity: bool) -> Result<Value> {
        let role = if validity { "rpc" } else { "builder" };
        let signer = WorkloadContext::signer(1)?;
        Self::wait_funded(context, role, signer.address()).await?;
        let nonce = context.provider(role)?.get_transaction_count(signer.address()).await?;
        let tx = TxEip8130 {
            chain_id: context.config.devnet.l2.chain_id,
            sender: None,
            nonce_key: U256::ZERO,
            nonce_sequence: nonce,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: 1_000_000_000,
            gas_limit: 200_000,
            account_changes: Vec::new(),
            calls: if validity {
                vec![vec![Call { to: Address::repeat_byte(0xde), data: Bytes::new() }]]
            } else {
                Vec::new()
            },
            metadata: Bytes::new(),
            payer: None,
        };
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash())?;
        let signed = Eip8130Signed::new(tx, signature.as_bytes().to_vec().into(), Bytes::new());
        let hash = *signed.hash();
        let raw: Bytes = signed.encoded_2718().into();
        ensure!(raw.first() == Some(&0x79), "EIP-8130 type byte missing");
        if validity {
            let current = context.provider(role)?.get_block_number().await?;
            ensure!(
                Self::validity_submit(
                    context,
                    role,
                    raw,
                    vec![
                        Self::balance(signer.address(), ValidityOperator::GreaterThan, U256::ZERO,),
                        Self::block(ValidityOperator::LessThanOrEqual, current + 300),
                    ]
                )
                .await?
                    == hash,
                "validity RPC hash mismatch"
            );
        } else {
            Self::submit(context, role, &raw, hash).await?;
        }
        let receipt = context.receipt("builder", hash).await?;
        ensure!(receipt.transaction_hash() == hash, "EIP-8130 receipt hash mismatch");
        ensure!(receipt.status(), "EIP-8130 execution failed");
        let value = serde_json::to_value(receipt)?;
        ensure!(crate::Rpc::quantity(&value["type"])? == 0x79, "receipt type is not 0x79");
        ensure!(value["payer"] == json!(signer.address()), "self-pay payer differs from sender");
        Ok(json!({"hash": hash, "receipt": value}))
    }

    /// Verifies a parked balance predicate wakes only after its triggering transaction.
    pub async fn balance_trigger(context: &WorkloadContext<'_>) -> Result<Value> {
        let watched = "0x1000000000000000000000000000000000000042".parse()?;
        ensure!(
            context.provider("builder")?.get_balance(watched).await? == U256::ZERO,
            "watched account is not fresh"
        );
        let (sender, raw, hash) =
            Self::signed_1559(context, "rpc", 1, None, RECIPIENT, Bytes::new()).await?;
        let current = context.provider("builder")?.get_block_number().await?;
        ensure!(
            Self::validity_submit(
                context,
                "rpc",
                raw,
                vec![
                    Self::balance(watched, ValidityOperator::GreaterThanOrEqual, U256::from(1)),
                    Self::block(ValidityOperator::LessThanOrEqual, current + 300),
                ]
            )
            .await?
                == hash,
            "validity hash mismatch"
        );
        Self::wait_pending(context, hash, true).await?;
        let at = context.provider("builder")?.get_block_number().await?;
        Self::wait_height(context, "builder", at + 2).await?;
        ensure!(
            context.provider("builder")?.get_transaction_receipt(hash).await?.is_none(),
            "false predicate transaction was included"
        );
        let (trigger_sender, trigger_raw, trigger_hash) =
            Self::signed_1559(context, "rpc", 2, None, watched, Bytes::new()).await?;
        Self::submit(context, "rpc", &trigger_raw, trigger_hash).await?;
        let trigger = context.receipt("builder", trigger_hash).await?;
        let receipt = context.receipt("builder", hash).await?;
        let trigger_position = (
            trigger.block_number().ok_or_else(|| eyre::eyre!("trigger receipt has no block"))?,
            trigger
                .transaction_index()
                .ok_or_else(|| eyre::eyre!("trigger receipt has no transaction index"))?,
        );
        let validity_position = (
            receipt.block_number().ok_or_else(|| eyre::eyre!("validity receipt has no block"))?,
            receipt
                .transaction_index()
                .ok_or_else(|| eyre::eyre!("validity receipt has no transaction index"))?,
        );
        ensure!(trigger_position <= validity_position, "predicate transaction preceded trigger");
        ensure!(
            context.provider("builder")?.get_balance(watched).await? >= U256::from(1),
            "trigger did not satisfy watched balance"
        );
        ensure!(receipt.from() == sender && trigger.from() == trigger_sender, "sender mismatch");
        Ok(json!({"validity_hash": hash, "trigger_hash": trigger_hash}))
    }

    /// Waits for a transaction's presence in the builder pool to match `present`.
    pub async fn wait_pending(
        context: &WorkloadContext<'_>,
        hash: B256,
        present: bool,
    ) -> Result<()> {
        let provider = context.provider("builder")?;
        timeout_at(context.deadline, async {
            loop {
                if provider.get_transaction_by_hash(hash).await?.is_some() == present {
                    return Ok(());
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .wrap_err("pending transaction state deadline")?
    }

    /// Verifies future, expired, and recoverable predicate lifecycle behavior.
    pub async fn block_lifecycle(context: &WorkloadContext<'_>) -> Result<Value> {
        let current = context.provider("builder")?.get_block_number().await?;
        let target = current + 50;
        let mut hashes = Vec::new();
        for (index, predicates) in [
            vec![
                Self::block(ValidityOperator::GreaterThanOrEqual, target),
                Self::block(ValidityOperator::LessThanOrEqual, current + 300),
            ],
            vec![
                Self::block(ValidityOperator::GreaterThanOrEqual, target + 1),
                Self::block(ValidityOperator::LessThanOrEqual, target),
            ],
            vec![
                Self::storage(RECIPIENT, U256::from(2)),
                Self::block(ValidityOperator::LessThanOrEqual, current + 300),
            ],
        ]
        .into_iter()
        .enumerate()
        {
            let (_, raw, hash) =
                Self::signed_1559(context, "rpc", index as u32 + 1, None, RECIPIENT, Bytes::new())
                    .await?;
            ensure!(
                Self::validity_submit(context, "rpc", raw, predicates).await? == hash,
                "validity hash mismatch"
            );
            Self::wait_pending(context, hash, true).await?;
            hashes.push(hash);
        }
        let future = context.receipt("builder", hashes[0]).await?;
        ensure!(
            future.block_number().is_some_and(|height| height >= target),
            "future transaction landed early"
        );
        Self::wait_height(context, "builder", target + 2).await?;
        ensure!(
            context.provider("builder")?.get_transaction_receipt(hashes[1]).await?.is_none(),
            "expired transaction was included"
        );
        Self::wait_pending(context, hashes[1], false).await?;
        ensure!(
            context.provider("builder")?.get_transaction_receipt(hashes[2]).await?.is_none(),
            "false storage transaction was included"
        );
        ensure!(
            context.provider("builder")?.get_transaction_by_hash(hashes[2]).await?.is_some(),
            "recoverable transaction was dropped"
        );
        Ok(json!({"future": hashes[0], "expired": hashes[1], "parked": hashes[2]}))
    }

    /// Verifies malformed predicate batches are rejected before forwarding.
    pub async fn invalid_validity(context: &WorkloadContext<'_>) -> Result<Value> {
        let (sender, raw, hash) =
            Self::signed_1559(context, "rpc", 1, None, RECIPIENT, Bytes::new()).await?;
        let repeated = Self::balance(sender, ValidityOperator::GreaterThan, U256::ZERO);
        let batches = [
            (Vec::new(), "validity predicates must not be empty"),
            (vec![repeated; DEFAULT_MAX_VALIDITY_PREDICATES + 1], "too many validity predicates"),
            (
                vec![ValidityPredicate::Storage {
                    address: RECIPIENT,
                    slot: U256::ZERO,
                    mask: U256::from(0xff),
                    op: ValidityOperator::Equal,
                    value: U256::from(0x100),
                }],
                "value bits set outside its mask",
            ),
        ];
        for (validity, expected) in batches {
            let error = match Self::validity_submit(context, "rpc", raw.clone(), validity).await {
                Ok(returned) => eyre::bail!("invalid predicates accepted as {returned}"),
                Err(error) => error,
            };
            ensure!(error.to_string().contains(expected), "unexpected validity rejection: {error}");
        }
        ensure!(
            context.provider("rpc")?.get_transaction_by_hash(hash).await?.is_none(),
            "rejected transaction entered RPC pool"
        );
        ensure!(
            context.provider("builder")?.get_transaction_by_hash(hash).await?.is_none(),
            "rejected transaction was forwarded"
        );
        Ok(json!({"hash": hash, "rejected_batches": 3}))
    }

    /// Sends forty interleaved transactions through a one-request-per-second forwarder.
    pub async fn high_load(context: &WorkloadContext<'_>) -> Result<Value> {
        let mut expected = Vec::new();
        let mut nonces = Vec::new();
        for index in 1..=4 {
            let signer = WorkloadContext::signer(index)?;
            Self::wait_funded(context, "rpc", signer.address()).await?;
            nonces.push(context.provider("rpc")?.get_transaction_count(signer.address()).await?);
        }
        for offset in 0..10 {
            for index in 1..=4 {
                let (sender, raw, hash) = Self::signed_1559(
                    context,
                    "rpc",
                    index,
                    Some(nonces[index as usize - 1] + offset),
                    RECIPIENT,
                    Bytes::new(),
                )
                .await?;
                Self::submit(context, "rpc", &raw, hash).await?;
                expected.push((sender, hash));
            }
        }
        for (sender, hash) in &expected {
            Self::assert_receipt(context, *hash, *sender, Some(RECIPIENT)).await?;
        }
        Ok(
            json!({"included": expected.len(), "hashes": expected.iter().map(|(_, hash)| hash).collect::<Vec<_>>()}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_case_names_are_strict_snake_case() {
        assert_eq!(
            serde_json::to_value(TransactionCase::BlockPredicateLifecycle).unwrap(),
            json!("block_predicate_lifecycle")
        );
        assert!(serde_json::from_value::<TransactionCase>(json!("unknown")).is_err());
    }

    #[test]
    fn high_load_requires_the_original_forwarding_constraints() {
        let mut config =
            ScenarioConfig::load("scenarios/system-transaction-high-load.toml").unwrap();
        assert!(TransactionCase::ForwardingHighLoad.validate(&config).is_ok());
        config.devnet.l2.forwarding.as_mut().unwrap().max_rps = 0;
        assert!(TransactionCase::ForwardingHighLoad.validate(&config).is_err());
        config.devnet.l2.forwarding.as_mut().unwrap().max_rps = 1;
        config.devnet.l2.forwarding.as_mut().unwrap().resend_after.0 = Duration::from_secs(2);
        assert!(TransactionCase::ForwardingHighLoad.validate(&config).is_err());
        config.devnet.l2.forwarding = None;
        assert!(TransactionCase::ForwardingHighLoad.validate(&config).is_err());
    }

    #[test]
    fn validity_wire_shape_preserves_mask_and_operator() {
        let predicate = TransactionWorkload::storage(RECIPIENT, U256::from(2));
        let encoded = serde_json::to_value(&predicate).unwrap();
        let decoded: ValidityPredicate = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, predicate);
        let malformed = ValidityPredicate::Storage {
            address: RECIPIENT,
            slot: U256::ZERO,
            mask: U256::from(0xff),
            op: ValidityOperator::Equal,
            value: U256::from(0x100),
        };
        assert!(malformed.validate_params(0).is_err());
    }
}
