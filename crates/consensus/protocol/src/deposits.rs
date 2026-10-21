//! Contains deposit transaction types and helper methods.

use alloc::vec::Vec;

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, Log, TxKind, U256, b256};
use base_common_consensus::{TxDeposit, UserDepositSource};

/// Deposit transaction utilities.
#[derive(Debug)]
pub struct Deposits;

impl Deposits {
    /// Deposit log event ABI signature.
    pub const EVENT_ABI: &'static str = "TransactionDeposited(address,address,uint256,bytes)";

    /// Deposit event ABI hash.
    ///
    /// This is the keccak256 hash of the deposit event ABI signature.
    /// `keccak256("TransactionDeposited(address,address,uint256,bytes)")`
    pub const EVENT_ABI_HASH: B256 =
        b256!("b3813568d9991fc951961fcb4c784893574240a28925604d09fc577c55bb7c32");

    /// The initial version of the deposit event log.
    pub const EVENT_VERSION_0: B256 = B256::ZERO;

    /// Derives a deposit transaction from an EVM log event emitted by the deposit contract.
    ///
    /// The emitted log must be in format:
    /// ```solidity
    /// event TransactionDeposited(
    ///    address indexed from,
    ///    address indexed to,
    ///    uint256 indexed version,
    ///    bytes opaqueData
    /// );
    /// ```
    pub fn decode(block_hash: B256, index: usize, log: &Log) -> Result<Bytes, DepositDecodeError> {
        let topics = log.data.topics();
        if topics.len() != 4 {
            return Err(DepositDecodeError::UnexpectedTopicsLen(topics.len()));
        }
        if topics[0] != Self::EVENT_ABI_HASH {
            return Err(DepositDecodeError::InvalidSelector(Self::EVENT_ABI_HASH, topics[0]));
        }
        if log.data.data.len() < 64 {
            return Err(DepositDecodeError::IncompleteOpaqueData(log.data.data.len()));
        }
        if log.data.data.len() % 32 != 0 {
            return Err(DepositDecodeError::UnalignedData(log.data.data.len()));
        }

        // Validate the `from` address.
        let mut from_bytes = [0u8; 20];
        from_bytes.copy_from_slice(&topics[1].as_slice()[12..]);
        if topics[1].iter().take(12).any(|&b| b != 0) {
            return Err(DepositDecodeError::FromDecode(topics[1]));
        }

        // Validate the `to` address.
        let mut to_bytes = [0u8; 20];
        to_bytes.copy_from_slice(&topics[2].as_slice()[12..]);
        if topics[2].iter().take(12).any(|&b| b != 0) {
            return Err(DepositDecodeError::ToDecode(topics[2]));
        }

        let from = Address::from(from_bytes);
        let to = Address::from(to_bytes);
        let version = log.data.topics()[3];

        // Solidity serializes the event's Data field as follows:
        //
        // ```solidity
        // abi.encode(abi.encodPacked(uint256 mint, uint256 value, uint64 gasLimit, uint8 isCreation, bytes data))
        // ```
        //
        // The opaqueData will be packed as shown below:
        //
        // ------------------------------------------------------------
        // | offset | 256 byte content                                |
        // ------------------------------------------------------------
        // | 0      | [0; 24] . {U64 big endian, hex encoded offset}  |
        // ------------------------------------------------------------
        // | 32     | [0; 24] . {U64 big endian, hex encoded length}  |
        // ------------------------------------------------------------

        let opaque_content_offset: U256 = U256::from_be_slice(&log.data.data[0..32]);
        if opaque_content_offset != U256::from(32) {
            return Err(DepositDecodeError::InvalidOpaqueDataOffset(Bytes::copy_from_slice(
                &log.data.data[0..32],
            )));
        }

        // The next 32 bytes indicate the length of the opaqueData content.
        let opaque_content_len: U256 = U256::from_be_slice(&log.data.data[32..64]);
        let opaque_content_len: u64 = opaque_content_len.try_into().map_err(|_| {
            DepositDecodeError::OpaqueContentOverflow(Bytes::copy_from_slice(
                &log.data.data[32..64],
            ))
        })?;

        let opaque_data_ceil_32: u64 =
            (opaque_content_len.saturating_add(31) / 32).saturating_mul(32);

        // Ensure that the remaining data is only zeros.
        // The padding ends at the next multiple of 32 after the opaque data.
        let Some(padding_end): Option<u64> = 64_u64.checked_add(opaque_data_ceil_32) else {
            return Err(DepositDecodeError::OpaqueDataPaddingOverflow);
        };

        // The remaining data is the opaqueData which is tightly packed and then padded to 32 bytes
        // by the EVM.
        let Some(opaque_data) = &log.data.data.get(64..64 + opaque_content_len as usize) else {
            return Err(DepositDecodeError::InvalidOpaqueDataLength {
                expected: opaque_content_len as usize,
                actual: log.data.data.len().saturating_sub(64),
            });
        };

        if !(opaque_content_len.is_multiple_of(32)
            || log
                .data
                .data
                .get((64 + opaque_content_len) as usize..padding_end as usize)
                .is_some_and(|data| data.iter().all(|&b| b == 0)))
        {
            return Err(DepositDecodeError::InvalidOpaqueDataPadding(Bytes::copy_from_slice(
                &log.data.data[(64 + opaque_content_len) as usize..],
            )));
        }

        let source = UserDepositSource::new(block_hash, index as u64);

        let mut deposit_tx = TxDeposit {
            from,
            is_system_transaction: false,
            source_hash: source.source_hash(),
            ..Default::default()
        };

        // Can only handle version 0 for now
        if !version.is_zero() {
            return Err(DepositDecodeError::InvalidVersion(version));
        }

        Self::unmarshal_v0(&mut deposit_tx, to, opaque_data)?;

        // Re-encode the deposit transaction
        let mut buffer = Vec::with_capacity(deposit_tx.eip2718_encoded_length());
        deposit_tx.encode_2718(&mut buffer);
        Ok(Bytes::from(buffer))
    }
}

/// An [`TxDeposit`] validation error.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DepositDecodeError {
    /// Unexpected number of deposit event log topics.
    #[error("Unexpected number of deposit event log topics: {0}")]
    UnexpectedTopicsLen(usize),
    /// Invalid deposit event selector.
    /// Expected: [`B256`] (deposit event selector), Actual: [`B256`] (event log topic).
    #[error("Invalid deposit event selector: {1}, expected {0}")]
    InvalidSelector(B256, B256),
    /// Incomplete opaqueData slice header (incomplete length).
    #[error("Incomplete opaqueData slice header (incomplete length): {0}")]
    IncompleteOpaqueData(usize),
    /// The log data is not aligned to 32 bytes.
    #[error("Unaligned log data, expected multiple of 32 bytes, got: {0}")]
    UnalignedData(usize),
    /// Failed to decode the `from` field of the deposit event (the second topic).
    #[error("Failed to decode the `from` address of the deposit log topic: {0}")]
    FromDecode(B256),
    /// Failed to decode the `to` field of the deposit event (the third topic).
    #[error("Failed to decode the `to` address of the deposit log topic: {0}")]
    ToDecode(B256),
    /// Invalid opaque data content offset.
    #[error("Invalid u64 opaque data content offset: {0}")]
    InvalidOpaqueDataOffset(Bytes),
    /// Invalid opaque data content length.
    #[error("Invalid u64 opaque data content length: expected {expected}, actual {actual}")]
    InvalidOpaqueDataLength {
        /// Expected length.
        expected: usize,
        /// Actual length.
        actual: usize,
    },
    /// Invalid opaque data.
    #[error("Invalid opaque data padding. Not all zeros or incorrect length: {0}")]
    InvalidOpaqueDataPadding(Bytes),
    /// Opaque content length overflow.
    #[error("Opaque content length overflow: {0}")]
    OpaqueContentOverflow(Bytes),
    /// Opaque data length exceeds the deposit log event data length.
    /// Specified: [usize] (data length), Actual: [usize] (opaque data length).
    #[error("Specified opaque data length {1} exceeds the deposit log event data length {0}")]
    OpaqueDataOverflow(u64, usize),
    /// Opaque data padding overflow.
    #[error("Opaque data padding overflow")]
    OpaqueDataPaddingOverflow,
    /// Opaque data with padding exceeds the specified data length.
    /// Specified: [usize] (data length), Actual: [usize] (opaque data length).
    #[error("Opaque data with padding exceeds the specified data length: {1} > {0}")]
    PaddedOpaqueDataOverflow(usize, u64),
    /// An invalid deposit version.
    #[error("Invalid deposit version: {0}")]
    InvalidVersion(B256),
    /// Unexpected opaque data length.
    #[error("Unexpected opaque data length: {0}")]
    UnexpectedOpaqueDataLen(usize),
    /// Failed to decode the deposit mint value.
    #[error("Failed to decode the u128 deposit mint value: {0}")]
    MintDecode(Bytes),
    /// Failed to decode the deposit gas value.
    #[error("Failed to decode the u64 deposit gas value: {0}")]
    GasDecode(Bytes),
}

impl Deposits {
    /// Unmarshals a deposit transaction from the opaque data.
    pub fn unmarshal_v0(
        tx: &mut TxDeposit,
        to: Address,
        data: &[u8],
    ) -> Result<(), DepositDecodeError> {
        if data.len() < 32 + 32 + 8 + 1 {
            return Err(DepositDecodeError::UnexpectedOpaqueDataLen(data.len()));
        }

        let mut offset = 0;

        let raw_mint: [u8; 16] = data[offset + 16..offset + 32].try_into().map_err(|_| {
            DepositDecodeError::MintDecode(Bytes::copy_from_slice(&data[offset + 16..offset + 32]))
        })?;
        tx.mint = u128::from_be_bytes(raw_mint);
        offset += 32;

        // uint256 value
        tx.value = U256::from_be_slice(&data[offset..offset + 32]);
        offset += 32;

        // uint64 gas
        let raw_gas: [u8; 8] = data[offset..offset + 8].try_into().map_err(|_| {
            DepositDecodeError::GasDecode(Bytes::copy_from_slice(&data[offset..offset + 8]))
        })?;
        tx.gas_limit = u64::from_be_bytes(raw_gas);
        offset += 8;

        // uint8 isCreation
        // isCreation: If the boolean byte is 1 then dep.To will stay nil,
        // and it will create a contract using L2 account nonce to determine the created address.
        if data[offset] == 0 {
            tx.to = TxKind::Call(to);
        } else {
            tx.to = TxKind::Create;
        }
        offset += 1;

        // The remainder of the opaqueData is the transaction data (without length prefix).
        // The data may be padded to a multiple of 32 bytes
        let tx_data_len = data.len() - offset;

        // Remaining bytes fill the data
        tx.input = Bytes::copy_from_slice(&data[offset..offset + tx_data_len]);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_primitives::{LogData, U64, U128, address, b256, hex};
    use rstest::rstest;

    use super::*;

    const SENTINEL: B256 =
        b256!("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF");
    const VERSION_ONE: B256 =
        b256!("0000000000000000000000000000000000000000000000000000000000000001");

    /// Returns `data` with `bytes` written at `at`.
    fn put(mut data: Vec<u8>, at: usize, bytes: &[u8]) -> Vec<u8> {
        data[at..at + bytes.len()].copy_from_slice(bytes);
        data
    }

    /// Returns zeroed opaque data of `total` bytes with offset 32 and the given length field.
    fn opaque(total: usize, len: u64) -> Vec<u8> {
        let data = put(vec![0u8; total], 24, &U64::from(32).to_be_bytes::<8>());
        put(data, 56, &U64::from(len).to_be_bytes::<8>())
    }

    /// Returns a valid, populated version-0 deposit payload.
    fn valid_opaque() -> Vec<u8> {
        let data = put(opaque(192, 128), 80, &10_u128.to_be_bytes());
        let data = put(data, 96, &U256::from(100).to_be_bytes::<32>());
        let data = put(data, 128, &1000_u64.to_be_bytes());
        put(data, 136, &[1])
    }

    /// Returns the `from` and `to` topics that match [`valid_opaque`].
    fn valid_topics() -> (B256, B256) {
        (
            B256::left_padding_from(
                address!("1111111111111111111111111111111111111111").as_slice(),
            ),
            B256::left_padding_from(
                address!("2222222222222222222222222222222222222222").as_slice(),
            ),
        )
    }

    fn decode(
        from: B256,
        to: B256,
        version: B256,
        data: Vec<u8>,
    ) -> Result<Bytes, DepositDecodeError> {
        let log = Log {
            address: Address::default(),
            data: LogData::new_unchecked(
                vec![Deposits::EVENT_ABI_HASH, from, to, version],
                Bytes::from(data),
            ),
        };
        Deposits::decode(B256::default(), 0, &log)
    }

    #[test]
    fn test_decode_deposit_invalid_first_topic() {
        let log = Log {
            address: Address::default(),
            data: LogData::new_unchecked(vec![B256::default(); 4], Bytes::default()),
        };
        let err: DepositDecodeError = Deposits::decode(B256::default(), 0, &log).unwrap_err();
        assert_eq!(
            err,
            DepositDecodeError::InvalidSelector(Deposits::EVENT_ABI_HASH, B256::default())
        );
    }

    #[rstest]
    #[case::incomplete_data(
        B256::ZERO, B256::ZERO, B256::ZERO, vec![0u8; 63],
        DepositDecodeError::IncompleteOpaqueData(63)
    )]
    #[case::unaligned_data(
        B256::ZERO, B256::ZERO, B256::ZERO, vec![0u8; 65],
        DepositDecodeError::UnalignedData(65)
    )]
    #[case::invalid_from(
        SENTINEL, B256::ZERO, B256::ZERO, vec![0u8; 64],
        DepositDecodeError::FromDecode(SENTINEL)
    )]
    #[case::invalid_to(
        B256::ZERO, SENTINEL, B256::ZERO, vec![0u8; 64],
        DepositDecodeError::ToDecode(SENTINEL)
    )]
    #[case::invalid_opaque_data_offset(
        B256::ZERO, B256::ZERO, B256::ZERO, vec![0u8; 64],
        DepositDecodeError::InvalidOpaqueDataOffset(Bytes::from(vec![0u8; 32]))
    )]
    #[case::opaque_data_overflow(
        B256::ZERO, B256::ZERO, B256::ZERO, opaque(128, 128),
        DepositDecodeError::InvalidOpaqueDataLength { expected: 128, actual: 64 }
    )]
    #[case::padded_overflow(
        B256::ZERO,
        B256::ZERO,
        B256::ZERO,
        opaque(256, 64),
        DepositDecodeError::UnexpectedOpaqueDataLen(64)
    )]
    #[case::invalid_version(
        B256::ZERO,
        B256::ZERO,
        VERSION_ONE,
        opaque(128, 64),
        DepositDecodeError::InvalidVersion(VERSION_ONE)
    )]
    #[case::offset_overflow(
        B256::ZERO, B256::ZERO, B256::ZERO,
        put(opaque(128, 128), 16, &U128::MAX.to_be_bytes::<16>()),
        DepositDecodeError::InvalidOpaqueDataOffset(Bytes::from(put(
            vec![0u8; 32], 16, &U128::MAX.to_be_bytes::<16>()
        )))
    )]
    #[case::length_overflow(
        B256::ZERO, B256::ZERO, B256::ZERO,
        put(opaque(128, 128), 48, &U128::MAX.to_be_bytes::<16>()),
        DepositDecodeError::OpaqueContentOverflow(Bytes::from(put(
            vec![0u8; 32], 16, &U128::MAX.to_be_bytes::<16>()
        )))
    )]
    #[case::opaque_data_length_mismatch(
        valid_topics().0, valid_topics().1, B256::ZERO,
        put(valid_opaque(), 56, &U64::from(129).to_be_bytes::<8>()),
        DepositDecodeError::InvalidOpaqueDataLength { expected: 129, actual: 128 }
    )]
    #[case::padding_overflow(
        valid_topics().0, valid_topics().1, B256::ZERO,
        put(valid_opaque(), 56, &U64::MAX.to_be_bytes::<8>()),
        DepositDecodeError::OpaqueDataPaddingOverflow
    )]
    #[case::invalid_padding(
        valid_topics().0, valid_topics().1, B256::ZERO,
        put(put(valid_opaque(), 56, &U64::from(127).to_be_bytes::<8>()), 191, &[1]),
        DepositDecodeError::InvalidOpaqueDataPadding(Bytes::from(vec![1u8]))
    )]
    fn test_decode_deposit_errors(
        #[case] from: B256,
        #[case] to: B256,
        #[case] version: B256,
        #[case] data: Vec<u8>,
        #[case] expected: DepositDecodeError,
    ) {
        assert_eq!(decode(from, to, version, data).unwrap_err(), expected);
    }

    #[test]
    fn test_decode_deposit_empty_succeeds() {
        let valid_to = b256!("000000000000000000000000bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let valid_from = b256!("000000000000000000000000FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF");
        let tx = decode(valid_from, valid_to, B256::ZERO, opaque(192, 128)).unwrap();
        let raw_hex = hex!(
            "7ef887a0ed428e1c45e1d9561b62834e1a2d3015a0caae3bfdc16b4da059ac885b01a14594ffffffffffffffffffffffffffffffffffffffff94bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb80808080b700000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(tx, Bytes::from(raw_hex));
    }

    #[test]
    fn test_decode_deposit_full_succeeds() {
        let (from, to) = valid_topics();
        let tx = decode(from, to, B256::ZERO, valid_opaque()).unwrap();
        let raw_hex = hex!(
            "7ef875a0ed428e1c45e1d9561b62834e1a2d3015a0caae3bfdc16b4da059ac885b01a145941111111111111111111111111111111111111111800a648203e880b700000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(tx, Bytes::from(raw_hex));
    }

    #[test]
    fn test_unmarshal_deposit_version0_invalid_len() {
        let to = address!("5555555555555555555555555555555555555555");
        let err = Deposits::unmarshal_v0(&mut TxDeposit::default(), to, &[0u8; 72]).unwrap_err();
        assert_eq!(err, DepositDecodeError::UnexpectedOpaqueDataLen(72));

        // Data must have at least length 73
        Deposits::unmarshal_v0(&mut TxDeposit::default(), to, &[0u8; 73]).unwrap();
    }

    #[test]
    fn test_unmarshal_v0() {
        let mut tx = TxDeposit {
            from: address!("1111111111111111111111111111111111111111"),
            to: TxKind::Call(address!("2222222222222222222222222222222222222222")),
            value: U256::from(100),
            gas_limit: 1000,
            mint: 10,
            ..Default::default()
        };
        let to = address!("5555555555555555555555555555555555555555");
        Deposits::unmarshal_v0(&mut tx, to, &valid_opaque()).unwrap();
        assert_eq!(tx.to, TxKind::Call(to));
    }
}
