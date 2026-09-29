//! System test for the ERC-8168 token payer served by the sequencer's builder.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_consensus::Typed2718;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::{EthereumWallet, ReceiptResponse, TransactionBuilder};
use alloy_primitives::{Address, Bytes, U64, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::TransactionRequest;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolConstructor};
use base_common_consensus::{Call, Eip8130Signed, TxEip8130};
use base_common_price_feed::test_utils::{InitCode, MockFeed};
use base_execution_payer::{
    BalanceLayout, FeedConfig, GetTermsParams, GetTermsResult, IERC20, LegConfig, PayerConfig,
    PayerExtension, PayerExtensionConfig, PayerSponsor, PayerTerms, PaymentOption, PriceConfig,
    QuoteConfig, SendTransactionParams, SendTransactionResult, TokenConfig,
};
use base_execution_txpool::{DEFAULT_MAX_VALIDITY_EXPIRY_SECS, DEFAULT_MAX_VALIDITY_PREDICATES};
use base_node_runner::FromExtensionConfig;
use base_system_tests::{
    ANVIL_ACCOUNT_6, ANVIL_ACCOUNT_7, SystemTestProviderExt, SystemTestStack,
    SystemTestStackBuilder,
};
use base_test_utils::MockERC20;
use eyre::{OptionExt, Result, WrapErr, ensure};
use url::Url;

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const EIP8130_TX_TYPE: u8 = 0x79;
const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

/// ETH/USD at $3,000 with 8 decimals.
const ETH_USD_ANSWER: u64 = 300_000_000_000;
/// USDC/USD at $1 with 8 decimals.
const USDC_USD_ANSWER: u64 = 100_000_000;
const USDC_DECIMALS: u8 = 6;
/// Storage slot of solmate `ERC20.balanceOf`.
const MOCK_ERC20_BALANCES_SLOT: U256 = U256::from_limbs([3, 0, 0, 0]);
/// 1,000 USDC.
const WALLET_USDC: U256 = U256::from_limbs([1_000_000_000, 0, 0, 0]);
/// Gas the wallet's intent needs beyond the phase-0 payment.
const INTENT_GAS: u64 = 150_000;
const MAX_EXPIRY_SECS: u64 = 60;

/// Contracts the deployer creates, at the addresses its first nonces produce.
#[derive(Debug, Clone, Copy)]
struct Fixtures {
    eth_usd: MockFeed,
    usdc_usd: MockFeed,
    token: Address,
}

impl Fixtures {
    fn new(deployer: Address) -> Self {
        let feed = |nonce: u64, answer: u64| MockFeed {
            aggregator: deployer.create(nonce),
            proxy: deployer.create(nonce + 1),
            ..MockFeed::new(0, answer)
        };
        Self {
            eth_usd: feed(0, ETH_USD_ANSWER),
            usdc_usd: feed(2, USDC_USD_ANSWER),
            token: deployer.create(4),
        }
    }

    fn payer_config(&self, payer: Address, probe_holder: Address) -> PayerConfig {
        PayerConfig {
            terms: PayerTerms {
                payer,
                max_expiry_secs: MAX_EXPIRY_SECS,
                quote_ttl_secs: 15,
                default_gas_limit: INTENT_GAS,
                max_gas_limit: None,
                max_cost_wei: None,
            },
            eth_usd: FeedConfig { proxy: self.eth_usd.proxy, deviation_bps: 15 },
            tokens: vec![TokenConfig {
                symbol: "USDC".to_owned(),
                address: self.token,
                decimals: USDC_DECIMALS,
                spread_bps: 100,
                payment_gas: 60_000,
                probe_holder,
                price: PriceConfig {
                    quote: QuoteConfig::Usd,
                    legs: vec![LegConfig {
                        proxy: self.usdc_usd.proxy,
                        deviation_bps: 30,
                        invert: false,
                    }],
                },
                balance: BalanceLayout::Mapping { slot: MOCK_ERC20_BALANCES_SLOT },
            }],
        }
    }

    /// Creation transactions in nonce order, ending with the token mints.
    fn deployments(&self, wallet: Address, probe_holder: Address) -> Vec<TransactionRequest> {
        let feed = |mock: &MockFeed| {
            [
                InitCode::deploying(&mock.aggregator_code(mock.answer), &mock.aggregator_storage()),
                InitCode::deploying(&mock.proxy_code(), &[]),
            ]
        };
        let token = [
            MockERC20::BYTECODE.as_ref(),
            &MockERC20::constructorCall {
                _name: "Devnet USDC".to_owned(),
                _symbol: "USDC".to_owned(),
                _decimals: USDC_DECIMALS,
            }
            .abi_encode(),
        ]
        .concat();
        let mint = |to: Address| {
            TransactionRequest::default()
                .with_to(self.token)
                .with_input(MockERC20::mintCall { to, value: WALLET_USDC }.abi_encode())
        };
        feed(&self.eth_usd)
            .into_iter()
            .chain(feed(&self.usdc_usd))
            .chain([token.into()])
            .map(|code: Bytes| TransactionRequest::default().with_deploy_code(code))
            .chain([mint(wallet), mint(probe_holder)])
            .collect()
    }
}

async fn start_payer_system(sponsor: PayerSponsor) -> Result<SystemTestStack> {
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_base_cobalt_activation_block(0)
        .with_base_denim_activation_block(0)
        .with_base_everest_activation_block(0)
        .with_payload_builder_cutover()
        .with_builder_extension(Box::new(PayerExtension::from_config(
            PayerExtensionConfig::Sponsor(Box::new(sponsor)),
        )))
        .build()
        .await?;
    system.l2_builder_provider()?.wait_for_block(3, Duration::from_secs(15)).await?;
    Ok(system)
}

async fn token_balance(provider: &impl Provider, token: Address, account: Address) -> Result<U256> {
    let output = provider
        .call(
            TransactionRequest::default()
                .with_to(token)
                .with_input(IERC20::balanceOfCall { account }.abi_encode()),
        )
        .await?;
    Ok(IERC20::balanceOfCall::abi_decode_returns(&output)?)
}

/// A wallet holding USDC and no ETH gets an EIP-8130 transaction included by
/// paying the sequencer's payer USDC in phase 0 through `payer_sendTransaction`.
#[tokio::test]
async fn payer_sponsors_eip8130_transaction_paid_in_token() -> Result<()> {
    let deployer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_6.private_key)?;
    let payer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_7.private_key)?;
    let wallet = PrivateKeySigner::random();
    let fixtures = Fixtures::new(deployer.address());
    let sponsor = PayerSponsor {
        config: fixtures.payer_config(payer.address(), deployer.address()),
        signer: payer.clone(),
        max_validity_predicates: DEFAULT_MAX_VALIDITY_PREDICATES,
        max_validity_expiry_secs: DEFAULT_MAX_VALIDITY_EXPIRY_SECS,
        experimental_override: false,
    };
    let system = start_payer_system(sponsor).await?;
    let l2_url: Url = system.l2_rpc_url()?;
    let provider = system.l2_builder_provider()?;
    provider.wait_for_balance(deployer.address(), Duration::from_secs(15)).await?;
    provider.wait_for_balance(payer.address(), Duration::from_secs(15)).await?;

    ensure!(
        provider.get_transaction_count(deployer.address()).await? == 0,
        "fixture addresses assume a fresh deployer"
    );
    let deployer_provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(deployer.clone()))
        .connect_http(l2_url.clone());
    for request in fixtures.deployments(wallet.address(), deployer.address()) {
        let tx_hash = *deployer_provider.send_transaction(request).await?.tx_hash();
        let receipt = provider.wait_for_receipt(tx_hash, TX_RECEIPT_TIMEOUT).await?;
        ensure!(receipt.status(), "fixture transaction {tx_hash} reverted");
    }
    ensure!(
        token_balance(&deployer_provider, fixtures.token, wallet.address()).await? == WALLET_USDC,
        "wallet must hold the minted USDC"
    );

    let rpc = RpcClient::builder().http(l2_url);
    let terms: GetTermsResult = rpc
        .request(
            "payer_getTerms",
            (GetTermsParams {
                chain_id: U64::from(L2_CHAIN_ID),
                gas_limit: Some(U64::from(INTENT_GAS)),
                preferred_tokens: vec![fixtures.token],
            },),
        )
        .await
        .wrap_err("payer_getTerms failed")?;
    let gas = terms.gas_estimate.ok_or_eyre("terms must carry a gas estimate")?;
    let [PaymentOption::Token(offer)] = terms.options.as_slice() else {
        eyre::bail!("expected one token offer, got {:?}", terms.options);
    };
    ensure!(offer.payer == payer.address(), "offer must name the configured payer");
    let [choice] = offer.tokens.as_slice() else {
        eyre::bail!("expected one accepted token, got {:?}", offer.tokens);
    };
    ensure!(choice.token == fixtures.token, "offer must accept the fixture token");
    ensure!(choice.payment_amount > U256::ZERO, "offer must charge for gas");

    let valid_before =
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + MAX_EXPIRY_SECS / 2;
    let tx = TxEip8130 {
        chain_id: L2_CHAIN_ID,
        sender: None,
        nonce_key: U256::ZERO,
        nonce_sequence: 0,
        valid_after: 0,
        valid_before,
        max_priority_fee_per_gas: gas.max_priority_fee_per_gas.to(),
        max_fee_per_gas: gas.max_fee_per_gas.to(),
        gas_limit: gas.gas_limit.to(),
        account_changes: Vec::new(),
        calls: vec![vec![Call {
            to: fixtures.token,
            value: U256::ZERO,
            data: IERC20::transferCall { to: payer.address(), amount: choice.payment_amount }
                .abi_encode()
                .into(),
        }]],
        metadata: Bytes::new(),
        payer: Some(payer.address()),
    };
    let sender_auth: Bytes = wallet.sign_hash_sync(&tx.sender_signature_hash())?.as_bytes().into();
    let signed = Eip8130Signed::new(tx, sender_auth, Bytes::new());

    let sent: SendTransactionResult = rpc
        .request(
            "payer_sendTransaction",
            (SendTransactionParams { signed_transaction: signed.encoded_2718().into() },),
        )
        .await
        .wrap_err("payer_sendTransaction failed")?;
    ensure!(sent.token_charged.token == fixtures.token, "charge must be in the fixture token");
    ensure!(
        sent.token_charged.amount == choice.payment_amount,
        "charge must equal the phase-0 transfer"
    );

    let receipt = provider.wait_for_receipt(sent.transaction_hash, TX_RECEIPT_TIMEOUT).await?;
    ensure!(receipt.status(), "sponsored transaction must succeed");
    ensure!(receipt.inner.inner.receipt.ty() == EIP8130_TX_TYPE, "receipt must be type 0x79");
    ensure!(receipt.payer == Some(payer.address()), "receipt must name the payer");

    let block = receipt.block_number().ok_or_eyre("receipt must be mined")?;
    let payer_before = provider.get_balance(payer.address()).number(block - 1).await?;
    let payer_after = provider.get_balance(payer.address()).number(block).await?;
    let execution_fee = U256::from(receipt.gas_used()) * U256::from(receipt.effective_gas_price());
    ensure!(payer_before - payer_after >= execution_fee, "payer must pay the transaction's gas");
    ensure!(
        provider.get_balance(wallet.address()).await? == U256::ZERO,
        "wallet must not spend ETH"
    );
    ensure!(
        token_balance(&deployer_provider, fixtures.token, payer.address()).await?
            == choice.payment_amount,
        "payer must receive the phase-0 payment"
    );
    ensure!(
        token_balance(&deployer_provider, fixtures.token, wallet.address()).await?
            == WALLET_USDC - choice.payment_amount,
        "wallet must pay the phase-0 payment"
    );

    system.shutdown().await?;
    Ok(())
}
