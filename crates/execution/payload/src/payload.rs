//! Payload related types

use std::{fmt::Debug, sync::Arc};

use alloy_consensus::{Block, BlockHeader};
use alloy_eips::{
    eip1559::BaseFeeParams,
    eip2718::{Decodable2718, Encodable2718},
    eip4895::Withdrawals,
    eip7685::Requests,
};
use alloy_primitives::{Address, B64, B256, Bytes, U256};
use alloy_rpc_types_engine::{
    BlobsBundleV1, BlobsBundleV2, ExecutionPayloadEnvelopeV2, ExecutionPayloadFieldV2,
    ExecutionPayloadV1, ExecutionPayloadV3, PayloadAttributes as EthPayloadAttributes,
    PayloadError, PayloadId,
};
use base_common_chains::Upgrades;
use base_common_consensus::{
    BasePrimitives, EIP1559ParamError, HoloceneExtraData, JovianExtraData, decode_2718_canonical,
};
/// Re-export for use in downstream arguments.
pub use base_common_rpc_types_engine::BasePayloadAttributes;
use base_common_rpc_types_engine::{
    BaseExecutionPayloadEnvelopeV3, BaseExecutionPayloadEnvelopeV4, BaseExecutionPayloadEnvelopeV5,
    BaseExecutionPayloadEnvelopeV6, BaseExecutionPayloadV4, BaseExecutionPayloadV5,
    BasePayloadError,
};
use base_execution_evm::BaseNextBlockEnvAttributes;
use reth_chainspec::EthChainSpec;
use reth_payload_builder::PayloadBuilderError;
use reth_payload_primitives::{BuildNextEnv, BuiltPayload, BuiltPayloadExecutedBlock};
use reth_primitives_traits::{
    Block as _, NodePrimitives, SealedBlock, SealedHeader, SignedTransaction, WithEncoded,
};

/// Minimal Ethereum payload builder attributes retained for Base payload construction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EthPayloadBuilderAttributes {
    /// Payload job ID.
    pub id: PayloadId,
    /// Parent block hash.
    pub parent: B256,
    /// Timestamp for the payload.
    pub timestamp: u64,
    /// Suggested fee recipient.
    pub suggested_fee_recipient: Address,
    /// Prev-randao value for the payload.
    pub prev_randao: B256,
    /// Whether withdrawals were provided in the original payload attributes.
    pub has_withdrawals: bool,
    /// Withdrawals included in the payload.
    pub withdrawals: Withdrawals,
    /// Parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,
    /// Slot number for the payload.
    pub slot_number: Option<u64>,
}

/// Base Payload Builder Attributes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasePayloadBuilderAttributes<T> {
    /// Inner ethereum payload builder attributes
    pub payload_attributes: EthPayloadBuilderAttributes,
    /// `NoTxPool` option for the generated payload
    pub no_tx_pool: bool,
    /// Decoded transactions and the original EIP-2718 encoded bytes as received in the payload
    /// attributes.
    pub transactions: Vec<WithEncoded<T>>,
    /// The gas limit for the generated payload
    pub gas_limit: Option<u64>,
    /// EIP-1559 parameters for the generated payload
    pub eip_1559_params: Option<B64>,
    /// Min base fee for the generated payload (only available post-Jovian)
    pub min_base_fee: Option<u64>,
}

impl<T> Default for BasePayloadBuilderAttributes<T> {
    fn default() -> Self {
        Self {
            payload_attributes: Default::default(),
            no_tx_pool: Default::default(),
            gas_limit: Default::default(),
            eip_1559_params: Default::default(),
            transactions: Default::default(),
            min_base_fee: Default::default(),
        }
    }
}

impl<T> BasePayloadBuilderAttributes<T> {
    /// Converts these builder attributes back into the RPC payload attribute representation.
    pub fn as_rpc_payload_attributes(&self) -> BasePayloadAttributes {
        BasePayloadAttributes {
            payload_attributes: EthPayloadAttributes {
                timestamp: self.payload_attributes.timestamp,
                prev_randao: self.payload_attributes.prev_randao,
                suggested_fee_recipient: self.payload_attributes.suggested_fee_recipient,
                withdrawals: self
                    .payload_attributes
                    .has_withdrawals
                    .then(|| self.payload_attributes.withdrawals.to_vec()),
                parent_beacon_block_root: self.payload_attributes.parent_beacon_block_root,
                slot_number: self.payload_attributes.slot_number,
                target_gas_limit: None,
            },
            transactions: (!self.transactions.is_empty())
                .then(|| self.transactions.iter().map(|tx| tx.encoded_bytes().clone()).collect()),
            no_tx_pool: Some(self.no_tx_pool),
            gas_limit: self.gas_limit,
            eip_1559_params: self.eip_1559_params,
            min_base_fee: self.min_base_fee,
        }
    }

    /// Extracts the extra data parameters post-Holocene upgrade.
    /// In Holocene, those parameters are the EIP-1559 base fee parameters.
    pub fn get_holocene_extra_data(
        &self,
        default_base_fee_params: BaseFeeParams,
    ) -> Result<Bytes, EIP1559ParamError> {
        self.eip_1559_params
            .map(|params| HoloceneExtraData::encode(params, default_base_fee_params))
            .ok_or(EIP1559ParamError::NoEIP1559Params)?
    }

    /// Extracts the extra data parameters post-Jovian upgrade.
    /// Those parameters are the EIP-1559 parameters from Holocene and the minimum base fee.
    pub fn get_jovian_extra_data(
        &self,
        default_base_fee_params: BaseFeeParams,
    ) -> Result<Bytes, EIP1559ParamError> {
        let min_base_fee = self.min_base_fee.ok_or(EIP1559ParamError::MinBaseFeeNotSet)?;
        self.eip_1559_params
            .map(|params| JovianExtraData::encode(params, default_base_fee_params, min_base_fee))
            .ok_or(EIP1559ParamError::NoEIP1559Params)?
    }

    /// Extracts the Holocene EIP-1559 parameters from the encoded form.
    ///
    /// Returns (`elasticity`, `denominator`).
    pub fn decode_eip_1559_params(&self) -> Option<(u32, u32)> {
        self.eip_1559_params.map(HoloceneExtraData::decode_params)
    }
}

impl<T: Decodable2718 + Encodable2718 + Send + Sync + Debug + Unpin + 'static>
    BasePayloadBuilderAttributes<T>
{
    /// Creates payload builder attributes for the given parent block and RPC payload attributes.
    pub fn try_new(
        parent: B256,
        attributes: BasePayloadAttributes,
        version: u8,
    ) -> Result<Self, alloy_rlp::Error> {
        if attributes.payload_attributes.target_gas_limit.is_some() {
            return Err(alloy_rlp::Error::Custom(
                "targetGasLimit is not supported by Base payload attributes",
            ));
        }

        let id = attributes.payload_id(&parent, version);

        let transactions = attributes
            .transactions
            .unwrap_or_default()
            .into_iter()
            .map(|data| decode_2718_canonical(data.as_ref()).map(|tx| WithEncoded::new(data, tx)))
            .collect::<Result<_, _>>()?;

        let payload_attributes = EthPayloadBuilderAttributes {
            id,
            parent,
            timestamp: attributes.payload_attributes.timestamp,
            suggested_fee_recipient: attributes.payload_attributes.suggested_fee_recipient,
            prev_randao: attributes.payload_attributes.prev_randao,
            has_withdrawals: attributes.payload_attributes.withdrawals.is_some(),
            withdrawals: attributes.payload_attributes.withdrawals.unwrap_or_default().into(),
            parent_beacon_block_root: attributes.payload_attributes.parent_beacon_block_root,
            slot_number: attributes.payload_attributes.slot_number,
        };

        Ok(Self {
            payload_attributes,
            no_tx_pool: attributes.no_tx_pool.unwrap_or_default(),
            transactions,
            gas_limit: attributes.gas_limit,
            eip_1559_params: attributes.eip_1559_params,
            min_base_fee: attributes.min_base_fee,
        })
    }
}

impl<BaseTransactionSigned> From<EthPayloadBuilderAttributes>
    for BasePayloadBuilderAttributes<BaseTransactionSigned>
{
    fn from(value: EthPayloadBuilderAttributes) -> Self {
        Self { payload_attributes: value, ..Default::default() }
    }
}

impl<BaseTransactionSigned> From<EthPayloadAttributes>
    for BasePayloadBuilderAttributes<BaseTransactionSigned>
{
    fn from(value: EthPayloadAttributes) -> Self {
        Self {
            payload_attributes: EthPayloadBuilderAttributes {
                id: Default::default(),
                parent: B256::ZERO,
                timestamp: value.timestamp,
                suggested_fee_recipient: value.suggested_fee_recipient,
                prev_randao: value.prev_randao,
                has_withdrawals: value.withdrawals.is_some(),
                withdrawals: value.withdrawals.unwrap_or_default().into(),
                parent_beacon_block_root: value.parent_beacon_block_root,
                slot_number: value.slot_number,
            },
            ..Default::default()
        }
    }
}

impl<T> serde::Serialize for BasePayloadBuilderAttributes<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.as_rpc_payload_attributes().serialize(serializer)
    }
}

impl<'de, T> serde::Deserialize<'de> for BasePayloadBuilderAttributes<T>
where
    T: Decodable2718 + Encodable2718 + Send + Sync + Debug + Unpin + 'static,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let attrs = BasePayloadAttributes::deserialize(deserializer)?;
        Self::try_new(B256::ZERO, attrs, 3).map_err(serde::de::Error::custom)
    }
}

impl<T> reth_payload_primitives::PayloadAttributes for BasePayloadBuilderAttributes<T>
where
    T: Clone + Decodable2718 + Encodable2718 + Send + Sync + Debug + Unpin + 'static,
{
    fn payload_id(&self, parent_hash: &B256) -> PayloadId {
        self.as_rpc_payload_attributes().payload_id(parent_hash, 3)
    }

    fn timestamp(&self) -> u64 {
        self.payload_attributes.timestamp
    }

    fn withdrawals(&self) -> Option<&Vec<alloy_eips::eip4895::Withdrawal>> {
        self.payload_attributes.has_withdrawals.then_some(&self.payload_attributes.withdrawals)
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.payload_attributes.parent_beacon_block_root
    }

    fn slot_number(&self) -> Option<u64> {
        self.payload_attributes.slot_number
    }
}

/// Contains the built payload.
#[derive(Debug, Clone)]
pub struct BaseBuiltPayload<N: NodePrimitives = BasePrimitives> {
    /// Identifier of the payload
    pub(crate) id: PayloadId,
    /// Sealed block
    pub(crate) block: Arc<SealedBlock<N::Block>>,
    /// Block execution data for the payload, if any.
    pub(crate) executed_block: Option<BuiltPayloadExecutedBlock<N>>,
    /// Amsterdam block access list RLP bytes, if any.
    pub(crate) block_access_list: Option<Bytes>,
    /// The fees of the block
    pub(crate) fees: U256,
}

// === impl BuiltPayload ===

impl<N: NodePrimitives> BaseBuiltPayload<N> {
    /// Initializes the payload with the given initial block.
    pub const fn new(
        id: PayloadId,
        block: Arc<SealedBlock<N::Block>>,
        fees: U256,
        executed_block: Option<BuiltPayloadExecutedBlock<N>>,
        block_access_list: Option<Bytes>,
    ) -> Self {
        Self { id, block, fees, executed_block, block_access_list }
    }

    /// Returns the identifier of the payload.
    pub const fn id(&self) -> PayloadId {
        self.id
    }

    /// Returns the built block(sealed)
    pub fn block(&self) -> &SealedBlock<N::Block> {
        &self.block
    }

    /// Fees of the block
    pub const fn fees(&self) -> U256 {
        self.fees
    }

    /// Converts the value into [`SealedBlock`].
    pub fn into_sealed_block(self) -> SealedBlock<N::Block> {
        Arc::unwrap_or_clone(self.block)
    }
}

impl<N: NodePrimitives> BuiltPayload for BaseBuiltPayload<N> {
    type Primitives = N;

    fn block(&self) -> &SealedBlock<N::Block> {
        self.block()
    }

    fn fees(&self) -> U256 {
        self.fees
    }

    fn executed_block(&self) -> Option<BuiltPayloadExecutedBlock<N>> {
        self.executed_block.clone()
    }

    fn block_access_list(&self) -> Option<&Bytes> {
        self.block_access_list.as_ref()
    }

    fn requests(&self) -> Option<Requests> {
        None
    }
}

impl<N: NodePrimitives> From<BaseBuiltPayload<N>> for base_common_rpc_types_engine::ExecutionData
where
    N::SignedTx: SignedTransaction,
{
    fn from(value: BaseBuiltPayload<N>) -> Self {
        let BaseBuiltPayload { block, block_access_list, .. } = value;
        let block_hash = block.hash();
        let block = Arc::unwrap_or_clone(block).into_block();

        Self::from_block_unchecked_with_extras(
            block_hash,
            &block.into_ethereum_block(),
            block_access_list,
        )
    }
}

// V1 engine_getPayloadV1 response
impl<T, N> From<BaseBuiltPayload<N>> for ExecutionPayloadV1
where
    T: SignedTransaction,
    N: NodePrimitives<Block = Block<T>>,
{
    fn from(value: BaseBuiltPayload<N>) -> Self {
        Self::from_block_unchecked(
            value.block().hash(),
            &Arc::unwrap_or_clone(value.block).into_block(),
        )
    }
}

// V2 engine_getPayloadV2 response
impl<T, N> From<BaseBuiltPayload<N>> for ExecutionPayloadEnvelopeV2
where
    T: SignedTransaction,
    N: NodePrimitives<Block = Block<T>>,
{
    fn from(value: BaseBuiltPayload<N>) -> Self {
        let BaseBuiltPayload { block, fees, .. } = value;

        Self {
            block_value: fees,
            execution_payload: ExecutionPayloadFieldV2::from_block_unchecked(
                block.hash(),
                &Arc::unwrap_or_clone(block).into_block(),
            ),
        }
    }
}

impl<T, N> From<BaseBuiltPayload<N>> for BaseExecutionPayloadEnvelopeV3
where
    T: SignedTransaction,
    N: NodePrimitives<Block = Block<T>>,
{
    fn from(value: BaseBuiltPayload<N>) -> Self {
        let BaseBuiltPayload { block, fees, .. } = value;

        let parent_beacon_block_root = block.parent_beacon_block_root.unwrap_or_default();

        Self {
            execution_payload: ExecutionPayloadV3::from_block_unchecked(
                block.hash(),
                &Arc::unwrap_or_clone(block).into_block(),
            ),
            block_value: fees,
            // From the engine API spec:
            //
            // > Client software **MAY** use any heuristics to decide whether to set
            // `shouldOverrideBuilder` flag or not. If client software does not implement any
            // heuristic this flag **SHOULD** be set to `false`.
            //
            // Spec:
            // <https://github.com/ethereum/execution-apis/blob/fe8e13c288c592ec154ce25c534e26cb7ce0530d/src/engine/cancun.md#specification-2>
            should_override_builder: false,
            // No blobs for Base execution payloads.
            blobs_bundle: BlobsBundleV1 { blobs: vec![], commitments: vec![], proofs: vec![] },
            parent_beacon_block_root,
        }
    }
}

impl<T, N> From<BaseBuiltPayload<N>> for BaseExecutionPayloadEnvelopeV4
where
    T: SignedTransaction,
    N: NodePrimitives<Block = Block<T>>,
{
    fn from(value: BaseBuiltPayload<N>) -> Self {
        let l2_withdrawals_root = value.block.withdrawals_root.unwrap_or_default();
        let v3 = BaseExecutionPayloadEnvelopeV3::from(value);

        Self {
            execution_payload: BaseExecutionPayloadV4::from_v3_with_withdrawals_root(
                v3.execution_payload,
                l2_withdrawals_root,
            ),
            block_value: v3.block_value,
            should_override_builder: v3.should_override_builder,
            blobs_bundle: v3.blobs_bundle,
            parent_beacon_block_root: v3.parent_beacon_block_root,
            execution_requests: vec![],
        }
    }
}

impl<T, N> From<BaseBuiltPayload<N>> for BaseExecutionPayloadEnvelopeV5
where
    T: SignedTransaction,
    N: NodePrimitives<Block = Block<T>>,
{
    fn from(value: BaseBuiltPayload<N>) -> Self {
        let v4 = BaseExecutionPayloadEnvelopeV4::from(value);

        Self {
            execution_payload: v4.execution_payload,
            block_value: v4.block_value,
            // No blobs for Base.
            blobs_bundle: BlobsBundleV2::default(),
            should_override_builder: v4.should_override_builder,
            execution_requests: v4.execution_requests,
        }
    }
}

// V6 engine_getPayloadV6 response (Amsterdam)
impl<T, N> TryFrom<BaseBuiltPayload<N>> for BaseExecutionPayloadEnvelopeV6
where
    T: SignedTransaction,
    N: NodePrimitives<Block = Block<T>>,
{
    type Error = BasePayloadError;

    fn try_from(value: BaseBuiltPayload<N>) -> Result<Self, Self::Error> {
        let BaseBuiltPayload { block, fees, block_access_list, .. } = value;

        // Amsterdam payloads must carry the block access list the header commits to, along with
        // the slot number (EIP-7843).
        let block_access_list =
            block_access_list.ok_or(PayloadError::PostAmsterdamBlockWithoutBlockAccessList)?;
        let slot_number =
            block.slot_number.ok_or(PayloadError::PostAmsterdamBlockWithoutSlotNumber)?;

        let l2_withdrawals_root = block.withdrawals_root.unwrap_or_default();
        let payload_v3 = ExecutionPayloadV3::from_block_unchecked(
            block.hash(),
            &Arc::unwrap_or_clone(block).into_block(),
        );

        Ok(Self {
            execution_payload: BaseExecutionPayloadV5::new(
                BaseExecutionPayloadV4::from_v3_with_withdrawals_root(
                    payload_v3,
                    l2_withdrawals_root,
                ),
                block_access_list,
                slot_number,
            ),
            block_value: fees,
            // No blobs for Base.
            blobs_bundle: BlobsBundleV2::default(),
            // From the engine API spec:
            //
            // > Client software **MAY** use any heuristics to decide whether to set
            // `shouldOverrideBuilder` flag or not. If client software does not implement any
            // heuristic this flag **SHOULD** be set to `false`.
            //
            // Spec:
            // <https://github.com/ethereum/execution-apis/blob/fe8e13c288c592ec154ce25c534e26cb7ce0530d/src/engine/cancun.md#specification-2>
            should_override_builder: false,
            execution_requests: vec![],
        })
    }
}

impl<H, T, ChainSpec> BuildNextEnv<BasePayloadBuilderAttributes<T>, H, ChainSpec>
    for BaseNextBlockEnvAttributes
where
    H: BlockHeader,
    T: SignedTransaction,
    ChainSpec: EthChainSpec + Upgrades,
{
    fn build_next_env(
        attributes: &BasePayloadBuilderAttributes<T>,
        parent: &SealedHeader<H>,
        chain_spec: &ChainSpec,
    ) -> Result<Self, PayloadBuilderError> {
        let timestamp = attributes.payload_attributes.timestamp;
        let extra_data = if chain_spec.is_jovian_active_at_timestamp(timestamp) {
            attributes
                .get_jovian_extra_data(chain_spec.base_fee_params_at_timestamp(timestamp))
                .map_err(PayloadBuilderError::other)?
        } else if chain_spec.is_holocene_active_at_timestamp(timestamp) {
            attributes
                .get_holocene_extra_data(chain_spec.base_fee_params_at_timestamp(timestamp))
                .map_err(PayloadBuilderError::other)?
        } else {
            Default::default()
        };

        Ok(Self {
            timestamp,
            suggested_fee_recipient: attributes.payload_attributes.suggested_fee_recipient,
            prev_randao: attributes.payload_attributes.prev_randao,
            gas_limit: attributes.gas_limit.unwrap_or_else(|| parent.gas_limit()),
            parent_beacon_block_root: attributes.payload_attributes.parent_beacon_block_root,
            extra_data,
            slot_number: attributes.payload_attributes.slot_number,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy_consensus::{BlockBody, Header};
    use alloy_primitives::{Bloom, FixedBytes, address, b256, bytes};
    use alloy_rpc_types_engine::{ExecutionPayloadV2, PayloadAttributes};
    use base_common_consensus::BaseTransactionSigned;
    use reth_payload_primitives::EngineApiMessageVersion;

    use super::*;

    #[test]
    fn test_built_payload_v4_v5_envelopes() {
        let transaction = bytes!(
            "7ef8f8a0417d134467f4737fcdf2475f0ecdd2a0ed6d87ecffc888ba9f60ee7e3b8ac26a94deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e20000008dd00101c1200000000000000040000000066c352bb000000000139c4f500000000000000000000000000000000000000000000000000000000c0cff1460000000000000000000000000000000000000000000000000000000000000001d4c88f4065ac9671e8b1329b90773e89b5ddff9cf8675b2b5e9c1b28320609930000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9"
        );
        let fees = U256::from(123_456_789);

        for withdrawals_root in [Some(B256::repeat_byte(0x11)), None] {
            for parent_beacon_block_root in [Some(B256::repeat_byte(0x22)), None] {
                let block = SealedBlock::seal_slow(Block {
                    header: Header {
                        parent_hash: B256::repeat_byte(0x33),
                        beneficiary: Address::repeat_byte(0x44),
                        state_root: B256::repeat_byte(0x55),
                        receipts_root: B256::repeat_byte(0x66),
                        logs_bloom: Bloom::repeat_byte(0x77),
                        mix_hash: B256::repeat_byte(0x88),
                        number: 42,
                        gas_limit: 30_000_000,
                        gas_used: 1_000_000,
                        timestamp: 1_748_000_000,
                        extra_data: bytes!("010203"),
                        base_fee_per_gas: Some(1_000_000_000),
                        blob_gas_used: Some(131_072),
                        excess_blob_gas: Some(262_144),
                        withdrawals_root,
                        parent_beacon_block_root,
                        ..Default::default()
                    },
                    body: BlockBody {
                        transactions: vec![decode_2718_canonical(&transaction).unwrap()],
                        withdrawals: Some(Withdrawals::default()),
                        ..Default::default()
                    },
                });
                let expected_payload = BaseExecutionPayloadV4 {
                    payload_inner: ExecutionPayloadV3 {
                        payload_inner: ExecutionPayloadV2 {
                            payload_inner: ExecutionPayloadV1 {
                                parent_hash: B256::repeat_byte(0x33),
                                fee_recipient: Address::repeat_byte(0x44),
                                state_root: B256::repeat_byte(0x55),
                                receipts_root: B256::repeat_byte(0x66),
                                logs_bloom: Bloom::repeat_byte(0x77),
                                prev_randao: B256::repeat_byte(0x88),
                                block_number: 42,
                                gas_limit: 30_000_000,
                                gas_used: 1_000_000,
                                timestamp: 1_748_000_000,
                                extra_data: bytes!("010203"),
                                base_fee_per_gas: U256::from(1_000_000_000),
                                block_hash: block.hash(),
                                transactions: vec![transaction.clone()],
                            },
                            withdrawals: vec![],
                        },
                        blob_gas_used: 131_072,
                        excess_blob_gas: 262_144,
                    },
                    withdrawals_root: withdrawals_root.unwrap_or(B256::ZERO),
                };
                let built_payload = BaseBuiltPayload::<BasePrimitives>::new(
                    PayloadId::new([1; 8]),
                    Arc::new(block),
                    fees,
                    None,
                    None,
                );

                assert_eq!(
                    BaseExecutionPayloadEnvelopeV4::from(built_payload.clone()),
                    BaseExecutionPayloadEnvelopeV4 {
                        execution_payload: expected_payload.clone(),
                        block_value: fees,
                        should_override_builder: false,
                        blobs_bundle: BlobsBundleV1::default(),
                        parent_beacon_block_root: parent_beacon_block_root.unwrap_or(B256::ZERO),
                        execution_requests: vec![],
                    }
                );
                assert_eq!(
                    BaseExecutionPayloadEnvelopeV5::from(built_payload),
                    BaseExecutionPayloadEnvelopeV5 {
                        execution_payload: expected_payload,
                        block_value: fees,
                        should_override_builder: false,
                        blobs_bundle: BlobsBundleV2::default(),
                        execution_requests: vec![],
                    }
                );
            }
        }
    }

    #[test]
    fn test_payload_id_parity_op_geth() {
        // INFO rollup_boost::server:received fork_choice_updated_v3 from builder and l2_client
        // payload_id_builder="0x6ef26ca02318dcf9" payload_id_l2="0x03d2dae446d2a86a"
        let expected =
            PayloadId::new(FixedBytes::<8>::from_str("0x03d2dae446d2a86a").unwrap().into());
        let attrs = BasePayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: 1728933301,
                prev_randao: b256!("0x9158595abbdab2c90635087619aa7042bbebe47642dfab3c9bfb934f6b082765"),
                suggested_fee_recipient: address!("0x4200000000000000000000000000000000000011"),
                withdrawals: Some([].into()),
                parent_beacon_block_root: b256!("0x8fe0193b9bf83cb7e5a08538e494fecc23046aab9a497af3704f4afdae3250ff").into(),
                slot_number: None,
                target_gas_limit: None,
            },
            transactions: Some([bytes!("7ef8f8a0dc19cfa777d90980e4875d0a548a881baaa3f83f14d1bc0d3038bc329350e54194deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e20000f424000000000000000000000000300000000670d6d890000000000000125000000000000000000000000000000000000000000000000000000000000000700000000000000000000000000000000000000000000000000000000000000014bf9181db6e381d4384bbf69c48b0ee0eed23c6ca26143c6d2544f9d39997a590000000000000000000000007f83d659683caf2767fd3c720981d51f5bc365bc")].into()),
            no_tx_pool: None,
            gas_limit: Some(30000000),
            eip_1559_params: None,
            min_base_fee: None,
        };

        // Reth's `PayloadId` should match op-geth's `PayloadId`. This fails
        assert_eq!(
            expected,
            attrs.payload_id(
                &b256!("0x3533bf30edaf9505d0810bf475cbe4e5f4b9889904b9845e83efdeab4e92eb1e"),
                EngineApiMessageVersion::V3 as u8
            )
        );
    }

    #[test]
    fn test_payload_id_parity_op_geth_jovian() {
        // <https://github.com/ethereum-optimism/op-geth/compare/optimism...mattsse:op-geth:matt/check-payload-id-equality>
        let expected =
            PayloadId::new(FixedBytes::<8>::from_str("0x046c65ffc4d659ec").unwrap().into());
        let attrs = BasePayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: 1728933301,
                prev_randao: b256!("0x9158595abbdab2c90635087619aa7042bbebe47642dfab3c9bfb934f6b082765"),
                suggested_fee_recipient: address!("0x4200000000000000000000000000000000000011"),
                withdrawals: Some([].into()),
                parent_beacon_block_root: b256!("0x8fe0193b9bf83cb7e5a08538e494fecc23046aab9a497af3704f4afdae3250ff").into(),
                slot_number: None,
                target_gas_limit: None,
            },
            transactions: Some([bytes!("7ef8f8a0dc19cfa777d90980e4875d0a548a881baaa3f83f14d1bc0d3038bc329350e54194deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e20000f424000000000000000000000000300000000670d6d890000000000000125000000000000000000000000000000000000000000000000000000000000000700000000000000000000000000000000000000000000000000000000000000014bf9181db6e381d4384bbf69c48b0ee0eed23c6ca26143c6d2544f9d39997a590000000000000000000000007f83d659683caf2767fd3c720981d51f5bc365bc")].into()),
            no_tx_pool: None,
            gas_limit: Some(30000000),
            eip_1559_params: None,
            min_base_fee: Some(100),
        };

        // Reth's `PayloadId` should match op-geth's `PayloadId`. This fails
        assert_eq!(
            expected,
            attrs.payload_id(
                &b256!("0x3533bf30edaf9505d0810bf475cbe4e5f4b9889904b9845e83efdeab4e92eb1e"),
                EngineApiMessageVersion::V4 as u8
            )
        );
    }

    #[test]
    fn test_get_extra_data_post_holocene() {
        let attributes: BasePayloadBuilderAttributes<BaseTransactionSigned> =
            BasePayloadBuilderAttributes {
                eip_1559_params: Some(B64::from_str("0x0000000800000008").unwrap()),
                ..Default::default()
            };
        let extra_data = attributes.get_holocene_extra_data(BaseFeeParams::new(80, 60));
        assert_eq!(extra_data.unwrap(), Bytes::copy_from_slice(&[0, 0, 0, 0, 8, 0, 0, 0, 8]));
    }

    #[test]
    fn test_payload_builder_rejects_target_gas_limit() {
        let attributes = BasePayloadAttributes {
            payload_attributes: PayloadAttributes {
                target_gas_limit: Some(30_000_000),
                ..Default::default()
            },
            ..Default::default()
        };

        let error = BasePayloadBuilderAttributes::<BaseTransactionSigned>::try_new(
            B256::ZERO,
            attributes,
            EngineApiMessageVersion::V3 as u8,
        )
        .expect_err("targetGasLimit must be rejected");

        assert_eq!(
            error,
            alloy_rlp::Error::Custom("targetGasLimit is not supported by Base payload attributes")
        );
    }

    #[test]
    fn test_get_extra_data_post_holocene_default() {
        let attributes: BasePayloadBuilderAttributes<BaseTransactionSigned> =
            BasePayloadBuilderAttributes { eip_1559_params: Some(B64::ZERO), ..Default::default() };
        let extra_data = attributes.get_holocene_extra_data(BaseFeeParams::new(80, 60));
        assert_eq!(extra_data.unwrap(), Bytes::copy_from_slice(&[0, 0, 0, 0, 80, 0, 0, 0, 60]));
    }

    #[test]
    fn test_get_extra_data_post_jovian() {
        let attributes: BasePayloadBuilderAttributes<BaseTransactionSigned> =
            BasePayloadBuilderAttributes {
                eip_1559_params: Some(B64::from_str("0x0000000800000008").unwrap()),
                min_base_fee: Some(10),
                ..Default::default()
            };
        let extra_data = attributes.get_jovian_extra_data(BaseFeeParams::new(80, 60));
        assert_eq!(
            extra_data.unwrap(),
            // Version byte is 1 for Jovian, then holocene payload followed by 8 bytes for the
            // minimum base fee
            Bytes::copy_from_slice(&[1, 0, 0, 0, 8, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 10])
        );
    }

    #[test]
    fn test_get_extra_data_post_jovian_default() {
        let attributes: BasePayloadBuilderAttributes<BaseTransactionSigned> =
            BasePayloadBuilderAttributes {
                eip_1559_params: Some(B64::ZERO),
                min_base_fee: Some(10),
                ..Default::default()
            };
        let extra_data = attributes.get_jovian_extra_data(BaseFeeParams::new(80, 60));
        assert_eq!(
            extra_data.unwrap(),
            // Version byte is 1 for Jovian, then holocene payload followed by 8 bytes for the
            // minimum base fee
            Bytes::copy_from_slice(&[1, 0, 0, 0, 80, 0, 0, 0, 60, 0, 0, 0, 0, 0, 0, 0, 10])
        );
    }

    #[test]
    fn test_get_extra_data_post_jovian_no_base_fee() {
        let attributes: BasePayloadBuilderAttributes<BaseTransactionSigned> =
            BasePayloadBuilderAttributes {
                eip_1559_params: Some(B64::ZERO),
                min_base_fee: None,
                ..Default::default()
            };
        let extra_data = attributes.get_jovian_extra_data(BaseFeeParams::new(80, 60));
        assert_eq!(extra_data.unwrap_err(), EIP1559ParamError::MinBaseFeeNotSet);
    }
}
