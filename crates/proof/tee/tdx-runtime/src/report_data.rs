use alloy_primitives::{B256, keccak256};

use crate::{Result, TdxRuntimeError};

/// Length of `TDREPORT.REPORTDATA`.
pub const TDX_REPORT_DATA_LEN: usize = 64;

/// Helper for constructing TDX `TDREPORT.REPORTDATA`.
#[derive(Debug)]
pub struct TdxReportData;

impl TdxReportData {
    /// Builds the report-data bytes expected by the TDX verifier.
    ///
    /// The first 32 bytes bind the signer public key. The last 32 bytes bind
    /// the app context, quote collection timestamp, and optional registrar
    /// nonce.
    pub fn for_public_key(
        public_key: &[u8],
        attestation_nonce: Option<B256>,
        quote_timestamp_millis: u64,
    ) -> Result<[u8; TDX_REPORT_DATA_LEN]> {
        if public_key.len() != 65 || public_key.first() != Some(&0x04) {
            return Err(TdxRuntimeError::InvalidPublicKey);
        }

        let mut report_data = [0u8; TDX_REPORT_DATA_LEN];
        report_data[..32].copy_from_slice(keccak256(&public_key[1..]).as_slice());
        let mut timestamp_binding =
            [&b"base-tdx-tee-prover-v1"[..], &quote_timestamp_millis.to_le_bytes()].concat();
        if let Some(nonce) = attestation_nonce {
            timestamp_binding.extend_from_slice(nonce.as_slice());
        }
        report_data[32..].copy_from_slice(keccak256(timestamp_binding).as_slice());
        Ok(report_data)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;

    use super::*;

    const TIMESTAMP_MILLIS: u64 = 1_711_111_111_000;

    #[test]
    fn report_data_binds_public_key_prefix_and_nonce_timestamp_suffix() {
        let public_key = [0x04; 65];
        let nonce = B256::repeat_byte(0x11);
        let report_data =
            TdxReportData::for_public_key(&public_key, Some(nonce), TIMESTAMP_MILLIS).unwrap();

        assert_eq!(&report_data[..32], keccak256(&public_key[1..]).as_slice());
        assert_eq!(
            &report_data[32..],
            keccak256(
                [&b"base-tdx-tee-prover-v1"[..], &TIMESTAMP_MILLIS.to_le_bytes(), nonce.as_slice()]
                    .concat()
            )
            .as_slice()
        );
    }

    #[test]
    fn report_data_without_nonce_keeps_legacy_timestamp_binding() {
        let public_key = [0x04; 65];
        let report_data =
            TdxReportData::for_public_key(&public_key, None, TIMESTAMP_MILLIS).unwrap();

        assert_eq!(&report_data[..32], keccak256(&public_key[1..]).as_slice());
        assert_eq!(
            &report_data[32..],
            keccak256([&b"base-tdx-tee-prover-v1"[..], &TIMESTAMP_MILLIS.to_le_bytes()].concat())
                .as_slice()
        );
    }

    #[test]
    fn report_data_rejects_malformed_public_keys() {
        assert!(matches!(
            TdxReportData::for_public_key(&[0u8; 64], None, TIMESTAMP_MILLIS),
            Err(TdxRuntimeError::InvalidPublicKey)
        ));
    }
}
