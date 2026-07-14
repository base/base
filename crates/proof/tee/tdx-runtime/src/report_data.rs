use alloy_primitives::{Address, B256, keccak256};

use crate::{Result, TdxRuntimeError};

/// Length of `TDREPORT.REPORTDATA`.
pub const TDX_REPORT_DATA_LEN: usize = 64;

/// Chain-specific context bound into a signer registration quote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TdxAttestationContext {
    /// L1 chain ID for the signer registry.
    pub chain_id: u64,
    /// `TEEProverRegistry` address receiving the registration.
    pub registry_address: Address,
}

/// Helper for constructing TDX `TDREPORT.REPORTDATA`.
#[derive(Debug)]
pub struct TdxReportData;

impl TdxReportData {
    /// Builds the report-data bytes expected by the TDX verifier.
    ///
    /// The first 32 bytes bind the signer public key. The last 32 bytes bind
    /// the CI workload digest, registrar nonce, quote timestamp, chain, and
    /// registry address.
    pub fn for_public_key(
        public_key: &[u8],
        workload_digest: B256,
        attestation_nonce: Option<B256>,
        quote_timestamp_millis: u64,
        context: TdxAttestationContext,
    ) -> Result<[u8; TDX_REPORT_DATA_LEN]> {
        if public_key.len() != 65 || public_key.first() != Some(&0x04) {
            return Err(TdxRuntimeError::InvalidPublicKey);
        }

        let mut report_data = [0u8; TDX_REPORT_DATA_LEN];
        report_data[..32].copy_from_slice(keccak256(&public_key[1..]).as_slice());
        let mut timestamp_binding =
            [&b"base-tdx-workload-v2"[..], workload_digest.as_slice()].concat();
        if let Some(nonce) = attestation_nonce {
            timestamp_binding.extend_from_slice(nonce.as_slice());
        }
        timestamp_binding.extend_from_slice(&quote_timestamp_millis.to_le_bytes());
        timestamp_binding.extend_from_slice(&context.chain_id.to_le_bytes());
        timestamp_binding.extend_from_slice(context.registry_address.as_slice());
        report_data[32..].copy_from_slice(keccak256(timestamp_binding).as_slice());
        Ok(report_data)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;

    use super::*;

    const TIMESTAMP_MILLIS: u64 = 1_711_111_111_000;
    const WORKLOAD_DIGEST: B256 = B256::repeat_byte(0x22);
    const CONTEXT: TdxAttestationContext = TdxAttestationContext {
        chain_id: 11_155_111,
        registry_address: Address::repeat_byte(0x33),
    };

    #[test]
    fn report_data_binds_public_key_and_registration_context() {
        let public_key = [0x04; 65];
        let nonce = B256::repeat_byte(0x11);
        let report_data = TdxReportData::for_public_key(
            &public_key,
            WORKLOAD_DIGEST,
            Some(nonce),
            TIMESTAMP_MILLIS,
            CONTEXT,
        )
        .unwrap();

        assert_eq!(&report_data[..32], keccak256(&public_key[1..]).as_slice());
        assert_eq!(
            &report_data[32..],
            keccak256(
                [
                    &b"base-tdx-workload-v2"[..],
                    WORKLOAD_DIGEST.as_slice(),
                    nonce.as_slice(),
                    &TIMESTAMP_MILLIS.to_le_bytes(),
                    &CONTEXT.chain_id.to_le_bytes(),
                    CONTEXT.registry_address.as_slice(),
                ]
                .concat()
            )
            .as_slice()
        );
    }

    #[test]
    fn report_data_without_nonce_binds_workload_context() {
        let public_key = [0x04; 65];
        let report_data = TdxReportData::for_public_key(
            &public_key,
            WORKLOAD_DIGEST,
            None,
            TIMESTAMP_MILLIS,
            CONTEXT,
        )
        .unwrap();

        assert_eq!(&report_data[..32], keccak256(&public_key[1..]).as_slice());
        assert_eq!(
            &report_data[32..],
            keccak256(
                [
                    &b"base-tdx-workload-v2"[..],
                    WORKLOAD_DIGEST.as_slice(),
                    &TIMESTAMP_MILLIS.to_le_bytes(),
                    &CONTEXT.chain_id.to_le_bytes(),
                    CONTEXT.registry_address.as_slice(),
                ]
                .concat()
            )
            .as_slice()
        );
    }

    #[test]
    fn report_data_rejects_malformed_public_keys() {
        assert!(matches!(
            TdxReportData::for_public_key(
                &[0u8; 64],
                WORKLOAD_DIGEST,
                None,
                TIMESTAMP_MILLIS,
                CONTEXT
            ),
            Err(TdxRuntimeError::InvalidPublicKey)
        ));
    }
}
