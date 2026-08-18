//! Parsed bundle type with decoded transactions.

use alloy_consensus::transaction::{Recovered, SignerRecoverable};
use alloy_provider::network::eip2718::Decodable2718;
use base_common_consensus::BaseTxEnvelope;

use crate::Bundle;

/// `ParsedBundle` is the type that contains utility methods for the `Bundle` type.
///
/// Unlike [`Bundle`], this type has decoded transactions with recovered signers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBundle {
    /// Decoded and recovered transactions.
    pub txs: Vec<Recovered<BaseTxEnvelope>>,
}

impl TryFrom<Bundle> for ParsedBundle {
    type Error = String;

    fn try_from(bundle: Bundle) -> Result<Self, Self::Error> {
        let txs: Vec<Recovered<BaseTxEnvelope>> = bundle
            .txs
            .into_iter()
            .map(|tx| {
                BaseTxEnvelope::decode_2718_exact(tx.iter().as_slice())
                    .map_err(|e| format!("Failed to decode transaction: {e:?}"))
                    .and_then(|tx| {
                        tx.try_into_recovered().map_err(|e| {
                            format!("Failed to convert transaction to recovered: {e:?}")
                        })
                    })
            })
            .collect::<Result<Vec<Recovered<BaseTxEnvelope>>, String>>()?;

        Ok(Self { txs })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;
    use alloy_provider::network::eip2718::Encodable2718;
    use alloy_signer_local::PrivateKeySigner;

    use super::*;
    use crate::test_utils::create_transaction;

    #[test]
    fn test_parsed_bundle_from_bundle() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx = create_transaction(alice, 1, bob.address(), U256::from(10_000));
        let tx_bytes = tx.encoded_2718();

        let bundle = Bundle { txs: vec![tx_bytes.into()] };

        let parsed: ParsedBundle = bundle.try_into().unwrap();
        assert_eq!(parsed.txs.len(), 1);
    }

    #[test]
    fn test_parsed_bundle_invalid_tx() {
        let bundle = Bundle { txs: vec![vec![0x00, 0x01, 0x02].into()] };

        let result: Result<ParsedBundle, _> = bundle.try_into();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to decode transaction"));
    }
}
