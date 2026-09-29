//! ERC-8168 token-payer fixtures and the wallet side of the payer flow.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U64, U256};
use alloy_provider::Provider;
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::TransactionRequest;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolConstructor};
use base_common_consensus::{Call, Eip8130Signed, TxEip8130};
use base_common_price_feed::test_utils::{InitCode, MockFeed};
use base_execution_payer::{
    BalanceLayout, FeedConfig, GetTermsParams, GetTermsResult, IERC20, LegConfig, PayerConfig,
    PayerTerms, PaymentOption, PriceConfig, QuoteConfig, SendTransactionParams,
    SendTransactionResult, TokenChoice, TokenConfig,
};
use base_test_utils::MockERC20;
use eyre::{OptionExt, Result, WrapErr, ensure};

/// Mock Chainlink feeds and a USDC-like token for exercising the payer on a
/// chain without real feeds.
///
/// The contracts sit at the addresses the deployer's next nonces create, so the
/// payer configuration can be written before they are deployed.
#[derive(Debug, Clone, Copy)]
pub struct PayerFixtures {
    /// Account deploying the fixtures, also the token's probe holder.
    pub deployer: Address,
    /// ETH / USD feed.
    pub eth_usd: MockFeed,
    /// USDC / USD feed.
    pub usdc_usd: MockFeed,
    /// Solmate `MockERC20` with 6 decimals and a public `mint`.
    pub token: Address,
}

impl PayerFixtures {
    /// ETH / USD answer: $3,000 at 8 decimals.
    pub const ETH_USD_ANSWER: u64 = 300_000_000_000;
    /// USDC / USD answer: $1 at 8 decimals.
    pub const USDC_USD_ANSWER: u64 = 100_000_000;
    /// Token decimals.
    pub const TOKEN_DECIMALS: u8 = 6;
    /// Storage slot of solmate `ERC20.balanceOf`.
    pub const TOKEN_BALANCES_SLOT: U256 = U256::from_limbs([3, 0, 0, 0]);
    /// Token minted to the probe holder: 1,000 USDC.
    pub const PROBE_BALANCE: U256 = U256::from_limbs([1_000_000_000, 0, 0, 0]);
    /// Gas assumed for a wallet's calls beyond the phase-0 payment.
    pub const INTENT_GAS: u64 = 150_000;
    /// Longest `valid_before` the payer co-signs, in seconds from now.
    pub const MAX_EXPIRY_SECS: u64 = 60;
    /// Gas the phase-0 `transfer` adds.
    pub const PAYMENT_GAS: u64 = 60_000;
    /// How long each fixture transaction may take to land.
    pub const RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

    /// Fixtures `deployer` creates starting at `nonce`.
    pub fn new(deployer: Address, nonce: u64) -> Self {
        let feed = |offset: u64, answer: u64| MockFeed {
            aggregator: deployer.create(nonce + offset),
            proxy: deployer.create(nonce + offset + 1),
            ..MockFeed::new(0, answer)
        };
        Self {
            deployer,
            eth_usd: feed(0, Self::ETH_USD_ANSWER),
            usdc_usd: feed(2, Self::USDC_USD_ANSWER),
            token: deployer.create(nonce + 4),
        }
    }

    /// Payer configuration accepting the fixture token, paid to `payer`.
    pub fn payer_config(&self, payer: Address) -> PayerConfig {
        PayerConfig {
            terms: PayerTerms {
                payer,
                max_expiry_secs: Self::MAX_EXPIRY_SECS,
                quote_ttl_secs: 15,
                default_gas_limit: Self::INTENT_GAS,
                max_gas_limit: None,
                max_cost_wei: None,
            },
            eth_usd: FeedConfig { proxy: self.eth_usd.proxy, deviation_bps: 15 },
            tokens: vec![TokenConfig {
                symbol: "USDC".to_owned(),
                address: self.token,
                decimals: Self::TOKEN_DECIMALS,
                spread_bps: 100,
                payment_gas: Self::PAYMENT_GAS,
                probe_holder: self.deployer,
                price: PriceConfig {
                    quote: QuoteConfig::Usd,
                    legs: vec![LegConfig {
                        proxy: self.usdc_usd.proxy,
                        deviation_bps: 30,
                        invert: false,
                    }],
                },
                balance: BalanceLayout::Mapping { slot: Self::TOKEN_BALANCES_SLOT },
            }],
        }
    }

    /// Deploys the fixtures through `wallet`, which must sign for
    /// [`Self::deployer`] at the nonce the fixtures were created for, and
    /// funds the probe holder.
    pub async fn deploy(&self, wallet: &impl Provider) -> Result<()> {
        let feed = |mock: &MockFeed| {
            [
                InitCode::deploying(&mock.aggregator_code(mock.answer), &mock.aggregator_storage()),
                InitCode::deploying(&mock.proxy_code(), &[]),
            ]
        };
        let token: Bytes = [
            MockERC20::BYTECODE.as_ref(),
            &MockERC20::constructorCall {
                _name: "Devnet USDC".to_owned(),
                _symbol: "USDC".to_owned(),
                _decimals: Self::TOKEN_DECIMALS,
            }
            .abi_encode(),
        ]
        .concat()
        .into();
        let creations = feed(&self.eth_usd).into_iter().chain(feed(&self.usdc_usd)).chain([token]);
        for code in creations {
            Self::send(wallet, TransactionRequest::default().with_deploy_code(code)).await?;
        }
        ensure!(
            !wallet.get_code_at(self.token).await?.is_empty(),
            "fixture token is not at {}; was the deployer nonce stale?",
            self.token
        );
        Self::mint(wallet, self.token, self.deployer, Self::PROBE_BALANCE).await
    }

    /// Mints `amount` of the fixture `token` to `to`.
    pub async fn mint(
        wallet: &impl Provider,
        token: Address,
        to: Address,
        amount: U256,
    ) -> Result<()> {
        let mint = MockERC20::mintCall { to, value: amount }.abi_encode();
        Self::send(wallet, TransactionRequest::default().with_to(token).with_input(mint)).await
    }

    /// Returns `account`'s balance of `token`.
    pub async fn balance_of(
        provider: &impl Provider,
        token: Address,
        account: Address,
    ) -> Result<U256> {
        let call = IERC20::balanceOfCall { account }.abi_encode();
        let output =
            provider.call(TransactionRequest::default().with_to(token).with_input(call)).await?;
        Ok(IERC20::balanceOfCall::abi_decode_returns(&output)?)
    }

    async fn send(wallet: &impl Provider, request: TransactionRequest) -> Result<()> {
        let receipt = wallet
            .send_transaction(request)
            .await?
            .with_timeout(Some(Self::RECEIPT_TIMEOUT))
            .get_receipt()
            .await
            .wrap_err("fixture transaction did not land")?;
        ensure!(receipt.status(), "fixture transaction {} reverted", receipt.transaction_hash);
        Ok(())
    }
}

/// A co-signed transaction the payer accepted.
#[derive(Debug, Clone)]
pub struct SponsoredSubmission {
    /// Payer that co-signed.
    pub payer: Address,
    /// Quote the wallet paid.
    pub choice: TokenChoice,
    /// The payer's response.
    pub result: SendTransactionResult,
}

/// Wallet paying for its EIP-8130 transactions in a token through an ERC-8168
/// payer, with an implicit EOA sender.
#[derive(Debug, Clone)]
pub struct TokenPayerWallet {
    /// Sender key.
    pub signer: PrivateKeySigner,
    /// Chain the wallet transacts on.
    pub chain_id: u64,
}

impl TokenPayerWallet {
    /// Quotes `token` from the payer at `rpc`, then submits `calls` behind a
    /// phase-0 payment of the quoted amount.
    ///
    /// `calls` run as phase 1 and may be empty. `nonce_sequence` is the
    /// sender's next EIP-8130 nonce on nonce key zero.
    pub async fn send(
        &self,
        rpc: &RpcClient,
        token: Address,
        calls: Vec<Call>,
        nonce_sequence: u64,
    ) -> Result<SponsoredSubmission> {
        let terms: GetTermsResult = rpc
            .request(
                "payer_getTerms",
                (GetTermsParams {
                    chain_id: U64::from(self.chain_id),
                    gas_limit: Some(U64::from(PayerFixtures::INTENT_GAS)),
                    preferred_tokens: vec![token],
                },),
            )
            .await
            .wrap_err("payer_getTerms failed")?;
        let gas = terms.gas_estimate.ok_or_eyre("terms carry no gas estimate")?;
        let (payer, choice) = terms
            .options
            .into_iter()
            .find_map(|PaymentOption::Token(offer)| {
                let choice = offer.tokens.into_iter().find(|choice| choice.token == token)?;
                Some((offer.payer, choice))
            })
            .ok_or_eyre("payer does not accept the token")?;

        let payment = Call {
            to: token,
            value: U256::ZERO,
            data: IERC20::transferCall { to: payer, amount: choice.payment_amount }
                .abi_encode()
                .into(),
        };
        let mut phases = vec![vec![payment]];
        if !calls.is_empty() {
            phases.push(calls);
        }
        let valid_before = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
            + PayerFixtures::MAX_EXPIRY_SECS / 2;
        let tx = TxEip8130 {
            chain_id: self.chain_id,
            sender: None,
            nonce_key: U256::ZERO,
            nonce_sequence,
            valid_after: 0,
            valid_before,
            max_priority_fee_per_gas: gas.max_priority_fee_per_gas.to(),
            max_fee_per_gas: gas.max_fee_per_gas.to(),
            gas_limit: gas.gas_limit.to(),
            account_changes: Vec::new(),
            calls: phases,
            metadata: Bytes::new(),
            payer: Some(payer),
        };
        let sender_auth: Bytes =
            self.signer.sign_hash_sync(&tx.sender_signature_hash())?.as_bytes().into();
        let signed = Eip8130Signed::new(tx, sender_auth, Bytes::new());

        let result: SendTransactionResult = rpc
            .request(
                "payer_sendTransaction",
                (SendTransactionParams { signed_transaction: signed.encoded_2718().into() },),
            )
            .await
            .wrap_err("payer_sendTransaction failed")?;
        Ok(SponsoredSubmission { payer, choice, result })
    }
}
