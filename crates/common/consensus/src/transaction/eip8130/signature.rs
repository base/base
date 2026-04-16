//! EIP-8130 signature hash computation and auth parsing.

use alloc::vec::Vec;

use alloy_primitives::{Address, B256, Bytes, keccak256};

use super::{TxEip8130, types::ConfigChangeEntry};

/// Computes the EIP-712 config change authorization digest.
///
/// Matches the JS reference in `send-aa-tx.mjs::configChangeDigest()`.
/// The authorizer (an owner with CONFIG scope) signs this digest to
/// authorize the owner changes in a [`ConfigChangeEntry`].
pub fn config_change_digest(account: Address, change: &ConfigChangeEntry) -> B256 {
    let typehash = keccak256(
        "SignedOwnerChanges(address account,uint64 chainId,uint64 sequence,\
         OwnerChange[] ownerChanges)\
         OwnerChange(uint8 changeType,address verifier,bytes32 ownerId,uint8 scope)",
    );

    let mut change_hashes = Vec::with_capacity(change.owner_changes.len() * 32);
    for oc in &change.owner_changes {
        let mut buf = [0u8; 128]; // 4 * 32 bytes
        buf[31] = oc.change_type;
        buf[44..64].copy_from_slice(oc.verifier.as_slice());
        buf[64..96].copy_from_slice(oc.owner_id.as_slice());
        buf[127] = oc.scope;
        change_hashes.extend_from_slice(keccak256(buf).as_slice());
    }
    let owner_changes_hash = keccak256(&change_hashes);

    let mut buf = [0u8; 160]; // 5 * 32 bytes
    buf[0..32].copy_from_slice(typehash.as_slice());
    buf[44..64].copy_from_slice(account.as_slice());
    buf[88..96].copy_from_slice(&change.chain_id.to_be_bytes());
    buf[120..128].copy_from_slice(&change.sequence.to_be_bytes());
    buf[128..160].copy_from_slice(owner_changes_hash.as_slice());
    keccak256(buf)
}

/// Computed sender signature hash.
pub fn sender_signature_hash(tx: &TxEip8130) -> B256 {
    let mut buf = Vec::with_capacity(512);
    tx.encode_for_sender_signing(&mut buf);
    keccak256(&buf)
}

/// Computed payer signature hash.
pub fn payer_signature_hash(tx: &TxEip8130) -> B256 {
    let mut buf = Vec::with_capacity(512);
    tx.encode_for_payer_signing(&mut buf);
    keccak256(&buf)
}

/// Parsed sender authentication data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedSenderAuth {
    /// EOA mode: raw 65-byte ECDSA signature `(r || s || v)`.
    Eoa {
        /// The raw 65-byte ECDSA signature.
        signature: [u8; 65],
    },
    /// Configured owner mode: verifier address + verifier-specific data.
    Configured {
        /// The verifier address (first 20 bytes of `sender_auth`).
        verifier: Address,
        /// The verifier-specific authentication data after the address prefix.
        data: Bytes,
    },
}

/// Parse `sender_auth` based on the transaction's `from` field.
///
/// - If `from == Address::ZERO` (EOA mode): expect exactly 65 bytes (raw ECDSA).
/// - Otherwise (configured owner): first 20 bytes are the verifier address.
pub fn parse_sender_auth(tx: &TxEip8130) -> Result<ParsedSenderAuth, &'static str> {
    if tx.is_eoa() {
        if tx.sender_auth.len() != 65 {
            return Err("EOA sender_auth must be exactly 65 bytes");
        }
        let mut sig = [0u8; 65];
        sig.copy_from_slice(&tx.sender_auth);
        return Ok(ParsedSenderAuth::Eoa { signature: sig });
    }

    if tx.sender_auth.len() < 20 {
        return Err("configured sender_auth must contain at least a 20-byte verifier address");
    }

    let verifier = Address::from_slice(&tx.sender_auth[..20]);
    let data = Bytes::copy_from_slice(&tx.sender_auth[20..]);
    Ok(ParsedSenderAuth::Configured { verifier, data })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TxEip8130;

    #[test]
    fn parse_eoa_auth() {
        let tx = TxEip8130 {
            from: None,
            sender_auth: Bytes::from([0xABu8; 65].as_slice()),
            ..Default::default()
        };
        let parsed = parse_sender_auth(&tx).unwrap();
        assert!(matches!(parsed, ParsedSenderAuth::Eoa { .. }));
    }

    #[test]
    fn parse_eoa_wrong_length() {
        let tx = TxEip8130 {
            from: None,
            sender_auth: Bytes::from_static(&[0x01; 64]),
            ..Default::default()
        };
        assert!(parse_sender_auth(&tx).is_err());
    }

    #[test]
    fn parse_configured_k1() {
        use crate::K1_VERIFIER_ADDRESS;
        let mut auth = Vec::new();
        auth.extend_from_slice(K1_VERIFIER_ADDRESS.as_slice());
        auth.extend_from_slice(&[0xAB; 65]);
        let tx = TxEip8130 {
            from: Some(Address::repeat_byte(0x01)),
            sender_auth: Bytes::from(auth),
            ..Default::default()
        };
        let parsed = parse_sender_auth(&tx).unwrap();
        match parsed {
            ParsedSenderAuth::Configured { verifier, data } => {
                assert_eq!(verifier, K1_VERIFIER_ADDRESS);
                assert_eq!(data.len(), 65);
            }
            _ => panic!("expected Configured"),
        }
    }

    #[test]
    fn parse_configured_custom() {
        let custom_verifier = Address::repeat_byte(0xCC);
        let mut auth = Vec::new();
        auth.extend_from_slice(custom_verifier.as_slice());
        auth.extend_from_slice(&[0xDD; 32]);
        let tx = TxEip8130 {
            from: Some(Address::repeat_byte(0x01)),
            sender_auth: Bytes::from(auth),
            ..Default::default()
        };
        let parsed = parse_sender_auth(&tx).unwrap();
        match parsed {
            ParsedSenderAuth::Configured { verifier, data } => {
                assert_eq!(verifier, custom_verifier);
                assert_eq!(data.len(), 32);
            }
            _ => panic!("expected Configured"),
        }
    }

    #[test]
    fn sender_payer_hashes_are_deterministic() {
        let tx = TxEip8130 {
            chain_id: 1,
            from: Some(Address::repeat_byte(0x01)),
            nonce_key: alloy_primitives::U256::ZERO,
            nonce_sequence: 1,
            ..Default::default()
        };
        let h1 = sender_signature_hash(&tx);
        let h2 = sender_signature_hash(&tx);
        assert_eq!(h1, h2);

        let p1 = payer_signature_hash(&tx);
        let p2 = payer_signature_hash(&tx);
        assert_eq!(p1, p2);

        assert_ne!(h1, p1, "sender and payer hashes must differ");
    }
}
