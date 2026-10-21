//! Shared helpers for the metering unit tests.

use std::sync::Arc;

use alloy_consensus::{BlockHeader, Header};
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, B256, Bytes};
use base_bundles::{Bundle, ParsedBundle};
use base_common_consensus::{BaseBlock, BaseBlockBody, BaseTransactionSigned};
use base_common_evm::L1BlockInfo;
use base_node_runner::test_utils::TestHarness;
use eyre::Context;
use reth_provider::StateProviderFactory;
use reth_transaction_pool::test_utils::TransactionBuilder;

use crate::{MeterBundleInput, MeterBundleOutput, MeteredOpcodes, meter_bundle};

/// Builds signed transactions, bundle runs, and blocks for metering tests.
#[derive(Debug)]
pub struct TestSupport;

impl TestSupport {
    /// Signs `builder` as an EIP-1559 transaction.
    pub fn sign(builder: TransactionBuilder) -> BaseTransactionSigned {
        let signed = builder.into_eip1559();
        BaseTransactionSigned::Eip1559(signed.as_eip1559().expect("eip1559 transaction").clone())
    }

    /// Returns the EIP-2718 encoding of `tx`.
    pub fn encoded(tx: &BaseTransactionSigned) -> Bytes {
        Bytes::from(tx.encoded_2718())
    }

    /// Meters `txs` as a bundle on top of the harness's latest block.
    pub fn run_meter(
        harness: &TestHarness,
        txs: Vec<BaseTransactionSigned>,
        metered: MeteredOpcodes,
    ) -> eyre::Result<MeterBundleOutput> {
        let latest = harness.latest_block();
        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;
        let bundle = Bundle { txs: txs.iter().map(Self::encoded).collect() };

        meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: ParsedBundle::try_from(bundle).map_err(|e| eyre::eyre!(e))?,
            header: latest.sealed_header().clone(),
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(metered),
        })
    }

    /// Builds an unexecuted block with `transactions` on top of the harness's latest block.
    pub fn child_block(
        harness: &TestHarness,
        transactions: Vec<BaseTransactionSigned>,
    ) -> BaseBlock {
        let latest = harness.latest_block();
        let header = Header {
            parent_hash: latest.hash(),
            number: latest.number() + 1,
            timestamp: latest.timestamp() + 2,
            gas_limit: 30_000_000,
            beneficiary: Address::random(),
            base_fee_per_gas: Some(1),
            // Required for post-Cancun blocks (EIP-4788)
            parent_beacon_block_root: Some(B256::ZERO),
            ..Default::default()
        };

        BaseBlock::new(header, BaseBlockBody { transactions, ommers: vec![], withdrawals: None })
    }
}
