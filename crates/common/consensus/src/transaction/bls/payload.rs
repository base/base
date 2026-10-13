//! Input payload codec for the development-only native BLS transaction type.

use alloc::vec::Vec;

use alloy_primitives::{Address, B256, Bytes, FixedBytes, U256, aliases::U96, keccak256};

use crate::BlsConstants;

/// Errors returned when a [`crate::TxBls`] input is not a canonical [`BlsPayload`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BlsPayloadError {
    /// The input is empty, so it has no kind byte.
    #[error("empty BLS payload")]
    Empty,
    /// The kind byte is neither registration nor batch.
    #[error("unknown BLS payload kind {0}")]
    UnknownKind(u8),
    /// The input length does not match the exact length implied by its kind and count.
    #[error("invalid BLS payload length: expected {expected}, got {actual}")]
    InvalidLength {
        /// Exact length required by the payload kind and transfer count.
        expected: usize,
        /// Length of the supplied input.
        actual: usize,
    },
    /// The batch transfer count is zero or exceeds [`BlsConstants::MAX_TRANSFERS`].
    #[error("invalid BLS transfer count {0}")]
    InvalidTransferCount(usize),
    /// Batch senders are not strictly ascending, so they are unsorted or duplicated.
    #[error("BLS transfer senders are not strictly ascending at index {0}")]
    UnsortedSenders(usize),
    /// The public key is not a canonical, non-infinity compressed G2 encoding.
    #[error("non-canonical compressed BLS public key")]
    NonCanonicalPublicKey,
    /// The proof of possession or aggregate signature is not a canonical, non-infinity
    /// compressed G1 encoding.
    #[error("non-canonical compressed BLS signature")]
    NonCanonicalSignature,
}

/// Decoded input of a [`crate::TxBls`].
///
/// Decoding enforces exact lengths, the transfer count bounds, strictly ascending senders and
/// canonical compressed point encodings (compression flag set, no point at infinity, every
/// coordinate below the field modulus). It does not check that a point lies on the curve or in
/// the prime-order subgroup; execution must run full BLS key and signature validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlsPayload {
    /// Registers an immutable BLS public key for the outer ECDSA signer.
    Registration {
        /// Compressed G2 public key.
        pubkey: FixedBytes<96>,
        /// Compressed G1 proof of possession over `pubkey`.
        pop: FixedBytes<48>,
    },
    /// Executes ETH transfers authorized by one aggregate signature of their senders.
    Batch {
        /// Transfers with strictly ascending senders.
        transfers: Vec<BlsTransfer>,
        /// Compressed G1 aggregate signature over every transfer signing digest.
        signature: FixedBytes<48>,
    },
}

impl BlsPayload {
    /// Decodes and validates a canonical payload from a transaction input.
    pub fn decode(input: &[u8]) -> Result<Self, BlsPayloadError> {
        let (&kind, rest) = input.split_first().ok_or(BlsPayloadError::Empty)?;
        match kind {
            BlsConstants::KIND_REGISTRATION => {
                if input.len() != BlsConstants::REGISTRATION_INPUT_LEN {
                    return Err(BlsPayloadError::InvalidLength {
                        expected: BlsConstants::REGISTRATION_INPUT_LEN,
                        actual: input.len(),
                    });
                }
                let (pubkey, pop) = rest.split_at(BlsConstants::PUBKEY_LEN);
                if !Self::is_canonical_point(pubkey) {
                    return Err(BlsPayloadError::NonCanonicalPublicKey);
                }
                if !Self::is_canonical_point(pop) {
                    return Err(BlsPayloadError::NonCanonicalSignature);
                }
                Ok(Self::Registration {
                    pubkey: FixedBytes::from_slice(pubkey),
                    pop: FixedBytes::from_slice(pop),
                })
            }
            BlsConstants::KIND_BATCH => {
                let (&count, records) =
                    rest.split_first().ok_or(BlsPayloadError::InvalidLength {
                        expected: BlsConstants::batch_input_len(1),
                        actual: input.len(),
                    })?;
                let count = count as usize;
                if count == 0 || count > BlsConstants::MAX_TRANSFERS {
                    return Err(BlsPayloadError::InvalidTransferCount(count));
                }
                let expected = BlsConstants::batch_input_len(count);
                if input.len() != expected {
                    return Err(BlsPayloadError::InvalidLength { expected, actual: input.len() });
                }
                let (records, signature) = records.split_at(count * BlsConstants::TRANSFER_LEN);
                let transfers: Vec<BlsTransfer> = records
                    .chunks_exact(BlsConstants::TRANSFER_LEN)
                    .map(|record| BlsTransfer::decode(record.try_into().expect("exact chunk")))
                    .collect();
                if let Some(index) =
                    transfers.windows(2).position(|pair| pair[0].sender >= pair[1].sender)
                {
                    return Err(BlsPayloadError::UnsortedSenders(index + 1));
                }
                if !Self::is_canonical_point(signature) {
                    return Err(BlsPayloadError::NonCanonicalSignature);
                }
                Ok(Self::Batch { transfers, signature: FixedBytes::from_slice(signature) })
            }
            kind => Err(BlsPayloadError::UnknownKind(kind)),
        }
    }

    /// Encodes the payload as transaction input.
    ///
    /// Only payloads that [`Self::decode`] accepts round-trip; constructing a batch with more
    /// than 255 transfers truncates its count byte.
    pub fn encode(&self) -> Bytes {
        let mut out = Vec::new();
        match self {
            Self::Registration { pubkey, pop } => {
                out.reserve_exact(BlsConstants::REGISTRATION_INPUT_LEN);
                out.push(BlsConstants::KIND_REGISTRATION);
                out.extend_from_slice(pubkey.as_slice());
                out.extend_from_slice(pop.as_slice());
            }
            Self::Batch { transfers, signature } => {
                out.reserve_exact(BlsConstants::batch_input_len(transfers.len()));
                out.push(BlsConstants::KIND_BATCH);
                out.push(transfers.len() as u8);
                for transfer in transfers {
                    out.extend_from_slice(&transfer.encode());
                }
                out.extend_from_slice(signature.as_slice());
            }
        }
        out.into()
    }

    /// Returns whether `bytes` is a canonical, non-infinity compressed BLS12-381 G1 (48-byte)
    /// or G2 (96-byte) point encoding.
    ///
    /// The first byte carries the compression, infinity and sign flags in its top three bits.
    /// Every 48-byte coordinate must be strictly below the field modulus. Curve and subgroup
    /// membership are not checked.
    pub fn is_canonical_point(bytes: &[u8]) -> bool {
        const COMPRESSION_FLAG: u8 = 0x80;
        const INFINITY_FLAG: u8 = 0x40;
        const FLAG_MASK: u8 = 0xe0;
        if bytes.len() != BlsConstants::SIGNATURE_LEN && bytes.len() != BlsConstants::PUBKEY_LEN {
            return false;
        }
        if bytes[0] & COMPRESSION_FLAG == 0 || bytes[0] & INFINITY_FLAG != 0 {
            return false;
        }
        let mut first_coordinate = [0u8; 48];
        first_coordinate.copy_from_slice(&bytes[..48]);
        first_coordinate[0] &= !FLAG_MASK;
        core::iter::once(first_coordinate.as_slice())
            .chain(bytes[48..].chunks_exact(48))
            .all(|coordinate| coordinate < BlsConstants::FIELD_MODULUS.as_slice())
    }
}

/// One ETH transfer authorized by its sender's registered BLS key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct BlsTransfer {
    /// Native account debited by the transfer.
    pub sender: Address,
    /// Sender account nonce consumed by the transfer.
    pub nonce: u64,
    /// Native account credited by the transfer.
    pub recipient: Address,
    /// Transferred amount in wei.
    pub value: U96,
}

impl BlsTransfer {
    /// Encodes the record as `sender20 | nonce8 | recipient20 | value12`, big endian.
    pub fn encode(&self) -> [u8; BlsConstants::TRANSFER_LEN] {
        let mut out = [0u8; BlsConstants::TRANSFER_LEN];
        out[..20].copy_from_slice(self.sender.as_slice());
        out[20..28].copy_from_slice(&self.nonce.to_be_bytes());
        out[28..48].copy_from_slice(self.recipient.as_slice());
        out[48..].copy_from_slice(&self.value.to_be_bytes::<12>());
        out
    }

    /// Decodes a record encoded by [`Self::encode`]. Every 60-byte record is valid.
    pub fn decode(record: &[u8; BlsConstants::TRANSFER_LEN]) -> Self {
        Self {
            sender: Address::from_slice(&record[..20]),
            nonce: u64::from_be_bytes(record[20..28].try_into().expect("8 bytes")),
            recipient: Address::from_slice(&record[28..48]),
            value: U96::from_be_bytes::<12>(record[48..].try_into().expect("12 bytes")),
        }
    }

    /// Returns the message the sender signs:
    /// `keccak256(DOMAIN || chain_id as 32-byte big endian || record60)`.
    pub fn signing_digest(&self, chain_id: u64) -> B256 {
        let mut preimage = [0u8; 64 + BlsConstants::TRANSFER_LEN];
        preimage[..32].copy_from_slice(BlsConstants::DOMAIN.as_slice());
        preimage[32..64].copy_from_slice(&U256::from(chain_id).to_be_bytes::<32>());
        preimage[64..].copy_from_slice(&self.encode());
        keccak256(preimage)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn point(len: usize) -> Vec<u8> {
        let mut bytes = vec![0x11; len];
        bytes[0] = 0x80 | 0x20 | 0x11;
        bytes
    }

    fn transfer(sender: u8) -> BlsTransfer {
        BlsTransfer {
            sender: Address::repeat_byte(sender),
            nonce: 0x0102_0304_0506_0708,
            recipient: Address::repeat_byte(0xee),
            value: U96::from(0x0a0b_0c0d_u64),
        }
    }

    fn batch_input(transfers: &[BlsTransfer], count: u8) -> Vec<u8> {
        let mut input = vec![BlsConstants::KIND_BATCH, count];
        for transfer in transfers {
            input.extend_from_slice(&transfer.encode());
        }
        input.extend_from_slice(&point(48));
        input
    }

    #[test]
    fn transfer_record_layout_is_big_endian() {
        let record = BlsTransfer {
            sender: Address::repeat_byte(0xaa),
            nonce: 0x0102_0304_0506_0708,
            recipient: Address::repeat_byte(0xbb),
            value: U96::MAX,
        }
        .encode();
        let mut expected = vec![0xaa; 20];
        expected.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        expected.extend_from_slice(&[0xbb; 20]);
        expected.extend_from_slice(&[0xff; 12]);
        assert_eq!(record.as_slice(), expected.as_slice());
        assert_eq!(BlsTransfer::decode(&record).value, U96::MAX);
    }

    #[test]
    fn signing_digest_binds_domain_chain_and_record() {
        let transfer = transfer(0x01);
        let mut preimage = BlsConstants::DOMAIN.to_vec();
        preimage.extend_from_slice(&[0u8; 28]);
        preimage.extend_from_slice(&84_538_453_u32.to_be_bytes());
        preimage.extend_from_slice(&transfer.encode());
        assert_eq!(transfer.signing_digest(BlsConstants::CHAIN_ID), keccak256(&preimage));
        assert_ne!(transfer.signing_digest(BlsConstants::CHAIN_ID), transfer.signing_digest(8453));
    }

    #[test]
    fn registration_roundtrip() {
        let mut input = vec![BlsConstants::KIND_REGISTRATION];
        input.extend_from_slice(&point(96));
        input.extend_from_slice(&point(48));
        let payload = BlsPayload::decode(&input).unwrap();
        assert!(matches!(payload, BlsPayload::Registration { .. }));
        assert_eq!(payload.encode().as_ref(), input.as_slice());
    }

    #[test]
    fn batch_roundtrip_at_max_transfers() {
        let transfers: Vec<_> = (1..=32).map(transfer).collect();
        let input = batch_input(&transfers, 32);
        assert_eq!(input.len(), 2 + 32 * 60 + 48);
        let payload = BlsPayload::decode(&input).unwrap();
        let BlsPayload::Batch { transfers: decoded, .. } = &payload else {
            panic!("expected batch, got {payload:?}");
        };
        assert_eq!(decoded, &transfers);
        assert_eq!(payload.encode().as_ref(), input.as_slice());
    }

    #[test]
    fn rejects_malformed_payloads() {
        let mut registration = vec![BlsConstants::KIND_REGISTRATION];
        registration.extend_from_slice(&point(96));
        registration.extend_from_slice(&point(48));
        let two = [transfer(1), transfer(2)];

        let cases: Vec<(Vec<u8>, BlsPayloadError)> = vec![
            (vec![], BlsPayloadError::Empty),
            (vec![2], BlsPayloadError::UnknownKind(2)),
            (
                registration[..144].to_vec(),
                BlsPayloadError::InvalidLength { expected: 145, actual: 144 },
            ),
            (
                [registration.as_slice(), &[0]].concat(),
                BlsPayloadError::InvalidLength { expected: 145, actual: 146 },
            ),
            (vec![1], BlsPayloadError::InvalidLength { expected: 110, actual: 1 }),
            (batch_input(&[], 0), BlsPayloadError::InvalidTransferCount(0)),
            (
                batch_input(&(1..=33).map(transfer).collect::<Vec<_>>(), 33),
                BlsPayloadError::InvalidTransferCount(33),
            ),
            // Count byte disagrees with the number of records present.
            (batch_input(&two, 1), BlsPayloadError::InvalidLength { expected: 110, actual: 170 }),
            (batch_input(&two, 3), BlsPayloadError::InvalidLength { expected: 230, actual: 170 }),
            (batch_input(&[transfer(2), transfer(1)], 2), BlsPayloadError::UnsortedSenders(1)),
            (batch_input(&[transfer(1), transfer(1)], 2), BlsPayloadError::UnsortedSenders(1)),
        ];
        for (input, expected) in cases {
            assert_eq!(BlsPayload::decode(&input), Err(expected), "input {input:02x?}");
        }
    }

    #[test]
    fn rejects_non_canonical_points() {
        let registration = |pubkey: &[u8], pop: &[u8]| {
            [&[BlsConstants::KIND_REGISTRATION][..], pubkey, pop].concat()
        };
        let modulus_g1 = {
            let mut p = BlsConstants::FIELD_MODULUS;
            p[0] |= 0x80;
            p.to_vec()
        };
        let just_below_modulus_g1 = {
            let mut p = modulus_g1.clone();
            p[47] -= 1;
            p
        };
        let mut infinity_g1 = vec![0u8; 48];
        infinity_g1[0] = 0xc0;
        let mut uncompressed_g2 = point(96);
        uncompressed_g2[0] &= 0x7f;
        // Second G2 coordinate equal to the modulus.
        let g2_high_c0 = [&point(48)[..], &BlsConstants::FIELD_MODULUS].concat();

        assert_eq!(
            BlsPayload::decode(&registration(&uncompressed_g2, &point(48))),
            Err(BlsPayloadError::NonCanonicalPublicKey)
        );
        assert_eq!(
            BlsPayload::decode(&registration(&g2_high_c0, &point(48))),
            Err(BlsPayloadError::NonCanonicalPublicKey)
        );
        assert_eq!(
            BlsPayload::decode(&registration(&point(96), &infinity_g1)),
            Err(BlsPayloadError::NonCanonicalSignature)
        );
        assert_eq!(
            BlsPayload::decode(&registration(&point(96), &modulus_g1)),
            Err(BlsPayloadError::NonCanonicalSignature)
        );
        assert!(BlsPayload::decode(&registration(&point(96), &just_below_modulus_g1)).is_ok());

        let mut batch = batch_input(&[transfer(1)], 1);
        let len = batch.len();
        batch[len - 48..].copy_from_slice(&infinity_g1);
        assert_eq!(BlsPayload::decode(&batch), Err(BlsPayloadError::NonCanonicalSignature));
    }
}
