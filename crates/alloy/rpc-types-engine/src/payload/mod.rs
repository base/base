//! Versioned Optimism execution payloads

pub mod error;
pub mod v3;
pub mod v4;

use crate::{OpExecutionPayloadSidecar, OpExecutionPayloadV4};
use alloc::string::String;
use alloy_consensus::{Block, EMPTY_ROOT_HASH};
use alloy_eips::{Decodable2718, Typed2718};
use alloy_primitives::{map::HashMap, B256};
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
use error::OpPayloadError;

/// An execution payload, which can be either [`ExecutionPayloadV2`], [`ExecutionPayloadV3`], or
/// [`OpExecutionPayloadV4`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(untagged))]
pub enum OpExecutionPayload {
    /// V2 payload
    V2(ExecutionPayloadV2),
    /// V3 payload
    V3(ExecutionPayloadV3),
    /// V4 payload
    V4(OpExecutionPayloadV4),
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for OpExecutionPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ExecutionPayloadVisitor;

        impl<'de> serde::de::Visitor<'de> for ExecutionPayloadVisitor {
            type Value = OpExecutionPayload;

            fn expecting(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                formatter.write_str("a valid OpExecutionPayload object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                use serde::de::IntoDeserializer;

                enum Fields {
                    ParentHash,
                    FeeRecipient,
                    StateRoot,
                    ReceiptsRoot,
                    LogsBloom,
                    PrevRandao,
                    BlockNumber,
                    GasLimit,
                    GasUsed,
                    Timestamp,
                    ExtraData,
                    BaseFeePerGas,
                    BlockHash,
                    Transactions,
                    Withdrawals,
                    BlobGasUsed,
                    ExcessBlobGas,
                    WithdrawalsRoot,
                    Unknown(alloc::string::String),
                }

                impl<'de> serde::Deserialize<'de> for Fields {
                    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
                    where
                        D: serde::Deserializer<'de>,
                    {
                        struct FieldVisitor;

                        impl serde::de::Visitor<'_> for FieldVisitor {
                            type Value = Fields;

                            fn expecting(
                                &self,
                                formatter: &mut core::fmt::Formatter<'_>,
                            ) -> core::fmt::Result {
                                formatter.write_str("a known field")
                            }

                            fn visit_str<E>(self, value: &str) -> Result<Fields, E>
                            where
                                E: serde::de::Error,
                            {
                                Ok(match value {
                                    "parentHash" => Fields::ParentHash,
                                    "feeRecipient" => Fields::FeeRecipient,
                                    "stateRoot" => Fields::StateRoot,
                                    "receiptsRoot" => Fields::ReceiptsRoot,
                                    "logsBloom" => Fields::LogsBloom,
                                    "prevRandao" => Fields::PrevRandao,
                                    "blockNumber" => Fields::BlockNumber,
                                    "gasLimit" => Fields::GasLimit,
                                    "gasUsed" => Fields::GasUsed,
                                    "timestamp" => Fields::Timestamp,
                                    "extraData" => Fields::ExtraData,
                                    "baseFeePerGas" => Fields::BaseFeePerGas,
                                    "blockHash" => Fields::BlockHash,
                                    "transactions" => Fields::Transactions,
                                    "withdrawals" => Fields::Withdrawals,
                                    "blobGasUsed" => Fields::BlobGasUsed,
                                    "excessBlobGas" => Fields::ExcessBlobGas,
                                    "withdrawalsRoot" => Fields::WithdrawalsRoot,
                                    _ => Fields::Unknown(value.into()),
                                })
                            }
                        }

                        deserializer.deserialize_str(FieldVisitor)
                    }
                }

                let mut parent_hash = None;
                let mut fee_recipient = None;
                let mut state_root = None;
                let mut receipts_root = None;
                let mut logs_bloom = None;
                let mut prev_randao = None;
                let mut block_number = None;
                let mut gas_limit = None;
                let mut gas_used = None;
                let mut timestamp = None;
                let mut extra_data = None;
                let mut base_fee_per_gas = None;
                let mut block_hash = None;
                let mut transactions = None;
                let mut withdrawals = None;
                let mut blob_gas_used = None;
                let mut excess_blob_gas = None;
                let mut withdrawals_root = None;

                let mut extra_fields = HashMap::new();

                while let Some(key) = map.next_key()? {
                    match key {
                        Fields::ParentHash => parent_hash = Some(map.next_value()?),
                        Fields::FeeRecipient => fee_recipient = Some(map.next_value()?),
                        Fields::StateRoot => state_root = Some(map.next_value()?),
                        Fields::ReceiptsRoot => receipts_root = Some(map.next_value()?),
                        Fields::LogsBloom => logs_bloom = Some(map.next_value()?),
                        Fields::PrevRandao => prev_randao = Some(map.next_value()?),
                        Fields::BlockNumber => {
                            let raw = map.next_value::<&str>()?;
                            block_number =
                                Some(alloy_serde::quantity::deserialize(raw.into_deserializer())?);
                        }
                        Fields::GasLimit => {
                            let raw = map.next_value::<&str>()?;
                            gas_limit =
                                Some(alloy_serde::quantity::deserialize(raw.into_deserializer())?);
                        }
                        Fields::GasUsed => {
                            let raw = map.next_value::<String>()?;
                            gas_used =
                                Some(alloy_serde::quantity::deserialize(raw.into_deserializer())?);
                        }
                        Fields::Timestamp => {
                            let raw = map.next_value::<String>()?;
                            timestamp =
                                Some(alloy_serde::quantity::deserialize(raw.into_deserializer())?);
                        }
                        Fields::ExtraData => extra_data = Some(map.next_value()?),
                        Fields::BaseFeePerGas => base_fee_per_gas = Some(map.next_value()?),
                        Fields::BlockHash => block_hash = Some(map.next_value()?),
                        Fields::Transactions => transactions = Some(map.next_value()?),
                        Fields::Withdrawals => withdrawals = Some(map.next_value()?),
                        Fields::BlobGasUsed => {
                            let raw = map.next_value::<String>()?;
                            blob_gas_used =
                                Some(alloy_serde::quantity::deserialize(raw.into_deserializer())?);
                        }
                        Fields::ExcessBlobGas => {
                            let raw = map.next_value::<String>()?;
                            excess_blob_gas =
                                Some(alloy_serde::quantity::deserialize(raw.into_deserializer())?);
                        }
                        Fields::WithdrawalsRoot => withdrawals_root = Some(map.next_value()?),
                        Fields::Unknown(field) => {
                            let raw = map.next_value::<String>()?;
                            extra_fields.insert(field, raw);
                        }
                    }
                }

                let v1 = ExecutionPayloadV1 {
                    parent_hash: parent_hash
                        .ok_or_else(|| serde::de::Error::missing_field("parentHash"))?,
                    fee_recipient: fee_recipient
                        .ok_or_else(|| serde::de::Error::missing_field("feeRecipient"))?,
                    state_root: state_root
                        .ok_or_else(|| serde::de::Error::missing_field("stateRoot"))?,
                    receipts_root: receipts_root
                        .ok_or_else(|| serde::de::Error::missing_field("receiptsRoot"))?,
                    logs_bloom: logs_bloom
                        .ok_or_else(|| serde::de::Error::missing_field("logsBloom"))?,
                    prev_randao: prev_randao
                        .ok_or_else(|| serde::de::Error::missing_field("prevRandao"))?,
                    block_number: block_number
                        .ok_or_else(|| serde::de::Error::missing_field("blockNumber"))?,
                    gas_limit: gas_limit
                        .ok_or_else(|| serde::de::Error::missing_field("gasLimit"))?,
                    gas_used: gas_used.ok_or_else(|| serde::de::Error::missing_field("gasUsed"))?,
                    timestamp: timestamp
                        .ok_or_else(|| serde::de::Error::missing_field("timestamp"))?,
                    extra_data: extra_data
                        .ok_or_else(|| serde::de::Error::missing_field("extraData"))?,
                    base_fee_per_gas: base_fee_per_gas
                        .ok_or_else(|| serde::de::Error::missing_field("baseFeePerGas"))?,
                    block_hash: block_hash
                        .ok_or_else(|| serde::de::Error::missing_field("blockHash"))?,
                    transactions: transactions
                        .ok_or_else(|| serde::de::Error::missing_field("transactions"))?,
                };

                // Ensure `withdrawals` is present before proceeding
                let withdrawals =
                    withdrawals.ok_or_else(|| serde::de::Error::missing_field("withdrawals"))?;

                // Construct base V2 payload
                let payload_v2 = ExecutionPayloadV2 { payload_inner: v1, withdrawals };

                // Ensure `blob_gas_used` and `excess_blob_gas` are either both present or both
                // absent
                match (blob_gas_used, excess_blob_gas) {
                    // If both are present, create V3
                    (Some(blob_gas_used), Some(excess_blob_gas)) => {
                        let payload_v3 = ExecutionPayloadV3 {
                            payload_inner: payload_v2,
                            blob_gas_used,
                            excess_blob_gas,
                        };

                        // If `withdrawals_root` is present, wrap into V4; otherwise, return V3
                        if let Some(withdrawals_root) = withdrawals_root {
                            Ok(OpExecutionPayload::V4(OpExecutionPayloadV4 {
                                payload_inner: payload_v3,
                                withdrawals_root,
                            }))
                        } else {
                            Ok(OpExecutionPayload::V3(payload_v3))
                        }
                    }
                    // If one is missing, reject as invalid
                    (Some(_), None) | (None, Some(_)) => {
                        Err(serde::de::Error::custom("invalid enum variant"))
                    }
                    // If neither are present, return V2
                    (None, None) => Ok(OpExecutionPayload::V2(payload_v2)),
                }
            }
        }

        const FIELDS: &[&str] = &[
            "parentHash",
            "feeRecipient",
            "stateRoot",
            "receiptsRoot",
            "logsBloom",
            "prevRandao",
            "blockNumber",
            "gasLimit",
            "gasUsed",
            "timestamp",
            "extraData",
            "baseFeePerGas",
            "blockHash",
            "transactions",
            "withdrawals",
            "blobGasUsed",
            "excessBlobGas",
            "withdrawalsRoot",
        ];

        deserializer.deserialize_struct("OpExecutionPayload", FIELDS, ExecutionPayloadVisitor)
    }
}

impl OpExecutionPayload {
    /// Returns a reference to the V2 payload, if any.
    pub const fn as_v2(&self) -> &ExecutionPayloadV2 {
        match self {
            Self::V2(payload) => payload,
            Self::V3(payload) => &payload.payload_inner,
            Self::V4(payload) => &payload.payload_inner.payload_inner,
        }
    }

    /// Returns a mutable reference to the V2 payload, if any.
    pub fn as_v2_mut(&mut self) -> &ExecutionPayloadV2 {
        match self {
            Self::V2(payload) => payload,
            Self::V3(payload) => &mut payload.payload_inner,
            Self::V4(payload) => &payload.payload_inner.payload_inner,
        }
    }
    /// Returns a reference to the V3 payload, if any.
    pub const fn as_v3(&self) -> Option<&ExecutionPayloadV3> {
        match self {
            Self::V2(_) => None,
            Self::V3(payload) => Some(payload),
            Self::V4(payload) => Some(&payload.payload_inner),
        }
    }

    /// Returns a mutable reference to the V3 payload, if any.
    pub fn as_v3_mut(&mut self) -> Option<&ExecutionPayloadV3> {
        match self {
            Self::V2(_) => None,
            Self::V3(payload) => Some(payload),
            Self::V4(payload) => Some(&payload.payload_inner),
        }
    }

    /// Returns a reference to the V4 payload, if any.
    pub const fn as_v4(&self) -> Option<&OpExecutionPayloadV4> {
        match self {
            Self::V2(_) | Self::V3(_) => None,
            Self::V4(payload) => Some(payload),
        }
    }
    /// Returns the parent hash for the payload.
    pub const fn parent_hash(&self) -> B256 {
        self.as_v2().payload_inner.parent_hash
    }

    /// Returns the block hash for the payload.
    pub const fn block_hash(&self) -> B256 {
        self.as_v2().payload_inner.block_hash
    }

    /// Returns the block number for this payload.
    pub const fn block_number(&self) -> u64 {
        self.as_v2().payload_inner.block_number
    }

    #[allow(rustdoc::broken_intra_doc_links)]
    /// Converts [`OpExecutionPayload`] to [`Block`].
    ///
    /// Checks that payload doesn't contain:
    /// - blob transactions
    /// - L1 withdrawals
    ///
    /// Caution: This does not set fields that are not part of the payload and only part of the
    /// [`OpExecutionPayloadSidecar`]:
    /// - parent_beacon_block_root
    ///
    /// See also: [`OpExecutionPayload::try_into_block_with_sidecar`]
    pub fn try_into_block<T: Decodable2718 + Typed2718>(self) -> Result<Block<T>, OpPayloadError> {
        if !self.as_v2().withdrawals.is_empty() {
            return Err(OpPayloadError::NonEmptyL1Withdrawals);
        }
        let block = match self {
            Self::V2(payload) => payload.try_into_block()?,
            Self::V3(payload) => payload.try_into_block()?,
            Self::V4(payload) => payload.try_into_block()?,
        };
        if block.body.has_eip4844_transactions() {
            return Err(OpPayloadError::BlobTransaction);
        }

        Ok(block)
    }

    /// Tries to create a new unsealed block from the given payload and payload sidecar.
    ///
    /// Additional to checks preformed in [`OpExecutionPayload::try_into_block`], which is called
    /// under the hood, also checks that sidecar doesn't contain:
    /// - blob versioned hashes
    /// - execution layer requests
    ///
    /// See also docs for
    /// [`ExecutionPayload::try_into_block_with_sidecar`](alloy_rpc_types_engine::ExecutionPayload::try_into_block_with_sidecar).
    pub fn try_into_block_with_sidecar<T: Decodable2718 + Typed2718>(
        self,
        sidecar: &OpExecutionPayloadSidecar,
    ) -> Result<Block<T>, OpPayloadError> {
        let mut base_payload = self.try_into_block()?;
        if let Some(blobs_hashes) = sidecar.versioned_hashes() {
            if !blobs_hashes.is_empty() {
                return Err(OpPayloadError::NonEmptyBlobVersionedHashes);
            }
        }
        if let Some(reqs_hash) = sidecar.requests_hash() {
            if reqs_hash != EMPTY_ROOT_HASH {
                return Err(OpPayloadError::NonEmptyELRequests);
            }
            base_payload.header.requests_hash = Some(EMPTY_ROOT_HASH)
        }
        base_payload.header.parent_beacon_block_root = sidecar.parent_beacon_block_root();

        Ok(base_payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "serde")]
    fn serde_payload_input_enum_v4() {
        let response_v4 = r#"{"parentHash":"0xe927a1448525fb5d32cb50ee1408461a945ba6c39bd5cf5621407d500ecc8de9","feeRecipient":"0x0000000000000000000000000000000000000000","stateRoot":"0x10f8a0830000e8edef6d00cc727ff833f064b1950afd591ae41357f97e543119","receiptsRoot":"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421","logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","prevRandao":"0xe0d8b4521a7da1582a713244ffb6a86aa1726932087386e2dc7973f43fc6cb24","blockNumber":"0x1","gasLimit":"0x2ffbd2","gasUsed":"0x0","timestamp":"0x1235","extraData":"0xd883010d00846765746888676f312e32312e30856c696e7578","baseFeePerGas":"0x342770c0","blockHash":"0x44d0fa5f2f73a938ebb96a2a21679eb8dea3e7b7dd8fd9f35aa756dda8bf0a8a","transactions":[],"withdrawals":[],"blobGasUsed":"0x0","excessBlobGas":"0x0","withdrawalsRoot":"0x10f8a0830000e8edef6d00cc727ff833f064b1950afd591ae41357f97e543119"}"#;

        let payload: OpExecutionPayload = serde_json::from_str(response_v4).unwrap();
        assert!(payload.as_v4().is_some());
        assert_eq!(serde_json::to_string(&payload).unwrap(), response_v4);

        let payload_v4: OpExecutionPayloadV4 = serde_json::from_str(response_v4).unwrap();
        assert_eq!(payload.as_v4().unwrap(), &payload_v4);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn serde_payload_input_enum_v3() {
        let response_v3 = r#"{"parentHash":"0xe927a1448525fb5d32cb50ee1408461a945ba6c39bd5cf5621407d500ecc8de9","feeRecipient":"0x0000000000000000000000000000000000000000","stateRoot":"0x10f8a0830000e8edef6d00cc727ff833f064b1950afd591ae41357f97e543119","receiptsRoot":"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421","logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","prevRandao":"0xe0d8b4521a7da1582a713244ffb6a86aa1726932087386e2dc7973f43fc6cb24","blockNumber":"0x1","gasLimit":"0x2ffbd2","gasUsed":"0x0","timestamp":"0x1235","extraData":"0xd883010d00846765746888676f312e32312e30856c696e7578","baseFeePerGas":"0x342770c0","blockHash":"0x44d0fa5f2f73a938ebb96a2a21679eb8dea3e7b7dd8fd9f35aa756dda8bf0a8a","transactions":[],"withdrawals":[],"blobGasUsed":"0x0","excessBlobGas":"0x0"}"#;

        let payload: OpExecutionPayload = serde_json::from_str(response_v3).unwrap();
        assert!(payload.as_v3().is_some());
        assert_eq!(serde_json::to_string(&payload).unwrap(), response_v3);

        let payload_v3: ExecutionPayloadV3 = serde_json::from_str(response_v3).unwrap();
        assert_eq!(payload.as_v3().unwrap(), &payload_v3);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn serde_payload_input_enum_v2() {
        let response_v2 = r#"{"parentHash":"0xe927a1448525fb5d32cb50ee1408461a945ba6c39bd5cf5621407d500ecc8de9","feeRecipient":"0x0000000000000000000000000000000000000000","stateRoot":"0x10f8a0830000e8edef6d00cc727ff833f064b1950afd591ae41357f97e543119","receiptsRoot":"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421","logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","prevRandao":"0xe0d8b4521a7da1582a713244ffb6a86aa1726932087386e2dc7973f43fc6cb24","blockNumber":"0x1","gasLimit":"0x2ffbd2","gasUsed":"0x0","timestamp":"0x1235","extraData":"0xd883010d00846765746888676f312e32312e30856c696e7578","baseFeePerGas":"0x342770c0","blockHash":"0x44d0fa5f2f73a938ebb96a2a21679eb8dea3e7b7dd8fd9f35aa756dda8bf0a8a","transactions":[],"withdrawals":[]}"#;

        let payload: OpExecutionPayload = serde_json::from_str(response_v2).unwrap();
        assert!(payload.as_v3().is_none());
        assert_eq!(serde_json::to_string(&payload).unwrap(), response_v2);

        let payload_v2: ExecutionPayloadV2 = serde_json::from_str(response_v2).unwrap();
        assert_eq!(payload.as_v2(), &payload_v2);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn serde_payload_input_enum_faulty_v2() {
        // incomplete V3 payload should be rejected even if it has all V2 fields
        let response_faulty = r#"{"parentHash":"0xe927a1448525fb5d32cb50ee1408461a945ba6c39bd5cf5621407d500ecc8de9","feeRecipient":"0x0000000000000000000000000000000000000000","stateRoot":"0x10f8a0830000e8edef6d00cc727ff833f064b1950afd591ae41357f97e543119","receiptsRoot":"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421","logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","prevRandao":"0xe0d8b4521a7da1582a713244ffb6a86aa1726932087386e2dc7973f43fc6cb24","blockNumber":"0x1","gasLimit":"0x2ffbd2","gasUsed":"0x0","timestamp":"0x1235","extraData":"0xd883010d00846765746888676f312e32312e30856c696e7578","baseFeePerGas":"0x342770c0","blockHash":"0x44d0fa5f2f73a938ebb96a2a21679eb8dea3e7b7dd8fd9f35aa756dda8bf0a8a","transactions":[],"withdrawals":[], "blobGasUsed": "0x0"}"#;

        let payload: Result<OpExecutionPayload, serde_json::Error> =
            serde_json::from_str(response_faulty);
        assert!(payload.is_err());
    }

    #[test]
    #[cfg(feature = "serde")]
    fn serde_payload_input_enum_faulty_v1() {
        // incomplete V3 payload should be rejected even if it has all V1 fields
        let response_faulty = r#"{"parentHash":"0xe927a1448525fb5d32cb50ee1408461a945ba6c39bd5cf5621407d500ecc8de9","feeRecipient":"0x0000000000000000000000000000000000000000","stateRoot":"0x10f8a0830000e8edef6d00cc727ff833f064b1950afd591ae41357f97e543119","receiptsRoot":"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421","logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","prevRandao":"0xe0d8b4521a7da1582a713244ffb6a86aa1726932087386e2dc7973f43fc6cb24","blockNumber":"0x1","gasLimit":"0x2ffbd2","gasUsed":"0x0","timestamp":"0x1235","extraData":"0xd883010d00846765746888676f312e32312e30856c696e7578","baseFeePerGas":"0x342770c0","blockHash":"0x44d0fa5f2f73a938ebb96a2a21679eb8dea3e7b7dd8fd9f35aa756dda8bf0a8a","transactions":[],"blobGasUsed": "0x0"}"#;

        let payload: Result<OpExecutionPayload, serde_json::Error> =
            serde_json::from_str(response_faulty);
        assert!(payload.is_err());
    }
}
