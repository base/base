//! Test fixtures for consumers of the SNARK receipt encoding.

/// Wire layout of an SP1 PLONK receipt after its proof-variant index:
/// `public_inputs`, `encoded_proof`, `raw_proof`, `plonk_vkey_hash`,
/// `public_values`, `sp1_version`, `tee_proof`.
type PlonkReceiptFields<'a> =
    ([String; 5], &'a str, &'a str, [u8; 32], Vec<u8>, &'a str, Option<Vec<u8>>);

/// Builds prover-service SP1 PLONK receipt fixtures for tests.
///
/// Consumers of [`crate::SnarkReceiptEncoder`] can use this instead of
/// depending on `sp1-sdk` and `bincode` directly to fabricate receipt bytes.
/// The bytes are encoded from the receipt's wire layout, so this fixture does
/// not link the SP1 prover stack; this crate's tests check them against the
/// pinned `sp1-sdk` encoding.
#[derive(Debug, Clone, Copy)]
pub struct SnarkReceiptFixture;

impl SnarkReceiptFixture {
    /// Returns bincode-encoded receipt bytes carrying a PLONK proof with the
    /// given verifying-key prefix and hex-encoded proof, matching the
    /// payloads produced by the `base-proof-zk-backend` provers.
    pub fn plonk_receipt_bytes(vkey_prefix: [u8; 4], encoded_proof: &str) -> Vec<u8> {
        let mut plonk_vkey_hash = [0u8; 32];
        plonk_vkey_hash[..4].copy_from_slice(&vkey_prefix);
        let config = bincode::config::standard();
        // `SP1Proof::Plonk` variant index, then the remaining fields.
        let mut bytes =
            bincode::serde::encode_to_vec(2u32, config).expect("receipt variant must encode");
        let fields: PlonkReceiptFields<'_> =
            (Default::default(), encoded_proof, "", plonk_vkey_hash, Vec::new(), "test", None);
        bytes.extend(bincode::serde::encode_to_vec(fields, config).expect("receipt must encode"));
        bytes
    }
}
