//! Rung-2: ephemeral throwaway signing + fully-offline verification. Byte-parity
//! port of the TS `scripts/arb-dryrun/rung2-ephemeral-signer.ts`.
//!
//! Every call generates ONE fresh, unfunded, in-memory k256 keypair, signs the
//! rung-1 unsigned envelope exactly once, and drops the key at function scope.
//! The signing capability never escapes: only the public address and the signed
//! bytes are returned. There is no external key input path — no file, env, argv,
//! keystore, mnemonic, or homedir loader exists in this module.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, B256, Signature, U256, b256, keccak256};
use k256::ecdsa::SigningKey;
use rand_08::rngs::OsRng;

use crate::assembler::dummy_signature;

/// secp256k1 `n/2`. A canonical transaction signature must have `s <= n/2`
/// (EIP-2 low-s). Matches the TS `SECP256K1_HALF_ORDER`.
const SECP256K1_HALF_ORDER: B256 =
    b256!("7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0");

/// The offline verification proof for an ephemeral signed envelope. Every field
/// is `true` only on success; any failure returns [`SignerError`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EphemeralSignatureVerification {
    /// The envelope is a well-formed EIP-1559 (`0x02`) transaction.
    pub valid_eip1559: bool,
    /// ecrecover returned the ephemeral signer address.
    pub recovered_signer: bool,
    /// All signed fields equal the unsigned input.
    pub fields_match: bool,
    /// The signature is NOT the rung-1 fixed invalid dummy.
    pub non_dummy_signature: bool,
    /// The signature uses canonical low-s.
    pub canonical_low_s: bool,
}

/// A rung-2 ephemeral signed envelope. Carries NO secret — only the public
/// address, the signed bytes, and the offline verification proof.
#[derive(Debug, Clone)]
pub struct EphemeralSignedTx {
    /// The ephemeral (unfunded, throwaway) signer address.
    pub signer_address: Address,
    /// The real-signed EIP-1559 raw backrun envelope.
    pub raw_backrun: Vec<u8>,
    /// The offline verification proof.
    pub verification: EphemeralSignatureVerification,
}

/// A rung-2 signing/verification failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignerError {
    /// The k256 prehash signing operation failed.
    Sign,
    /// The envelope is not a byte-aligned, fully-consumed EIP-1559 transaction.
    NotEip1559,
    /// The signature had a zero `r`/`s` or a non-binary parity.
    DegenerateSignature,
    /// The signature was the rung-1 fixed invalid dummy signature.
    DummySignature,
    /// The signature used non-canonical high-s.
    HighS,
    /// The signed transaction carried a non-empty access list.
    NonEmptyAccessList,
    /// A signed field did not match the unsigned input.
    FieldMismatch(&'static str),
    /// The signature was not cryptographically recoverable.
    Unrecoverable,
    /// The recovered signer did not match the ephemeral address.
    SignerMismatch,
}

impl core::fmt::Display for SignerError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Sign => write!(formatter, "ephemeral prehash signing failed"),
            Self::NotEip1559 => write!(formatter, "raw backrun must be a signed EIP-1559 envelope"),
            Self::DegenerateSignature => {
                write!(formatter, "raw backrun must contain a real r/s/yParity signature")
            }
            Self::DummySignature => {
                write!(
                    formatter,
                    "raw backrun must not use the rung-1 fixed invalid dummy signature"
                )
            }
            Self::HighS => write!(formatter, "raw backrun must use a canonical low-s signature"),
            Self::NonEmptyAccessList => {
                write!(formatter, "signed transaction accessList must be empty")
            }
            Self::FieldMismatch(name) => {
                write!(formatter, "signed transaction {name} does not match unsigned input")
            }
            Self::Unrecoverable => {
                write!(formatter, "raw backrun signature is not cryptographically recoverable")
            }
            Self::SignerMismatch => {
                write!(formatter, "recovered signer does not match expected signer address")
            }
        }
    }
}

impl core::error::Error for SignerError {}

/// Derive the Ethereum address of a k256 verifying key (keccak of the
/// uncompressed public key, low 20 bytes).
fn address_from_verifying_key(verifying_key: &k256::ecdsa::VerifyingKey) -> Address {
    let encoded = verifying_key.to_encoded_point(false);
    // Uncompressed SEC1 point: 0x04 || X(32) || Y(32); hash the 64-byte body.
    let hash = keccak256(&encoded.as_bytes()[1..]);
    Address::from_slice(&hash[12..])
}

/// Generate a fresh ephemeral keypair, hand ONLY its public address to `factory`
/// (which returns the unsigned envelope to sign — e.g. after deploying an
/// executor authorized to that address), sign once, and verify entirely offline.
/// The private key is generated in-memory, used once, and dropped here; only the
/// address ever leaves scope. Mirrors the TS `buildAndSignBlinkEphemeralAtomicTx`.
pub fn build_and_sign_ephemeral_atomic_tx<F>(factory: F) -> Result<EphemeralSignedTx, SignerError>
where
    F: FnOnce(Address) -> TxEip1559,
{
    let mut rng = OsRng;
    let signing_key = SigningKey::random(&mut rng);
    let signer_address = address_from_verifying_key(signing_key.verifying_key());
    let unsigned_tx = factory(signer_address);

    let signature_hash = unsigned_tx.signature_hash();
    let (signature, recovery_id) = signing_key
        .sign_prehash_recoverable(signature_hash.as_slice())
        .map_err(|_| SignerError::Sign)?;
    let signature_bytes = signature.to_bytes();
    let alloy_signature = Signature::new(
        U256::from_be_slice(&signature_bytes[..32]),
        U256::from_be_slice(&signature_bytes[32..]),
        recovery_id.is_y_odd(),
    );
    let raw_backrun = unsigned_tx.clone().into_signed(alloy_signature).encoded_2718();

    let verification = verify_ephemeral_signed_tx(&unsigned_tx, &raw_backrun, signer_address)?;
    Ok(EphemeralSignedTx { signer_address, raw_backrun, verification })
    // `signing_key` is dropped here and never leaves this scope.
}

/// Sign a supplied unsigned envelope once with a fresh ephemeral keypair and
/// verify it offline. Returns no secret. Mirrors the TS `signBlinkEphemeralAtomicTx`.
pub fn sign_ephemeral_atomic_tx(unsigned_tx: &TxEip1559) -> Result<EphemeralSignedTx, SignerError> {
    build_and_sign_ephemeral_atomic_tx(|_signer_address| unsigned_tx.clone())
}

/// Verify a signed envelope entirely offline: byte-well-formed EIP-1559, a real
/// non-dummy low-s signature, an empty access list, field integrity vs the
/// unsigned input, and ecrecover to `expected_signer`.
pub fn verify_ephemeral_signed_tx(
    unsigned_tx: &TxEip1559,
    raw_backrun: &[u8],
    expected_signer: Address,
) -> Result<EphemeralSignatureVerification, SignerError> {
    if raw_backrun.first() != Some(&0x02) {
        return Err(SignerError::NotEip1559);
    }
    let mut slice: &[u8] = raw_backrun;
    let envelope = TxEnvelope::decode_2718(&mut slice).map_err(|_| SignerError::NotEip1559)?;
    if !slice.is_empty() {
        return Err(SignerError::NotEip1559);
    }
    let TxEnvelope::Eip1559(signed) = envelope else {
        return Err(SignerError::NotEip1559);
    };

    let signature = *signed.signature();
    if signature.r().is_zero() || signature.s().is_zero() {
        return Err(SignerError::DegenerateSignature);
    }
    let dummy = dummy_signature();
    if signature.r() == dummy.r() && signature.s() == dummy.s() && signature.v() == dummy.v() {
        return Err(SignerError::DummySignature);
    }
    if signature.s() > U256::from_be_bytes(SECP256K1_HALF_ORDER.0) {
        return Err(SignerError::HighS);
    }

    let signed_tx = signed.tx();
    if !signed_tx.access_list.is_empty() {
        return Err(SignerError::NonEmptyAccessList);
    }
    check_field("chainId", signed_tx.chain_id, unsigned_tx.chain_id)?;
    check_field("nonce", signed_tx.nonce, unsigned_tx.nonce)?;
    check_field("gas", signed_tx.gas_limit, unsigned_tx.gas_limit)?;
    check_field("maxFeePerGas", signed_tx.max_fee_per_gas, unsigned_tx.max_fee_per_gas)?;
    check_field(
        "maxPriorityFeePerGas",
        signed_tx.max_priority_fee_per_gas,
        unsigned_tx.max_priority_fee_per_gas,
    )?;
    if signed_tx.to != unsigned_tx.to {
        return Err(SignerError::FieldMismatch("to"));
    }
    if signed_tx.value != unsigned_tx.value {
        return Err(SignerError::FieldMismatch("value"));
    }
    if signed_tx.input != unsigned_tx.input {
        return Err(SignerError::FieldMismatch("data"));
    }

    let recovered = signed.recover_signer().map_err(|_| SignerError::Unrecoverable)?;
    if recovered != expected_signer {
        return Err(SignerError::SignerMismatch);
    }

    Ok(EphemeralSignatureVerification {
        valid_eip1559: true,
        recovered_signer: true,
        fields_match: true,
        non_dummy_signature: true,
        canonical_low_s: true,
    })
}

fn check_field<T: PartialEq>(
    name: &'static str,
    actual: T,
    expected: T,
) -> Result<(), SignerError> {
    if actual == expected { Ok(()) } else { Err(SignerError::FieldMismatch(name)) }
}
