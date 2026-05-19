//! B-20 precompile RPC client helpers.

use std::time::Duration;

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::TransactionInput;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolValue};
use base_common_network::Base;
use base_common_precompiles::{IB20, ITokenFactory, TokenFactory, TokenVariant};
use base_common_rpc_types::BaseTransactionRequest;
use eyre::{Result, WrapErr, ensure};
use tokio::time::{sleep, timeout};

/// RPC client for the B-20 token factory and created token precompiles.
#[derive(Debug)]
pub struct B20PrecompileClient<'a> {
    provider: &'a RootProvider<Base>,
    signer: &'a PrivateKeySigner,
    chain_id: u64,
    gas_limit: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    receipt_timeout: Duration,
}

impl<'a> B20PrecompileClient<'a> {
    /// Default gas limit used when sending B-20 transactions.
    pub const DEFAULT_GAS_LIMIT: u64 = 10_000_000;

    /// Default max fee per gas used when sending B-20 transactions.
    pub const DEFAULT_MAX_FEE_PER_GAS: u128 = 1_000_000_000;

    /// Default priority fee per gas used when sending B-20 transactions.
    pub const DEFAULT_MAX_PRIORITY_FEE_PER_GAS: u128 = 1_000_000;

    /// Default receipt timeout used after sending B-20 transactions.
    pub const DEFAULT_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

    /// Creates a B-20 precompile client.
    pub const fn new(
        provider: &'a RootProvider<Base>,
        signer: &'a PrivateKeySigner,
        chain_id: u64,
    ) -> Self {
        Self {
            provider,
            signer,
            chain_id,
            gas_limit: Self::DEFAULT_GAS_LIMIT,
            max_fee_per_gas: Self::DEFAULT_MAX_FEE_PER_GAS,
            max_priority_fee_per_gas: Self::DEFAULT_MAX_PRIORITY_FEE_PER_GAS,
            receipt_timeout: Self::DEFAULT_RECEIPT_TIMEOUT,
        }
    }

    /// Sets the gas limit used for B-20 transactions.
    pub const fn with_gas_limit(mut self, gas_limit: u64) -> Self {
        self.gas_limit = gas_limit;
        self
    }

    /// Sets the receipt timeout used after sending B-20 transactions.
    pub const fn with_receipt_timeout(mut self, receipt_timeout: Duration) -> Self {
        self.receipt_timeout = receipt_timeout;
        self
    }

    /// Sets the max fee per gas used for B-20 transactions.
    pub const fn with_max_fee_per_gas(mut self, max_fee_per_gas: u128) -> Self {
        self.max_fee_per_gas = max_fee_per_gas;
        self
    }

    /// Sets the priority fee per gas used for B-20 transactions.
    pub const fn with_max_priority_fee_per_gas(mut self, max_priority_fee_per_gas: u128) -> Self {
        self.max_priority_fee_per_gas = max_priority_fee_per_gas;
        self
    }

    /// Builds the required B-20 token params for factory creation.
    pub fn token_params(
        name: &str,
        symbol: &str,
        decimals: u8,
        initial_supply: U256,
        initial_supply_recipient: Address,
    ) -> ITokenFactory::B20TokenParams {
        ITokenFactory::B20TokenParams {
            name: name.to_string(),
            symbol: symbol.to_string(),
            decimals,
            admin: initial_supply_recipient,
            capabilities: U256::ZERO,
            initialSupply: initial_supply,
            initialSupplyRecipient: initial_supply_recipient,
            supplyCap: U256::MAX,
            minimumRedeemable: U256::ZERO,
            contractURI: String::new(),
        }
    }

    /// Creates a B-20 token through the factory and returns the deterministic token address.
    pub async fn create_token(
        &self,
        variant: TokenVariant,
        params: ITokenFactory::B20TokenParams,
        salt: B256,
    ) -> Result<Address> {
        let token = self.predict_token_address(variant, params.decimals, salt);
        let call = ITokenFactory::createTokenCall {
            params: ITokenFactory::CreateTokenParams {
                version: TokenFactory::CREATE_TOKEN_VERSION,
                variant: variant.discriminant(),
                requiredParams: params.abi_encode().into(),
                optionalParams: Bytes::new(),
                postCreateCalls: Vec::new(),
                salt,
            },
        };
        self.send_call(TokenFactory::ADDRESS, call, "create B-20 token").await?;
        Ok(token)
    }

    /// Computes the token address a factory creation call will use.
    pub fn predict_token_address(
        &self,
        variant: TokenVariant,
        decimals: u8,
        salt: B256,
    ) -> Address {
        variant.compute_address(self.signer.address(), decimals, salt).0
    }

    /// Waits for a created token address to return non-empty bytecode.
    pub async fn wait_for_token_code(
        &self,
        token: Address,
        wait_timeout: Duration,
        poll_interval: Duration,
    ) -> Result<()> {
        timeout(wait_timeout, async {
            loop {
                let code = self.provider.get_code_at(token).await?;
                if !code.is_empty() {
                    return Ok::<_, eyre::Error>(());
                }
                sleep(poll_interval).await;
            }
        })
        .await
        .wrap_err("Timed out waiting for B-20 token code")?
    }

    /// Reads the B-20 balance for an account.
    pub async fn balance_of(&self, token: Address, account: Address) -> Result<U256> {
        let output = self.call(token, IB20::balanceOfCall { account }).await?;
        IB20::balanceOfCall::abi_decode_returns(output.as_ref())
            .wrap_err("Failed to decode balanceOf")
    }

    /// Reads the variant encoded in a token address via the factory.
    pub async fn variant_of(&self, token: Address) -> Result<u8> {
        let output =
            self.call(TokenFactory::ADDRESS, ITokenFactory::variantOfCall { token }).await?;
        ITokenFactory::variantOfCall::abi_decode_returns(output.as_ref())
            .wrap_err("Failed to decode variantOf")
    }

    /// Reads the decimals encoded in a token address via the factory.
    pub async fn decimals_of(&self, token: Address) -> Result<u8> {
        let output =
            self.call(TokenFactory::ADDRESS, ITokenFactory::decimalsOfCall { token }).await?;
        ITokenFactory::decimalsOfCall::abi_decode_returns(output.as_ref())
            .wrap_err("Failed to decode decimalsOf")
    }

    /// Mints B-20 tokens to an account.
    pub async fn mint(&self, token: Address, to: Address, amount: U256) -> Result<()> {
        self.send_call(token, IB20::mintCall { to, amount }, "mint B-20 token").await
    }

    /// Transfers B-20 tokens.
    pub async fn transfer(&self, token: Address, to: Address, amount: U256) -> Result<()> {
        self.send_call(token, IB20::transferCall { to, amount }, "transfer B-20 token").await
    }

    /// Executes an `eth_call` against `to`.
    pub async fn call<C>(&self, to: Address, call: C) -> Result<Bytes>
    where
        C: SolCall,
    {
        let request = BaseTransactionRequest::default()
            .from(self.signer.address())
            .to(to)
            .input(TransactionInput::new(Bytes::from(call.abi_encode())));

        self.provider.call(request).await.wrap_err("B-20 eth_call failed")
    }

    /// Signs, sends, and waits for a transaction against `to`.
    pub async fn send_call<C>(&self, to: Address, call: C, label: &'static str) -> Result<()>
    where
        C: SolCall,
    {
        let nonce = self.provider.get_transaction_count(self.signer.address()).await?;
        let (raw_tx, expected_tx_hash) =
            self.create_signed_tx(to, nonce, Bytes::from(call.abi_encode())).wrap_err(label)?;

        let pending_tx = self
            .provider
            .send_raw_transaction(&raw_tx)
            .await
            .wrap_err_with(|| format!("Failed to send {label} transaction"))?;
        let tx_hash = *pending_tx.tx_hash();
        ensure!(tx_hash == expected_tx_hash, "{label} transaction hash mismatch");

        let receipt = timeout(self.receipt_timeout, async {
            loop {
                if let Some(receipt) = self.provider.get_transaction_receipt(tx_hash).await? {
                    return Ok::<_, eyre::Error>(receipt);
                }
                sleep(Duration::from_secs(1)).await;
            }
        })
        .await
        .wrap_err_with(|| format!("{label} receipt timed out"))?
        .wrap_err_with(|| format!("Failed to get {label} receipt"))?;

        ensure!(receipt.status(), "{label} transaction reverted");
        ensure!(receipt.inner.to == Some(to), "{label} receipt target mismatch");

        Ok(())
    }

    /// Creates a signed transaction targeting `to`.
    pub fn create_signed_tx(&self, to: Address, nonce: u64, input: Bytes) -> Result<(Bytes, B256)> {
        let tx_request = BaseTransactionRequest::default()
            .from(self.signer.address())
            .to(to)
            .value(U256::ZERO)
            .transaction_type(2)
            .gas_limit(self.gas_limit)
            .max_fee_per_gas(self.max_fee_per_gas)
            .max_priority_fee_per_gas(self.max_priority_fee_per_gas)
            .chain_id(self.chain_id)
            .nonce(nonce)
            .input(TransactionInput::new(input));

        let tx = tx_request
            .build_typed_tx()
            .map_err(|tx| eyre::eyre!("invalid B-20 transaction request: {tx:?}"))?;
        let signature = self.signer.sign_hash_sync(&tx.signature_hash())?;
        let signed_tx = tx.into_signed(signature);
        let tx_hash = *signed_tx.hash();
        let raw_tx = signed_tx.encoded_2718().into();

        Ok((raw_tx, tx_hash))
    }
}
