//! ERC-8168 token payer that co-signs at ingress.

use alloy_consensus::BlockHeader;
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, U64, U128, U256};
use alloy_signer::Signer;
use base_common_consensus::{Eip8130Constants, Eip8130Signed};
use base_execution_txpool::{ValidityOperator, ValidityPredicate};
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorCode, ErrorObjectOwned},
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_revm::database::StateProviderDatabase;
use reth_storage_api::{BlockReaderIdExt, StateProviderFactory};
use revm::{Database, context::BlockEnv};
use tracing::{info, warn};

use crate::{
    GasDiagnostic, GasEstimate, GetTermsParams, GetTermsResult, OfferConditions, PayerConfigError,
    PayerErrorCode, PayerRejection, PayerTerms, PayerToken, PaymentOption, Requote, Revert,
    SendTransactionResult, Shortfall, TokenCharged, TokenChoice, TokenPayment, TokenPaymentOffer,
    TransferOutcome, ValidityIngress,
};

/// Payer that accepts ERC-20 tokens for gas on EIP-8130 transactions.
///
/// It co-signs a sender-signed transaction once the phase-0 payment covers
/// the gas at the current rate and the transfer succeeds when simulated, then
/// admits it as a validity transaction that stays includable only while the
/// sender still holds the payment.
#[derive(Debug)]
pub struct PayerService<Client, Ingress, S> {
    terms: PayerTerms,
    tokens: Vec<PayerToken>,
    client: Client,
    ingress: Ingress,
    signer: S,
}

impl<Client, Ingress, S> PayerService<Client, Ingress, S>
where
    Client: StateProviderFactory + BlockReaderIdExt + ChainSpecProvider,
    Ingress: ValidityIngress,
    S: Signer + Send + Sync,
{
    /// Multiplier on the latest base fee quoted as `maxFeePerGas`.
    pub const BASE_FEE_HEADROOM: u128 = 2;

    /// Creates a payer; `signer` must hold the payer account's own key.
    pub fn new(
        terms: PayerTerms,
        tokens: Vec<PayerToken>,
        client: Client,
        ingress: Ingress,
        signer: S,
    ) -> Result<Self, PayerConfigError> {
        if signer.address() != terms.payer {
            return Err(PayerConfigError::SignerMismatch {
                payer: terms.payer,
                signer: signer.address(),
            });
        }
        Ok(Self { terms, tokens, client, ingress, signer })
    }

    /// Quotes every token whose price can be read at the latest state.
    pub fn terms(&self, params: &GetTermsParams) -> RpcResult<GetTermsResult> {
        let chain_id = self.client.chain_spec().chain_id();
        if params.chain_id != U64::from(chain_id) {
            return Err(ErrorObjectOwned::owned(
                ErrorCode::InvalidParams.code(),
                format!("payer serves chain {chain_id}"),
                None::<()>,
            ));
        }
        let header = self.client.latest_header().map_err(Self::unavailable)?.ok_or_else(|| {
            PayerRejection::new(PayerErrorCode::TemporarilyUnavailable, "no canonical head")
        })?;
        let max_fee_per_gas =
            u128::from(header.base_fee_per_gas().unwrap_or_default()) * Self::BASE_FEE_HEADROOM;
        let payment_gas = self.tokens.iter().map(|token| token.payment_gas).max().unwrap_or(0);
        let gas_limit = params
            .gas_limit
            .map_or(self.terms.default_gas_limit, |gas| gas.to::<u64>())
            .saturating_add(payment_gas);

        let state = self.client.latest().map_err(Self::unavailable)?;
        let mut db = StateProviderDatabase::new(&state);
        let mut tokens = Vec::with_capacity(self.tokens.len());
        for token in &self.tokens {
            let rate = match token.quote(&mut db) {
                Ok(rate) => rate,
                Err(error) => {
                    warn!(token = %token.symbol, error = %error, "skipping unpriceable token");
                    continue;
                }
            };
            let Some(payment_amount) = rate.required_amount(gas_limit, max_fee_per_gas) else {
                continue;
            };
            tokens.push(TokenChoice {
                token: token.address,
                symbol: token.symbol.clone(),
                decimals: token.decimals,
                rate: rate.0,
                payment_amount,
                payment_gas: U64::from(token.payment_gas),
            });
        }
        if tokens.is_empty() && !self.tokens.is_empty() {
            return Err(PayerRejection::new(
                PayerErrorCode::TemporarilyUnavailable,
                "no token price is readable",
            )
            .into());
        }
        tokens.sort_by_key(|choice| {
            params
                .preferred_tokens
                .iter()
                .position(|preferred| *preferred == choice.token)
                .unwrap_or(usize::MAX)
        });

        let cost = U256::from(gas_limit) * U256::from(max_fee_per_gas);
        let within_conditions = self.terms.max_gas_limit.is_none_or(|max| gas_limit <= max)
            && self.terms.max_cost_wei.is_none_or(|max| cost <= max);
        let options = if tokens.is_empty() {
            Vec::new()
        } else {
            vec![PaymentOption::Token(TokenPaymentOffer {
                payer: self.terms.payer,
                ttl: self.terms.quote_ttl_secs,
                conditions: OfferConditions {
                    max_expiry: self.terms.max_expiry_secs,
                    max_gas_limit: self.terms.max_gas_limit.map(U64::from),
                    max_cost: self.terms.max_cost_wei,
                },
                tokens,
            })]
        };
        Ok(GetTermsResult {
            gas_estimate: within_conditions.then(|| GasEstimate {
                gas_limit: U64::from(gas_limit),
                max_fee_per_gas: U128::from(max_fee_per_gas),
                max_priority_fee_per_gas: U128::ZERO,
            }),
            options,
        })
    }

    /// Co-signs `raw` and admits it to the pool, given the current time in
    /// milliseconds since the Unix epoch.
    pub async fn sponsor(&self, raw: &[u8], now_ms: u64) -> RpcResult<SendTransactionResult> {
        let signed = Eip8130Signed::decode_2718_exact(raw).map_err(|_| {
            PayerRejection::new(PayerErrorCode::InvalidTransaction, "not an EIP-8130 transaction")
        })?;
        let (token, sender, payment) = self.verify(&signed, now_ms)?;

        let tx = signed.tx();
        let signature = self
            .signer
            .sign_hash(&tx.payer_signature_hash(sender))
            .await
            .map_err(Self::internal)?;
        let payer_auth =
            [Eip8130Constants::K1_AUTHENTICATOR.as_slice(), &signature.as_bytes()].concat();
        let co_signed =
            Eip8130Signed::new(tx.clone(), signed.sender_auth().clone(), payer_auth.into());

        let bound = self.ingress.latest_block_expiry_bound()?.ok_or_else(|| {
            PayerRejection::new(PayerErrorCode::TemporarilyUnavailable, "no canonical head")
        })?;
        let validity = vec![
            token.balance.predicate(token.address, sender, payment.amount),
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(bound),
            },
        ];
        let transaction_hash =
            self.ingress.submit(co_signed.encoded_2718().into(), validity).await?;
        info!(tx_hash = %transaction_hash, token = %token.symbol, "co-signed token payment");
        Ok(SendTransactionResult {
            transaction_hash,
            token_charged: TokenCharged { token: token.address, amount: payment.amount },
        })
    }

    /// Checks `signed` against the payer's terms and the latest state, and
    /// returns the payment token, the resolved sender, and the payment.
    fn verify(
        &self,
        signed: &Eip8130Signed,
        now_ms: u64,
    ) -> RpcResult<(&PayerToken, Address, TokenPayment)> {
        let tx = signed.tx();
        if tx.chain_id != self.client.chain_spec().chain_id() {
            return Err(Self::invalid("wrong chain id"));
        }
        if tx.payer != Some(self.terms.payer) {
            return Err(Self::invalid("transaction must name this payer"));
        }
        if !signed.payer_auth().is_empty() {
            return Err(Self::invalid("payer_auth must be empty"));
        }
        let sender =
            signed.recover_sender().map_err(|_| Self::invalid("sender does not recover"))?;
        self.check_expiry(tx.valid_before_ms(), now_ms)?;
        self.check_gas(tx.gas_limit, tx.max_fee_per_gas)?;

        let payment = TokenPayment::from_phases(&tx.calls)?;
        let Some(token) = self.tokens.iter().find(|token| token.address == payment.token) else {
            return Err(PayerRejection::new(
                PayerErrorCode::UnsupportedToken,
                "token is not accepted",
            )
            .into());
        };
        if payment.recipient != self.terms.payer {
            return Err(Self::invalid("phase 0 must pay the payer"));
        }

        let state = self.client.latest().map_err(Self::unavailable)?;
        let mut db = StateProviderDatabase::new(&state);
        let rate = token.quote(&mut db).map_err(|error| {
            warn!(token = %token.symbol, error = %error, "failed to price payment token");
            PayerRejection::new(PayerErrorCode::TemporarilyUnavailable, "token price unavailable")
        })?;
        let required = rate
            .required_amount(tx.gas_limit, tx.max_fee_per_gas)
            .ok_or_else(|| Self::invalid("payment amount overflows"))?;
        if payment.amount < required {
            return Err(PayerRejection {
                requote: Some(Box::new(Requote {
                    token: token.address,
                    payment_amount: required,
                    rate: rate.0,
                    ttl: self.terms.quote_ttl_secs,
                })),
                ..PayerRejection::new(
                    PayerErrorCode::PaymentInsufficient,
                    "phase-0 credit is below the required amount",
                )
            }
            .into());
        }
        let word =
            db.storage(token.address, token.balance.slot(sender)).map_err(Self::unavailable)?;
        let available = token.balance.balance(word);
        if available < payment.amount {
            return Err(PayerRejection {
                shortfall: Some(Box::new(Shortfall {
                    token: token.address,
                    required: payment.amount,
                    available,
                })),
                ..PayerRejection::new(
                    PayerErrorCode::SenderBalanceInsufficient,
                    "sender balance is below the phase-0 credit",
                )
            }
            .into());
        }

        let header = self.client.latest_header().map_err(Self::unavailable)?.ok_or_else(|| {
            PayerRejection::new(PayerErrorCode::TemporarilyUnavailable, "no canonical head")
        })?;
        let block = BlockEnv {
            number: U256::from(header.number()),
            timestamp: U256::from(header.timestamp()),
            ..Default::default()
        };
        let data = match payment
            .simulate(&mut db, sender, block, tx.chain_id)
            .map_err(Self::unavailable)?
        {
            TransferOutcome::Transferred => return Ok((token, sender, payment)),
            TransferOutcome::Reverted(data) => Some(data),
            TransferOutcome::Failed => None,
        };
        Err(PayerRejection {
            revert: Some(Box::new(Revert { phase: 0, data })),
            ..PayerRejection::new(PayerErrorCode::ExecutionReverted, "phase-0 transfer fails")
        }
        .into())
    }

    /// Rejects a transaction that could stay includable beyond `maxExpiry`.
    fn check_expiry(&self, valid_before_ms: u64, now_ms: u64) -> Result<(), PayerRejection> {
        let latest_ms = now_ms.saturating_add(self.terms.max_expiry_secs.saturating_mul(1_000));
        let reason = if valid_before_ms == 0 {
            "valid_before is required"
        } else if valid_before_ms <= now_ms {
            "transaction has expired"
        } else if valid_before_ms > latest_ms {
            "valid_before exceeds maxExpiry"
        } else {
            return Ok(());
        };
        Err(PayerRejection::new(PayerErrorCode::ExpiryOutOfBounds, reason))
    }

    /// Rejects gas beyond the payer's per-transaction ceilings.
    fn check_gas(&self, gas_limit: u64, max_fee_per_gas: u128) -> Result<(), PayerRejection> {
        if self.terms.max_gas_limit.is_some_and(|max| gas_limit > max) {
            return Err(PayerRejection::new(
                PayerErrorCode::GasExceedsLimit,
                "gas_limit exceeds maxGasLimit",
            ));
        }
        let cost = U256::from(gas_limit) * U256::from(max_fee_per_gas);
        match self.terms.max_cost_wei {
            Some(max_cost) if cost > max_cost => Err(PayerRejection {
                gas: Some(Box::new(GasDiagnostic { estimated_cost: cost, max_cost })),
                ..PayerRejection::new(PayerErrorCode::GasExceedsLimit, "cost exceeds maxCost")
            }),
            _ => Ok(()),
        }
    }

    fn invalid(reason: &'static str) -> ErrorObjectOwned {
        PayerRejection::new(PayerErrorCode::InvalidTransaction, reason).into()
    }

    fn unavailable(error: impl std::fmt::Display) -> ErrorObjectOwned {
        warn!(error = %error, "payer state read failed");
        PayerRejection::new(PayerErrorCode::TemporarilyUnavailable, "state unavailable").into()
    }

    fn internal(error: impl std::fmt::Display) -> ErrorObjectOwned {
        warn!(error = %error, "payer signing failed");
        ErrorObjectOwned::owned(ErrorCode::InternalError.code(), "payer signing failed", None::<()>)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use alloy_primitives::{B256, Bytes, hex};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use alloy_sol_types::SolCall;
    use base_common_consensus::{BasePrimitives, Call, TxEip8130};
    use base_common_price_feed::{ChainlinkFeed, ChainlinkLayout, PriceLeg, PricePath, PriceQuote};
    use base_execution_chainspec::BaseChainSpec;
    use base_test_utils::build_test_genesis_everest;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};

    use super::*;
    use crate::{BalanceLayout, IERC20, MockValidityIngress};

    const CHAIN_ID: u64 = 8453;
    const TOKEN: Address = Address::repeat_byte(0x83);
    const NOW_MS: u64 = 1_800_000_000_000;
    const VALID_BEFORE_SECS: u64 = 1_800_000_005;
    const GAS_LIMIT: u64 = 70_000;
    const MAX_FEE_PER_GAS: u128 = 1_500_000_000;
    /// 70,000 gas at 1.5 gwei priced at 2,000 USDC per ETH.
    const REQUIRED: u64 = 210_000;
    const EXPIRY_BOUND: u64 = 31;
    /// Token runtime whose every call returns `true`.
    const RETURNS_TRUE: &[u8] = &hex!("600160005260206000f3");
    /// Token runtime whose every call reverts with `0xdead`.
    const REVERTS: &[u8] = &hex!("61dead6000526002601efd");

    type Provider = MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>;
    type Service = PayerService<Provider, MockValidityIngress, PrivateKeySigner>;

    /// Writes a single-round OCR2 feed with `answer` at 8 decimals.
    fn feed(provider: &Provider, byte: u8, answer: u64) -> ChainlinkFeed {
        let feed = ChainlinkFeed {
            aggregator: Address::repeat_byte(byte),
            layout: ChainlinkLayout::Ocr2,
            decimals: 8,
        };
        let hot_vars = U256::from(1) << ChainlinkLayout::ROUND_ID_BIT_OFFSET;
        provider.add_account(
            feed.aggregator,
            ExtendedAccount::new(0, U256::ZERO).extend_storage([
                (B256::from(feed.layout.hot_vars_slot()), hot_vars),
                (B256::from(feed.layout.transmission_slot(1)), U256::from(answer)),
            ]),
        );
        feed
    }

    /// Provider after Everest holding ETH at $2,000, USDC at $1, and a USDC
    /// token running `token_code` with balance word `balance_word` for `sender`.
    fn provider(
        sender: Address,
        balance_word: U256,
        token_code: &'static [u8],
    ) -> (Provider, PayerToken) {
        let mut genesis = build_test_genesis_everest();
        genesis.config.chain_id = CHAIN_ID;
        let provider = MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(genesis)))
            .with_genesis_block();
        let eth_usd = feed(&provider, 0xe1, 2_000 * 100_000_000);
        let usdc_usd = feed(&provider, 0xe2, 100_000_000);
        let balance = BalanceLayout::FiatToken;
        provider.add_account(
            TOKEN,
            ExtendedAccount::new(0, U256::ZERO)
                .with_bytecode(Bytes::from_static(token_code))
                .extend_storage([(B256::from(balance.slot(sender)), balance_word)]),
        );
        let token = PayerToken {
            symbol: "USDC".to_owned(),
            address: TOKEN,
            decimals: 6,
            spread_bps: 0,
            payment_gas: 20_000,
            price: PricePath {
                quote: PriceQuote::Usd { eth_usd },
                legs: vec![PriceLeg { feed: usdc_usd, invert: false }],
            },
            balance,
        };
        (provider, token)
    }

    fn terms(payer: Address) -> PayerTerms {
        PayerTerms {
            payer,
            max_expiry_secs: 10,
            quote_ttl_secs: 15,
            default_gas_limit: 100_000,
            max_gas_limit: Some(1_000_000),
            max_cost_wei: Some(U256::from(200_000_000_000_000u64)),
        }
    }

    struct Fixture {
        payer: PrivateKeySigner,
        sender: PrivateKeySigner,
        tx: TxEip8130,
        balance_word: U256,
        token_code: &'static [u8],
    }

    impl Fixture {
        fn new() -> Self {
            let payer = PrivateKeySigner::random();
            let tx = TxEip8130 {
                chain_id: CHAIN_ID,
                sender: None,
                nonce_key: U256::ZERO,
                nonce_sequence: 0,
                valid_after: 0,
                valid_before: VALID_BEFORE_SECS,
                max_priority_fee_per_gas: 0,
                max_fee_per_gas: MAX_FEE_PER_GAS,
                gas_limit: GAS_LIMIT,
                account_changes: Vec::new(),
                calls: vec![
                    vec![Self::transfer(payer.address(), REQUIRED)],
                    vec![Call {
                        to: Address::repeat_byte(1),
                        value: U256::ZERO,
                        data: Bytes::new(),
                    }],
                ],
                metadata: Bytes::new(),
                payer: Some(payer.address()),
            };
            Self {
                payer,
                sender: PrivateKeySigner::random(),
                tx,
                balance_word: U256::from(REQUIRED),
                token_code: RETURNS_TRUE,
            }
        }

        fn transfer(to: Address, amount: u64) -> Call {
            Call {
                to: TOKEN,
                value: U256::ZERO,
                data: IERC20::transferCall { to, amount: U256::from(amount) }.abi_encode().into(),
            }
        }

        fn sender_auth(&self) -> Bytes {
            let signature =
                self.sender.sign_hash_sync(&self.tx.sender_signature_hash()).expect("test signer");
            Bytes::from(signature.as_bytes().to_vec())
        }

        fn raw(&self) -> Bytes {
            Eip8130Signed::new(self.tx.clone(), self.sender_auth(), Bytes::new())
                .encoded_2718()
                .into()
        }

        fn service(&self, ingress: MockValidityIngress) -> (Service, PayerToken) {
            let (provider, token) =
                provider(self.sender.address(), self.balance_word, self.token_code);
            let service = PayerService::new(
                terms(self.payer.address()),
                vec![token.clone()],
                provider,
                ingress,
                self.payer.clone(),
            )
            .unwrap();
            (service, token)
        }

        async fn reject(&self) -> PayerRejection {
            let (service, _) = self.service(MockValidityIngress::new());
            let error = service.sponsor(&self.raw(), NOW_MS).await.unwrap_err();
            assert_eq!(error.code(), PayerRejection::RPC_CODE);
            serde_json::from_str(error.data().unwrap().get()).unwrap()
        }
    }

    #[tokio::test]
    async fn co_signs_and_admits_with_balance_predicate() {
        let fixture = Fixture::new();
        let submitted = Arc::new(Mutex::new(None));
        let mut ingress = MockValidityIngress::new();
        ingress.expect_latest_block_expiry_bound().returning(|| Ok(Some(EXPIRY_BOUND)));
        let captured = Arc::clone(&submitted);
        ingress.expect_submit().times(1).returning(move |raw, validity| {
            let hash = *Eip8130Signed::decode_2718_exact(&raw).unwrap().hash();
            *captured.lock().unwrap() = Some((raw, validity));
            Ok(hash)
        });
        let (service, token) = fixture.service(ingress);

        let result = service.sponsor(&fixture.raw(), NOW_MS).await.unwrap();

        let (raw, validity) = submitted.lock().unwrap().take().unwrap();
        let co_signed = Eip8130Signed::decode_2718_exact(&raw).unwrap();
        assert_eq!(&result.transaction_hash, co_signed.hash());
        assert_eq!(
            result.token_charged,
            TokenCharged { token: TOKEN, amount: U256::from(REQUIRED) }
        );
        assert_eq!(co_signed.tx(), &fixture.tx);
        assert_eq!(co_signed.sender_auth(), &fixture.sender_auth());

        let payer_auth = co_signed.payer_auth();
        assert_eq!(&payer_auth[..20], Eip8130Constants::K1_AUTHENTICATOR.as_slice());
        let signer = Eip8130Signed::recover_raw_k1(
            fixture.tx.payer_signature_hash(fixture.sender.address()),
            &payer_auth[20..],
        )
        .unwrap();
        assert_eq!(signer, fixture.payer.address());

        assert_eq!(
            validity,
            vec![
                token.balance.predicate(TOKEN, fixture.sender.address(), U256::from(REQUIRED)),
                ValidityPredicate::BlockNumber {
                    op: ValidityOperator::LessThanOrEqual,
                    value: U256::from(EXPIRY_BOUND),
                },
            ]
        );
    }

    #[tokio::test]
    async fn requotes_insufficient_payment() {
        let mut fixture = Fixture::new();
        fixture.tx.calls[0] = vec![Fixture::transfer(fixture.payer.address(), REQUIRED - 1)];

        let rejection = fixture.reject().await;

        assert_eq!(rejection.code, PayerErrorCode::PaymentInsufficient);
        assert_eq!(
            rejection.requote.as_deref(),
            Some(&Requote {
                token: TOKEN,
                payment_amount: U256::from(REQUIRED),
                rate: U256::from(2_000_000_000u64),
                ttl: 15,
            })
        );
    }

    #[tokio::test]
    async fn rejects_sender_that_cannot_pay() {
        let mut underfunded = Fixture::new();
        underfunded.balance_word = U256::from(REQUIRED - 1);
        let rejection = underfunded.reject().await;
        assert_eq!(rejection.code, PayerErrorCode::SenderBalanceInsufficient);
        assert_eq!(
            rejection.shortfall.as_deref(),
            Some(&Shortfall {
                token: TOKEN,
                required: U256::from(REQUIRED),
                available: U256::from(REQUIRED - 1),
            })
        );
    }

    #[tokio::test]
    async fn rejects_payment_that_fails_to_transfer() {
        let mut fixture = Fixture::new();
        fixture.token_code = REVERTS;

        let rejection = fixture.reject().await;

        assert_eq!(rejection.code, PayerErrorCode::ExecutionReverted);
        assert_eq!(
            rejection.revert.as_deref(),
            Some(&Revert { phase: 0, data: Some(Bytes::from_static(&[0xde, 0xad])) })
        );
    }

    #[tokio::test]
    async fn rejects_transactions_outside_the_terms() {
        type Mutation = fn(&mut Fixture);
        let cases: [(Mutation, PayerErrorCode); 8] = [
            (|f| f.tx.valid_before = 0, PayerErrorCode::ExpiryOutOfBounds),
            (|f| f.tx.valid_before = NOW_MS / 1_000, PayerErrorCode::ExpiryOutOfBounds),
            (|f| f.tx.valid_before = NOW_MS / 1_000 + 11, PayerErrorCode::ExpiryOutOfBounds),
            (|f| f.tx.max_fee_per_gas = 1_000_000_000_000, PayerErrorCode::GasExceedsLimit),
            (|f| f.tx.gas_limit = 1_000_001, PayerErrorCode::GasExceedsLimit),
            (|f| f.tx.payer = Some(Address::repeat_byte(9)), PayerErrorCode::InvalidTransaction),
            (
                |f| f.tx.calls[0] = vec![Fixture::transfer(Address::repeat_byte(9), REQUIRED)],
                PayerErrorCode::InvalidTransaction,
            ),
            (
                |f| f.tx.calls[0][0].to = Address::repeat_byte(0x84),
                PayerErrorCode::UnsupportedToken,
            ),
        ];
        for (mutate, code) in cases {
            let mut fixture = Fixture::new();
            mutate(&mut fixture);
            assert_eq!(fixture.reject().await.code, code);
        }
    }

    #[tokio::test]
    async fn rejects_transaction_already_co_signed() {
        let fixture = Fixture::new();
        let (service, _) = fixture.service(MockValidityIngress::new());
        let raw: Bytes =
            Eip8130Signed::new(fixture.tx.clone(), fixture.sender_auth(), Bytes::from_static(&[1]))
                .encoded_2718()
                .into();

        let error = service.sponsor(&raw, NOW_MS).await.unwrap_err();

        let rejection: PayerRejection = serde_json::from_str(error.data().unwrap().get()).unwrap();
        assert_eq!(rejection.code, PayerErrorCode::InvalidTransaction);
    }

    #[test]
    fn rejects_signer_for_another_account() {
        let fixture = Fixture::new();
        let (provider, token) = provider(fixture.sender.address(), U256::ZERO, RETURNS_TRUE);

        let error = PayerService::new(
            terms(fixture.payer.address()),
            vec![token],
            provider,
            MockValidityIngress::new(),
            fixture.sender,
        )
        .unwrap_err();

        assert!(matches!(error, PayerConfigError::SignerMismatch { .. }));
    }

    #[test]
    fn quotes_tokens_at_the_advisory_gas_estimate() {
        let fixture = Fixture::new();
        let (service, _) = fixture.service(MockValidityIngress::new());
        let params = GetTermsParams {
            chain_id: U64::from(CHAIN_ID),
            gas_limit: Some(U64::from(50_000)),
            preferred_tokens: Vec::new(),
        };

        let result = service.terms(&params).unwrap();

        // 50,000 call gas plus 20,000 payment gas at twice the 1 gwei genesis base fee.
        assert_eq!(
            result.gas_estimate,
            Some(GasEstimate {
                gas_limit: U64::from(70_000),
                max_fee_per_gas: U128::from(2_000_000_000u64),
                max_priority_fee_per_gas: U128::ZERO,
            })
        );
        let [PaymentOption::Token(offer)] = result.options.as_slice() else {
            panic!("expected one token offer");
        };
        assert_eq!(offer.payer, fixture.payer.address());
        assert_eq!(offer.conditions.max_expiry, 10);
        assert_eq!(offer.tokens.len(), 1);
        assert_eq!(offer.tokens[0].rate, U256::from(2_000_000_000u64));
        assert_eq!(offer.tokens[0].payment_amount, U256::from(280_000));
    }

    #[test]
    fn get_terms_rejects_other_chains() {
        let fixture = Fixture::new();
        let (service, _) = fixture.service(MockValidityIngress::new());
        let params = GetTermsParams { chain_id: U64::from(1), ..Default::default() };

        let error = service.terms(&params).unwrap_err();

        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
    }
}
