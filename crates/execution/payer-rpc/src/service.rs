//! Server implementation of the `payer_*` JSON-RPC surface.

use alloy_consensus::transaction::Recovered;
use alloy_eips::{Decodable2718, eip2718::Encodable2718};
use alloy_primitives::{B256, Bytes};
use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope};
use base_execution_payer::{PayerCosigner, PayerDigestSigner};
use base_execution_txpool::BasePooledTransaction;
use jsonrpsee::core::RpcResult;
use reth_transaction_pool::{AddedTransactionOutcome, TransactionOrigin, TransactionPool};
use tracing::debug;

use crate::{
    api::{PayerApiServer, PayerTermsResponse},
    error::PayerRpcError,
    terms::PayerTerms,
};

/// Server for the `payer_*` methods, backed by a transaction pool, a per-block
/// [`PayerTerms`] resolver, and the builder's payer co-signer.
#[derive(Debug)]
pub struct PayerApiImpl<Pool, Terms, Signer> {
    pool: Pool,
    terms: Terms,
    cosigner: PayerCosigner<Signer>,
}

impl<Pool, Terms, Signer> PayerApiImpl<Pool, Terms, Signer> {
    /// Creates a handler from a pool, a terms resolver, and a payer co-signer.
    pub const fn new(pool: Pool, terms: Terms, cosigner: PayerCosigner<Signer>) -> Self {
        Self { pool, terms, cosigner }
    }
}

impl<Pool, Terms, Signer> PayerApiImpl<Pool, Terms, Signer>
where
    Terms: PayerTerms,
    Signer: PayerDigestSigner,
{
    /// Decodes, validates and co-signs a partially-signed EIP-8130 transaction,
    /// returning the fully-authorized transaction recovered against its sender.
    ///
    /// The co-signature is produced only once the service is confirmed live,
    /// configured for this builder's payer, and quoting at least one token; the
    /// authoritative payment check remains the phase-0 transfer at inclusion.
    fn cosign(&self, raw: &Bytes) -> Result<Recovered<BaseTransactionSigned>, PayerRpcError> {
        let envelope = BaseTxEnvelope::decode_2718(&mut raw.as_ref())
            .map_err(|e| PayerRpcError::Decode(e.to_string()))?;
        let signed = envelope.as_eip8130().ok_or(PayerRpcError::NotEip8130)?;

        let payer_account = self.cosigner.address();
        if signed.tx().payer != Some(payer_account) {
            return Err(PayerRpcError::PayerMismatch {
                found: signed.tx().payer,
                expected: payer_account,
            });
        }
        if !signed.payer_auth().is_empty() {
            return Err(PayerRpcError::AlreadyCosigned);
        }

        let snapshot = self.terms.price_snapshot()?;
        if !snapshot.enabled {
            return Err(PayerRpcError::Disabled);
        }
        if snapshot.payer != payer_account {
            return Err(PayerRpcError::PayerNotConfigured {
                configured: snapshot.payer,
                actual: payer_account,
            });
        }
        if snapshot.prices.is_empty() {
            return Err(PayerRpcError::NoQuotableTokens);
        }

        let sender = signed.recover_sender().map_err(|_| PayerRpcError::SenderRecovery)?;
        let cosigned =
            self.cosigner.cosign(signed.tx().clone(), signed.sender_auth().clone(), sender)?;
        Ok(Recovered::new_unchecked(BaseTxEnvelope::Eip8130(cosigned), sender))
    }
}

#[async_trait::async_trait]
impl<Pool, Terms, Signer> PayerApiServer for PayerApiImpl<Pool, Terms, Signer>
where
    Pool: TransactionPool<Transaction = BasePooledTransaction> + 'static,
    Terms: PayerTerms + 'static,
    Signer: PayerDigestSigner + Send + Sync + 'static,
{
    async fn get_terms(&self) -> RpcResult<PayerTermsResponse> {
        let snapshot = self.terms.price_snapshot().map_err(PayerRpcError::from)?;
        Ok(snapshot.into())
    }

    async fn send_transaction(&self, transaction: Bytes) -> RpcResult<B256> {
        let recovered = self.cosign(&transaction)?;
        let encoded_len = recovered.encode_2718_len();
        let pool_tx = BasePooledTransaction::new(recovered, encoded_len);

        // Private origin runs the full validator guards but keeps the co-signed
        // transaction off p2p, so the payer's gas can only be spent by this
        // builder.
        let AddedTransactionOutcome { hash, .. } = self
            .pool
            .add_transaction(TransactionOrigin::Private, pool_tx)
            .await
            .map_err(|e| PayerRpcError::Pool(e.to_string()))?;

        debug!(tx_hash = %hash, "co-signed and inserted sponsored transaction");
        Ok(hash)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256, address};
    use base_common_consensus::{Eip8130Signed, TxEip8130};
    use base_execution_payer::{LocalPayerSigner, PriceSnapshot, Rate, TokenPrice};
    use reth_transaction_pool::noop::NoopTransactionPool;

    use super::*;
    use crate::error::PayerTermsError;

    const SENDER: Address = address!("0x00000000000000000000000000000000000000dd");
    const TOKEN: Address = address!("0x0000000000000000000000000000000000000011");
    const FEE_RECIPIENT: Address = address!("0x0000000000000000000000000000000000000022");

    /// A [`PayerTerms`] that always yields the same snapshot.
    struct FixedTerms(PriceSnapshot);

    impl PayerTerms for FixedTerms {
        fn price_snapshot(&self) -> Result<PriceSnapshot, PayerTermsError> {
            Ok(self.0.clone())
        }
    }

    fn signer() -> LocalPayerSigner {
        LocalPayerSigner::from_bytes(&[0x11; 32]).unwrap()
    }

    fn snapshot(payer: Address, enabled: bool, with_token: bool) -> PriceSnapshot {
        let prices = if with_token {
            vec![TokenPrice {
                token: TOKEN,
                fee_recipient: FEE_RECIPIENT,
                rate: Rate::new(U256::from(1u64), U256::from(400_000_000u64)),
                margin_bps: 100,
            }]
        } else {
            vec![]
        };
        PriceSnapshot { payer, enabled, prices }
    }

    /// A partially-signed EIP-8130 transaction (explicit sender, payer set to
    /// `payer`, empty `payer_auth`), EIP-2718 encoded.
    fn partial_tx(payer: Option<Address>) -> Bytes {
        let tx = TxEip8130 { sender: Some(SENDER), payer, ..Default::default() };
        // Configured-actor `sender_auth`: authenticator(20) || data; the exact
        // payload is irrelevant here because the explicit sender short-circuits
        // recovery.
        let sender_auth = Bytes::from(vec![0u8; 21]);
        let signed = Eip8130Signed::new(tx, sender_auth, Bytes::new());
        let mut buf = Vec::new();
        BaseTxEnvelope::Eip8130(signed).encode_2718(&mut buf);
        Bytes::from(buf)
    }

    fn handler(
        snapshot: PriceSnapshot,
    ) -> PayerApiImpl<NoopTransactionPool<BasePooledTransaction>, FixedTerms, LocalPayerSigner> {
        PayerApiImpl::new(
            NoopTransactionPool::<BasePooledTransaction>::new(),
            FixedTerms(snapshot),
            PayerCosigner::new(signer()),
        )
    }

    #[test]
    fn cosign_attaches_payer_auth_bound_to_sender() {
        let payer = signer().address();
        let handler = handler(snapshot(payer, true, true));
        let recovered = handler.cosign(&partial_tx(Some(payer))).unwrap();

        assert_eq!(recovered.signer(), SENDER);
        let cosigned = recovered.as_eip8130().unwrap();
        // K1_AUTHENTICATOR(20) || r(32) || s(32) || v(1).
        assert_eq!(cosigned.payer_auth().len(), 85);
        assert_eq!(cosigned.tx().payer, Some(payer));
    }

    #[test]
    fn rejects_non_eip8130() {
        let handler = handler(snapshot(signer().address(), true, true));
        // A bare legacy type byte is not a decodable EIP-8130 transaction.
        let err = handler.cosign(&Bytes::from_static(&[0x02, 0x01])).unwrap_err();
        assert!(matches!(err, PayerRpcError::Decode(_)));
    }

    #[test]
    fn rejects_payer_mismatch() {
        let handler = handler(snapshot(signer().address(), true, true));
        let other = address!("0x00000000000000000000000000000000000000ee");
        let err = handler.cosign(&partial_tx(Some(other))).unwrap_err();
        assert!(matches!(err, PayerRpcError::PayerMismatch { .. }));
    }

    #[test]
    fn rejects_missing_payer() {
        let handler = handler(snapshot(signer().address(), true, true));
        let err = handler.cosign(&partial_tx(None)).unwrap_err();
        assert!(matches!(err, PayerRpcError::PayerMismatch { found: None, .. }));
    }

    #[test]
    fn rejects_when_disabled() {
        let payer = signer().address();
        let handler = handler(snapshot(payer, false, true));
        let err = handler.cosign(&partial_tx(Some(payer))).unwrap_err();
        assert!(matches!(err, PayerRpcError::Disabled));
    }

    #[test]
    fn rejects_when_payer_not_configured() {
        let payer = signer().address();
        let other = address!("0x00000000000000000000000000000000000000ee");
        let handler = handler(snapshot(other, true, true));
        let err = handler.cosign(&partial_tx(Some(payer))).unwrap_err();
        assert!(matches!(err, PayerRpcError::PayerNotConfigured { .. }));
    }

    #[test]
    fn rejects_when_no_quotable_tokens() {
        let payer = signer().address();
        let handler = handler(snapshot(payer, true, false));
        let err = handler.cosign(&partial_tx(Some(payer))).unwrap_err();
        assert!(matches!(err, PayerRpcError::NoQuotableTokens));
    }

    #[tokio::test]
    async fn get_terms_maps_snapshot() {
        let payer = signer().address();
        let handler = handler(snapshot(payer, true, true));
        let terms = handler.get_terms().await.unwrap();

        assert_eq!(terms.payer, payer);
        assert!(terms.enabled);
        assert_eq!(terms.tokens.len(), 1);
        assert_eq!(terms.tokens[0].token, TOKEN);
        assert_eq!(terms.tokens[0].fee_recipient, FEE_RECIPIENT);
        assert_eq!(terms.tokens[0].margin_bps, 100);
        assert_eq!(terms.tokens[0].rate.denominator, U256::from(400_000_000u64));
    }
}
