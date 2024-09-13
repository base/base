//! This module contains the [L1BlockInfoTx] type, and various encoding / decoding methods for it.

use super::{DepositSourceDomain, L1InfoDepositSource};
#[cfg(not(feature = "std"))]
use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use alloy_consensus::Header;
use alloy_eips::BlockNumHash;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use op_alloy_consensus::{OpTxEnvelope, TxDeposit};
use superchain_primitives::{RollupConfig, SystemConfig};

/// The system transaction gas limit post-Regolith
const REGOLITH_SYSTEM_TX_GAS: u128 = 1_000_000;
/// The type byte identifier for the L1 scalar format in Ecotone.
const L1_SCALAR_ECOTONE: u8 = 1;
/// The length of an L1 info transaction in Bedrock.
const L1_INFO_TX_LEN_BEDROCK: usize = 4 + 32 * 8;
/// The length of an L1 info transaction in Ecotone.
const L1_INFO_TX_LEN_ECOTONE: usize = 4 + 32 * 5;
/// The 4 byte selector of the
/// "setL1BlockValues(uint64,uint64,uint256,bytes32,uint64,bytes32,uint256,uint256)" function
const L1_INFO_TX_SELECTOR_BEDROCK: [u8; 4] = [0x01, 0x5d, 0x8e, 0xb9];
/// The 4 byte selector of "setL1BlockValuesEcotone()"
const L1_INFO_TX_SELECTOR_ECOTONE: [u8; 4] = [0x44, 0x0a, 0x5e, 0x20];
/// The address of the L1 Block contract
const L1_BLOCK_ADDRESS: Address = address!("4200000000000000000000000000000000000015");
/// The depositor address of the L1 info transaction
const L1_INFO_DEPOSITOR_ADDRESS: Address = address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001");

/// The [L1BlockInfoTx] enum contains variants for the different versions of the L1 block info
/// transaction on OP Stack chains.
///
/// This transaction always sits at the top of the block, and alters the `L1 Block` contract's
/// knowledge of the L1 chain.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum L1BlockInfoTx {
    /// A Bedrock L1 info transaction
    Bedrock(L1BlockInfoBedrock),
    /// An Ecotone L1 info transaction
    Ecotone(L1BlockInfoEcotone),
}

/// Represents the fields within a Bedrock L1 block info transaction.
///
/// Bedrock Binary Format
// +---------+--------------------------+
// | Bytes   | Field                    |
// +---------+--------------------------+
// | 4       | Function signature       |
// | 32      | Number                   |
// | 32      | Time                     |
// | 32      | BaseFee                  |
// | 32      | BlockHash                |
// | 32      | SequenceNumber           |
// | 32      | BatcherHash              |
// | 32      | L1FeeOverhead            |
// | 32      | L1FeeScalar              |
// +---------+--------------------------+
#[derive(Debug, Clone, Hash, Eq, PartialEq, Default, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct L1BlockInfoBedrock {
    /// The current L1 origin block number
    pub number: u64,
    /// The current L1 origin block's timestamp
    pub time: u64,
    /// The current L1 origin block's basefee
    pub base_fee: u64,
    /// The current L1 origin block's hash
    pub block_hash: B256,
    /// The current sequence number
    pub sequence_number: u64,
    /// The address of the batch submitter
    pub batcher_address: Address,
    /// The fee overhead for L1 data
    pub l1_fee_overhead: U256,
    /// The fee scalar for L1 data
    pub l1_fee_scalar: U256,
}

/// Represents the fields within an Ecotone L1 block info transaction.
///
/// Ecotone Binary Format
/// +---------+--------------------------+
/// | Bytes   | Field                    |
/// +---------+--------------------------+
/// | 4       | Function signature       |
/// | 4       | BaseFeeScalar            |
/// | 4       | BlobBaseFeeScalar        |
/// | 8       | SequenceNumber           |
/// | 8       | Timestamp                |
/// | 8       | L1BlockNumber            |
/// | 32      | BaseFee                  |
/// | 32      | BlobBaseFee              |
/// | 32      | BlockHash                |
/// | 32      | BatcherHash              |
/// +---------+--------------------------+
#[derive(Debug, Clone, Hash, Eq, PartialEq, Default, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct L1BlockInfoEcotone {
    /// The current L1 origin block number
    pub number: u64,
    /// The current L1 origin block's timestamp
    pub time: u64,
    /// The current L1 origin block's basefee
    pub base_fee: u64,
    /// The current L1 origin block's hash
    pub block_hash: B256,
    /// The current sequence number
    pub sequence_number: u64,
    /// The address of the batch submitter
    pub batcher_address: Address,
    /// The current blob base fee on L1
    pub blob_base_fee: u128,
    /// The fee scalar for L1 blobspace data
    pub blob_base_fee_scalar: u32,
    /// The fee scalar for L1 data
    pub base_fee_scalar: u32,
}

/// An error type for parsing L1 block info transactions.
#[derive(Debug, Copy, Clone)]
pub enum BlockInfoError {
    /// Failed to parse the L1 blob base fee scalar.
    L1BlobBaseFeeScalar,
    /// Failed to parse the base fee scalar.
    BaseFeeScalar,
}

impl core::fmt::Display for BlockInfoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BlockInfoError::L1BlobBaseFeeScalar => {
                write!(f, "Failed to parse the L1 blob base fee scalar")
            }
            BlockInfoError::BaseFeeScalar => write!(f, "Failed to parse the base fee scalar"),
        }
    }
}

#[allow(missing_docs)]
#[derive(Debug)]
pub enum DecodeError {
    InvalidSelector,
    ParseError(String),
    InvalidLength(String),
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::InvalidSelector => write!(f, "Invalid L1 info transaction selector"),
            DecodeError::ParseError(msg) => write!(f, "Parse error: {}", msg),
            DecodeError::InvalidLength(msg) => write!(f, "Invalid data length: {}", msg), /* Handle display for length errors */
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for DecodeError {}

impl L1BlockInfoTx {
    /// Creates a new [L1BlockInfoTx] from the given information.
    pub fn try_new(
        rollup_config: &RollupConfig,
        system_config: &SystemConfig,
        sequence_number: u64,
        l1_header: &Header,
        l2_block_time: u64,
    ) -> Result<Self, BlockInfoError> {
        // In the first block of Ecotone, the L1Block contract has not been upgraded yet due to the
        // upgrade transactions being placed after the L1 info transaction. Because of this,
        // for the first block of Ecotone, we send a Bedrock style L1 block info transaction
        if rollup_config.is_ecotone_active(l2_block_time)
            && rollup_config.ecotone_time.unwrap_or_default() != l2_block_time
        {
            let scalar = system_config.scalar.to_be_bytes::<32>();
            let blob_base_fee_scalar = (scalar[0] == L1_SCALAR_ECOTONE)
                .then(|| {
                    Ok::<u32, BlockInfoError>(u32::from_be_bytes(
                        scalar[24..28]
                            .try_into()
                            .map_err(|_| BlockInfoError::L1BlobBaseFeeScalar)?,
                    ))
                })
                .transpose()?
                .unwrap_or_default();
            let base_fee_scalar = u32::from_be_bytes(
                scalar[28..32].try_into().map_err(|_| BlockInfoError::BaseFeeScalar)?,
            );
            Ok(Self::Ecotone(L1BlockInfoEcotone {
                number: l1_header.number,
                time: l1_header.timestamp,
                base_fee: l1_header.base_fee_per_gas.unwrap_or(0) as u64,
                block_hash: l1_header.hash_slow(),
                sequence_number,
                batcher_address: system_config.batcher_address,
                blob_base_fee: l1_header.blob_fee().unwrap_or(1),
                blob_base_fee_scalar,
                base_fee_scalar,
            }))
        } else {
            Ok(Self::Bedrock(L1BlockInfoBedrock {
                number: l1_header.number,
                time: l1_header.timestamp,
                base_fee: l1_header.base_fee_per_gas.unwrap_or(0) as u64,
                block_hash: l1_header.hash_slow(),
                sequence_number,
                batcher_address: system_config.batcher_address,
                l1_fee_overhead: system_config.overhead,
                l1_fee_scalar: system_config.scalar,
            }))
        }
    }

    /// Creates a new [L1BlockInfoTx] from the given information and returns a typed [TxDeposit] to
    /// include at the top of a block.
    pub fn try_new_with_deposit_tx(
        rollup_config: &RollupConfig,
        system_config: &SystemConfig,
        sequence_number: u64,
        l1_header: &Header,
        l2_block_time: u64,
    ) -> Result<(L1BlockInfoTx, OpTxEnvelope), BlockInfoError> {
        let l1_info =
            Self::try_new(rollup_config, system_config, sequence_number, l1_header, l2_block_time)?;
        let l1_block_hash = match l1_info {
            Self::Bedrock(ref tx) => tx.block_hash,
            Self::Ecotone(ref tx) => tx.block_hash,
        };

        let source = DepositSourceDomain::L1Info(L1InfoDepositSource {
            l1_block_hash,
            seq_number: sequence_number,
        });

        let mut deposit_tx = TxDeposit {
            source_hash: source.source_hash(),
            from: L1_INFO_DEPOSITOR_ADDRESS,
            to: TxKind::Call(L1_BLOCK_ADDRESS),
            mint: None,
            value: U256::ZERO,
            gas_limit: 150_000_000,
            is_system_transaction: true,
            input: l1_info.encode_calldata(),
        };

        // With the regolith hardfork, system transactions were deprecated, and we allocate
        // a constant amount of gas for special transactions like L1 block info.
        if rollup_config.is_regolith_active(l2_block_time) {
            deposit_tx.is_system_transaction = false;
            deposit_tx.gas_limit = REGOLITH_SYSTEM_TX_GAS;
        }

        Ok((l1_info, OpTxEnvelope::Deposit(deposit_tx)))
    }

    /// Decodes the [L1BlockInfoEcotone] object from ethereum transaction calldata.
    pub fn decode_calldata(r: &[u8]) -> Result<Self, DecodeError> {
        let selector = r
            .get(0..4)
            .ok_or(DecodeError::ParseError("Slice out of range".to_string()))
            .and_then(|slice| {
                slice.try_into().map_err(|_| {
                    DecodeError::ParseError("Failed to convert 4byte slice to array".to_string())
                })
            })?;
        match selector {
            L1_INFO_TX_SELECTOR_BEDROCK => L1BlockInfoBedrock::decode_calldata(r)
                .map(Self::Bedrock)
                .map_err(|e| DecodeError::ParseError(format!("Bedrock decode error: {}", e))),
            L1_INFO_TX_SELECTOR_ECOTONE => L1BlockInfoEcotone::decode_calldata(r)
                .map(Self::Ecotone)
                .map_err(|e| DecodeError::ParseError(format!("Ecotone decode error: {}", e))),
            _ => Err(DecodeError::InvalidSelector),
        }
    }

    /// Encodes the [L1BlockInfoTx] object into Ethereum transaction calldata.
    pub fn encode_calldata(&self) -> Bytes {
        match self {
            Self::Bedrock(bedrock_tx) => bedrock_tx.encode_calldata(),
            Self::Ecotone(ecotone_tx) => ecotone_tx.encode_calldata(),
        }
    }

    /// Returns the L1 [BlockNumHash] for the info transaction.
    pub const fn id(&self) -> BlockNumHash {
        match self {
            Self::Ecotone(L1BlockInfoEcotone { number, block_hash, .. }) => {
                BlockNumHash { number: *number, hash: *block_hash }
            }
            Self::Bedrock(L1BlockInfoBedrock { number, block_hash, .. }) => {
                BlockNumHash { number: *number, hash: *block_hash }
            }
        }
    }

    /// Returns the L1 fee overhead for the info transaction. After ecotone, this value is ignored.
    pub const fn l1_fee_overhead(&self) -> U256 {
        match self {
            Self::Bedrock(L1BlockInfoBedrock { l1_fee_overhead, .. }) => *l1_fee_overhead,
            Self::Ecotone(_) => U256::ZERO,
        }
    }

    /// Returns the batcher address for the info transaction
    pub const fn batcher_address(&self) -> Address {
        match self {
            Self::Bedrock(L1BlockInfoBedrock { batcher_address, .. }) => *batcher_address,
            Self::Ecotone(L1BlockInfoEcotone { batcher_address, .. }) => *batcher_address,
        }
    }

    /// Returns the sequence number for the info transaction
    pub const fn sequence_number(&self) -> u64 {
        match self {
            Self::Bedrock(L1BlockInfoBedrock { sequence_number, .. }) => *sequence_number,
            Self::Ecotone(L1BlockInfoEcotone { sequence_number, .. }) => *sequence_number,
        }
    }
}

impl L1BlockInfoBedrock {
    /// Encodes the [L1BlockInfoBedrock] object into Ethereum transaction calldata.
    pub fn encode_calldata(&self) -> Bytes {
        let mut buf = Vec::with_capacity(L1_INFO_TX_LEN_BEDROCK);
        buf.extend_from_slice(L1_INFO_TX_SELECTOR_BEDROCK.as_ref());
        buf.extend_from_slice(U256::from(self.number).to_be_bytes::<32>().as_slice());
        buf.extend_from_slice(U256::from(self.time).to_be_bytes::<32>().as_slice());
        buf.extend_from_slice(U256::from(self.base_fee).to_be_bytes::<32>().as_slice());
        buf.extend_from_slice(self.block_hash.as_slice());
        buf.extend_from_slice(U256::from(self.sequence_number).to_be_bytes::<32>().as_slice());
        buf.extend_from_slice(self.batcher_address.into_word().as_slice());
        buf.extend_from_slice(self.l1_fee_overhead.to_be_bytes::<32>().as_slice());
        buf.extend_from_slice(self.l1_fee_scalar.to_be_bytes::<32>().as_slice());
        buf.into()
    }

    /// Decodes the [L1BlockInfoBedrock] object from ethereum transaction calldata.
    pub fn decode_calldata(r: &[u8]) -> Result<Self, DecodeError> {
        if r.len() != L1_INFO_TX_LEN_BEDROCK {
            return Err(DecodeError::InvalidLength(format!(
                "Invalid calldata length for Bedrock L1 info transaction, expected {}, got {}",
                L1_INFO_TX_LEN_BEDROCK,
                r.len()
            )));
        }

        let number = u64::from_be_bytes(
            r[28..36]
                .try_into()
                .map_err(|_| DecodeError::ParseError("Conversion error for number".to_string()))?,
        );
        let time = u64::from_be_bytes(
            r[60..68]
                .try_into()
                .map_err(|_| DecodeError::ParseError("Conversion error for time".to_string()))?,
        );
        let base_fee =
            u64::from_be_bytes(r[92..100].try_into().map_err(|_| {
                DecodeError::ParseError("Conversion error for base fee".to_string())
            })?);
        let block_hash = B256::from_slice(r[100..132].as_ref());
        let sequence_number = u64::from_be_bytes(r[156..164].try_into().map_err(|_| {
            DecodeError::ParseError("Conversion error for sequence number".to_string())
        })?);
        let batcher_address = Address::from_slice(r[176..196].as_ref());
        let l1_fee_overhead = U256::from_be_slice(r[196..228].as_ref());
        let l1_fee_scalar = U256::from_be_slice(r[228..260].as_ref());

        Ok(Self {
            number,
            time,
            base_fee,
            block_hash,
            sequence_number,
            batcher_address,
            l1_fee_overhead,
            l1_fee_scalar,
        })
    }
}

impl L1BlockInfoEcotone {
    /// Encodes the [L1BlockInfoEcotone] object into Ethereum transaction calldata.
    pub fn encode_calldata(&self) -> Bytes {
        let mut buf = Vec::with_capacity(L1_INFO_TX_LEN_ECOTONE);
        buf.extend_from_slice(L1_INFO_TX_SELECTOR_ECOTONE.as_ref());
        buf.extend_from_slice(self.base_fee_scalar.to_be_bytes().as_ref());
        buf.extend_from_slice(self.blob_base_fee_scalar.to_be_bytes().as_ref());
        buf.extend_from_slice(self.sequence_number.to_be_bytes().as_ref());
        buf.extend_from_slice(self.time.to_be_bytes().as_ref());
        buf.extend_from_slice(self.number.to_be_bytes().as_ref());
        buf.extend_from_slice(U256::from(self.base_fee).to_be_bytes::<32>().as_ref());
        buf.extend_from_slice(U256::from(self.blob_base_fee).to_be_bytes::<32>().as_ref());
        buf.extend_from_slice(self.block_hash.as_ref());
        buf.extend_from_slice(self.batcher_address.into_word().as_ref());
        buf.into()
    }

    /// Decodes the [L1BlockInfoEcotone] object from ethereum transaction calldata.
    pub fn decode_calldata(r: &[u8]) -> Result<Self, DecodeError> {
        if r.len() != L1_INFO_TX_LEN_ECOTONE {
            return Err(DecodeError::InvalidLength(format!(
                "Invalid calldata length for Ecotone L1 info transaction, expected {}, got {}",
                L1_INFO_TX_LEN_ECOTONE,
                r.len()
            )));
        }
        let base_fee_scalar = u32::from_be_bytes(r[4..8].try_into().map_err(|_| {
            DecodeError::ParseError("Conversion error for base fee scalar".to_string())
        })?);
        let blob_base_fee_scalar = u32::from_be_bytes(r[8..12].try_into().map_err(|_| {
            DecodeError::ParseError("Conversion error for blob base fee scalar".to_string())
        })?);
        let sequence_number = u64::from_be_bytes(r[12..20].try_into().map_err(|_| {
            DecodeError::ParseError("Conversion error for sequence number".to_string())
        })?);
        let timestamp =
            u64::from_be_bytes(r[20..28].try_into().map_err(|_| {
                DecodeError::ParseError("Conversion error for timestamp".to_string())
            })?);
        let l1_block_number = u64::from_be_bytes(r[28..36].try_into().map_err(|_| {
            DecodeError::ParseError("Conversion error for L1 block number".to_string())
        })?);
        let base_fee =
            u64::from_be_bytes(r[60..68].try_into().map_err(|_| {
                DecodeError::ParseError("Conversion error for base fee".to_string())
            })?);
        let blob_base_fee = u128::from_be_bytes(r[84..100].try_into().map_err(|_| {
            DecodeError::ParseError("Conversion error for blob base fee".to_string())
        })?);
        let block_hash = B256::from_slice(r[100..132].as_ref());
        let batcher_address = Address::from_slice(r[144..164].as_ref());

        Ok(Self {
            number: l1_block_number,
            time: timestamp,
            base_fee,
            block_hash,
            sequence_number,
            batcher_address,
            blob_base_fee,
            blob_base_fee_scalar,
            base_fee_scalar,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[cfg(not(feature = "std"))]
    use alloc::string::ToString;
    use alloy_primitives::{address, b256, hex};

    const RAW_BEDROCK_INFO_TX: [u8; L1_INFO_TX_LEN_BEDROCK] = hex!("015d8eb9000000000000000000000000000000000000000000000000000000000117c4eb0000000000000000000000000000000000000000000000000000000065280377000000000000000000000000000000000000000000000000000000026d05d953392012032675be9f94aae5ab442de73c5f4fb1bf30fa7dd0d2442239899a40fc00000000000000000000000000000000000000000000000000000000000000040000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f3298500000000000000000000000000000000000000000000000000000000000000bc00000000000000000000000000000000000000000000000000000000000a6fe0");
    const RAW_ECOTONE_INFO_TX: [u8; L1_INFO_TX_LEN_ECOTONE] = hex!("440a5e2000000558000c5fc5000000000000000500000000661c277300000000012bec20000000000000000000000000000000000000000000000000000000026e9f109900000000000000000000000000000000000000000000000000000000000000011c4c84c50740386c7dc081efddd644405f04cde73e30a2e381737acce9f5add30000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985");

    #[test]
    fn bedrock_l1_block_info_invalid_len() {
        let err = L1BlockInfoBedrock::decode_calldata(&[0xde, 0xad]);
        assert!(err.is_err());
        assert_eq!(
            err.err().unwrap().to_string(),
            "Invalid data length: Invalid calldata length for Bedrock L1 info transaction, expected 260, got 2"
        );
    }

    #[test]
    fn ecotone_l1_block_info_invalid_len() {
        let err = L1BlockInfoEcotone::decode_calldata(&[0xde, 0xad]);
        assert!(err.is_err());
        assert_eq!(
            err.err().unwrap().to_string(),
            "Invalid data length: Invalid calldata length for Ecotone L1 info transaction, expected 164, got 2"
        );
    }

    #[test]
    fn bedrock_l1_block_info_tx_roundtrip() {
        let expected = L1BlockInfoBedrock {
            number: 18334955,
            time: 1697121143,
            base_fee: 10419034451,
            block_hash: b256!("392012032675be9f94aae5ab442de73c5f4fb1bf30fa7dd0d2442239899a40fc"),
            sequence_number: 4,
            batcher_address: address!("6887246668a3b87f54deb3b94ba47a6f63f32985"),
            l1_fee_overhead: U256::from(0xbc),
            l1_fee_scalar: U256::from(0xa6fe0),
        };

        let L1BlockInfoTx::Bedrock(decoded) =
            L1BlockInfoTx::decode_calldata(RAW_BEDROCK_INFO_TX.as_ref()).unwrap()
        else {
            panic!("Wrong fork");
        };
        assert_eq!(expected, decoded);
        assert_eq!(RAW_BEDROCK_INFO_TX, decoded.encode_calldata().as_ref());
    }

    #[test]
    fn ecotone_l1_block_info_tx_roundtrip() {
        let expected = L1BlockInfoEcotone {
            number: 19655712,
            time: 1713121139,
            base_fee: 10445852825,
            block_hash: b256!("1c4c84c50740386c7dc081efddd644405f04cde73e30a2e381737acce9f5add3"),
            sequence_number: 5,
            batcher_address: address!("6887246668a3b87f54deb3b94ba47a6f63f32985"),
            blob_base_fee: 1,
            blob_base_fee_scalar: 810949,
            base_fee_scalar: 1368,
        };

        let L1BlockInfoTx::Ecotone(decoded) =
            L1BlockInfoTx::decode_calldata(RAW_ECOTONE_INFO_TX.as_ref()).unwrap()
        else {
            panic!("Wrong fork");
        };
        assert_eq!(expected, decoded);
        assert_eq!(decoded.encode_calldata().as_ref(), RAW_ECOTONE_INFO_TX);
    }

    #[test]
    fn try_new_with_deposit_tx_bedrock() {
        let rollup_config = RollupConfig::default();
        let system_config = SystemConfig::default();
        let sequence_number = 0;
        let l1_header = Header::default();
        let l2_block_time = 0;

        let l1_info = L1BlockInfoTx::try_new(
            &rollup_config,
            &system_config,
            sequence_number,
            &l1_header,
            l2_block_time,
        )
        .unwrap();

        let L1BlockInfoTx::Bedrock(l1_info) = l1_info else {
            panic!("Wrong fork");
        };

        assert_eq!(l1_info.number, l1_header.number);
        assert_eq!(l1_info.time, l1_header.timestamp);
        assert_eq!(l1_info.base_fee, l1_header.base_fee_per_gas.unwrap_or(0) as u64);
        assert_eq!(l1_info.block_hash, l1_header.hash_slow());
        assert_eq!(l1_info.sequence_number, sequence_number);
        assert_eq!(l1_info.batcher_address, system_config.batcher_address);
        assert_eq!(l1_info.l1_fee_overhead, system_config.overhead);
        assert_eq!(l1_info.l1_fee_scalar, system_config.scalar);
    }

    #[test]
    fn try_new_with_deposit_tx_ecotone() {
        let rollup_config = RollupConfig { ecotone_time: Some(1), ..Default::default() };
        let system_config = SystemConfig::default();
        let sequence_number = 0;
        let l1_header = Header::default();
        let l2_block_time = 0xFF;

        let l1_info = L1BlockInfoTx::try_new(
            &rollup_config,
            &system_config,
            sequence_number,
            &l1_header,
            l2_block_time,
        )
        .unwrap();

        let L1BlockInfoTx::Ecotone(l1_info) = l1_info else {
            panic!("Wrong fork");
        };

        assert_eq!(l1_info.number, l1_header.number);
        assert_eq!(l1_info.time, l1_header.timestamp);
        assert_eq!(l1_info.base_fee, l1_header.base_fee_per_gas.unwrap_or(0) as u64);
        assert_eq!(l1_info.block_hash, l1_header.hash_slow());
        assert_eq!(l1_info.sequence_number, sequence_number);
        assert_eq!(l1_info.batcher_address, system_config.batcher_address);
        assert_eq!(l1_info.blob_base_fee, l1_header.blob_fee().unwrap_or(1));

        let scalar = system_config.scalar.to_be_bytes::<32>();
        let blob_base_fee_scalar = (scalar[0] == L1_SCALAR_ECOTONE)
            .then(|| {
                u32::from_be_bytes(
                    scalar[24..28].try_into().expect("Failed to parse L1 blob base fee scalar"),
                )
            })
            .unwrap_or_default();
        let base_fee_scalar =
            u32::from_be_bytes(scalar[28..32].try_into().expect("Failed to parse base fee scalar"));
        assert_eq!(l1_info.blob_base_fee_scalar, blob_base_fee_scalar);
        assert_eq!(l1_info.base_fee_scalar, base_fee_scalar);
    }
}
