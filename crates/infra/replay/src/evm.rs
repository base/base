use alloy_consensus::{Header, Sealed, transaction::Recovered};
use alloy_eips::eip2718::WithEncoded;
use alloy_evm::{
    EvmEnv, EvmFactory,
    block::{BlockExecutionResult, BlockExecutor, BlockExecutorFactory},
};
use alloy_primitives::U256;
use base_common_chains::ChainConfig;
use base_common_consensus::{BaseReceiptEnvelope, BaseTxEnvelope};
use base_common_evm::{
    AlloyReceiptBuilder, BaseBlockExecutionCtx, BaseBlockExecutorFactory, BaseEvmFactory,
    BaseSpecId,
};
use base_common_genesis::RollupConfig;
use base_proof_executor::TrieDB;
use eyre::{Result, eyre};
use revm::{
    context::{BlockEnv, CfgEnv},
    context_interface::block::BlobExcessGasAndPrice,
    database::{BundleState, State, states::bundle_state::BundleRetention},
};

use crate::{ExecutionWitness, Fixture};

/// Prepared EVM inputs using the proof executor's witness-backed [`TrieDB`], not the
/// sequencer's stateful storage provider. The EVM implementation is shared, but these
/// timings include proof-trie reads. The full proof stateless-builder benchmark separately
/// exercises parent-derived fee calculation and header construction.
#[derive(Debug)]
pub struct EvmWorkload<'a> {
    /// Mock state/oracle with content-addressed mainnet preimages.
    pub witness: &'a ExecutionWitness,
    /// Header anchoring the prestate.
    pub parent: Sealed<Header>,
    /// Canonical mainnet block environment.
    pub env: EvmEnv<BaseSpecId>,
    /// Mainnet fork configuration.
    pub config: RollupConfig,
    /// Block system-call inputs.
    pub context: BaseBlockExecutionCtx,
    /// Predecoded, signature-verified transactions with original bytes for L1 fees.
    pub transactions: Vec<WithEncoded<Recovered<BaseTxEnvelope>>>,
}

/// EVM output before trie updates. Its opened database can be reused for isolated root timings.
#[derive(Debug)]
pub struct ExecutedState<'a> {
    /// Database containing read paths opened during execution.
    pub db: TrieDB<&'a ExecutionWitness, &'a ExecutionWitness>,
    /// Actual account/storage changes from the mainnet block, including writes and deletions.
    pub bundle: BundleState,
    /// Receipts and gas accounting from production execution.
    pub result: BlockExecutionResult<BaseReceiptEnvelope>,
}

impl<'a> EvmWorkload<'a> {
    /// Prepares the mainnet inputs once, outside timing. Requires a full block witness.
    pub fn new(fixture: &'a Fixture) -> Result<Self> {
        let witness = fixture.execution.as_ref().ok_or_else(|| eyre!("no execution witness"))?;
        let config = ChainConfig::mainnet().rollup_config();
        let header = &fixture.header;
        let spec = BaseSpecId::from_timestamp(&config, header.timestamp);
        let env = EvmEnv::new(
            CfgEnv::new().with_chain_id(fixture.chain_id).with_spec_and_mainnet_gas_params(spec),
            BlockEnv {
                number: U256::from(header.number),
                beneficiary: header.beneficiary,
                timestamp: U256::from(header.timestamp),
                gas_limit: header.gas_limit,
                basefee: header.base_fee_per_gas.unwrap_or_default(),
                prevrandao: Some(header.mix_hash),
                blob_excess_gas_and_price: config
                    .is_ecotone_active(header.timestamp)
                    .then_some(BlobExcessGasAndPrice { excess_blob_gas: 0, blob_gasprice: 1 }),
                ..Default::default()
            },
        );
        Ok(Self {
            witness,
            parent: Sealed::new(fixture.parent.clone()),
            env,
            config,
            context: BaseBlockExecutionCtx {
                parent_hash: header.parent_hash,
                parent_beacon_block_root: header.parent_beacon_block_root,
                extra_data: Default::default(),
            },
            transactions: fixture
                .execution_payload()?
                .recovered_transactions_with_encoded()
                .collect::<Result<_, _>>()?,
        })
    }

    /// Runs system calls, transactions, receipts and state transition merging on a cold trie DB.
    /// Signature recovery, fixture loading and state-root reconstruction are excluded.
    pub fn execute(&self) -> Result<ExecutedState<'a>> {
        let mut db = TrieDB::new(self.parent.clone(), self.witness, self.witness);
        let mut state = State::builder().with_database(&mut db).with_bundle_update().build();
        let factory = BaseBlockExecutorFactory::new(
            AlloyReceiptBuilder::default(),
            self.config.clone(),
            BaseEvmFactory::default(),
        );
        let evm = factory.evm_factory().create_evm(&mut state, self.env.clone());
        let result = factory
            .create_executor(evm, self.context.clone())
            .execute_block(self.transactions.iter())?;
        state.merge_transitions(BundleRetention::Reverts);
        let bundle = state.take_bundle();
        drop(state);
        Ok(ExecutedState { db, bundle, result })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use base_proof_executor::compute_receipts_root;

    use super::*;

    #[test]
    fn mainnet_execution_matches_commitments_and_rejects_incomplete_state() {
        let mut fixture = Fixture::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/mainnet-execution.json.zst"
        ))
        .unwrap();
        // Full production builder must reproduce the RPC-sourced mainnet header exactly.
        fixture.validate().unwrap();
        let workload = EvmWorkload::new(&fixture).unwrap();
        let mut executed = workload.execute().unwrap();
        assert_eq!(executed.result.gas_used, 41_070_754);
        assert_eq!(executed.db.state_root(&executed.bundle).unwrap(), fixture.header.state_root);
        assert_eq!(
            compute_receipts_root(
                &executed.result.receipts,
                &workload.config,
                fixture.header.timestamp
            ),
            fixture.header.receipts_root
        );
        drop(executed);
        drop(workload);

        let mut witness = fixture.execution.take().unwrap();
        let root = witness.nodes.remove(&fixture.parent.state_root).unwrap();
        assert!(witness.execute(&fixture).is_err());
        witness.nodes.insert(fixture.parent.state_root, root);
        let code = witness.codes.values_mut().next().unwrap();
        *code = Bytes::from_static(&[0xff]);
        assert!(witness.verify(&fixture).is_err());
        fixture.transactions.remove(1);
        assert!(fixture.execution_payload().is_err());
    }
}
