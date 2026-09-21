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
use base_common_precompiles::{
    ActivationRegistryStorage, B20FactoryStorage, B20Variant, IActivationRegistry, IB20,
    IB20Factory,
};
use base_common_rpc_types::{BaseTransactionReceipt, BaseTransactionRequest};
use eyre::{Result, WrapErr, ensure};
use tokio::time::{sleep, timeout};

/// Creation settings used by the system test B-20 factory client.
#[derive(Debug, Clone)]
pub struct B20CreateConfig {
    /// ABI-encoded creation params sent to `IB20Factory.createB20`.
    pub encoded_params: Bytes,
    /// Initial supply to mint during the factory init-call window.
    pub initial_supply: U256,
    /// Account receiving the initial supply.
    pub initial_supply_recipient: Address,
    /// Initial supply cap to configure during the factory init-call window.
    pub supply_cap: U256,
    /// Initial ERC-7572 contract URI.
    pub contract_uri: String,
}

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

    /// Sets the receipt timeout used after sending B-20 transactions.
    pub const fn with_receipt_timeout(mut self, receipt_timeout: Duration) -> Self {
        self.receipt_timeout = receipt_timeout;
        self
    }

    /// Builds the required B-20 token params for factory creation.
    pub fn token_params(
        name: &str,
        symbol: &str,
        initial_admin: Address,
        initial_supply: U256,
        initial_supply_recipient: Address,
    ) -> B20CreateConfig {
        B20CreateConfig {
            encoded_params: IB20Factory::B20AssetCreateParams {
                version: B20Variant::Asset.supported_version(),
                name: name.to_string(),
                symbol: symbol.to_string(),
                initialAdmin: initial_admin,
                decimals: 6,
            }
            .abi_encode()
            .into(),
            initial_supply,
            initial_supply_recipient,
            supply_cap: U256::MAX,
            contract_uri: String::new(),
        }
    }

    /// Creates a B-20 token through the factory and returns the deterministic token address.
    pub async fn create_token(
        &self,
        variant: B20Variant,
        params: B20CreateConfig,
        salt: B256,
    ) -> Result<Address> {
        let (token, _) = self.create_token_with_receipt(variant, params, salt).await?;
        Ok(token)
    }

    /// Creates a B-20 token through the factory and returns the token address plus receipt.
    pub async fn create_token_with_receipt(
        &self,
        variant: B20Variant,
        params: B20CreateConfig,
        salt: B256,
    ) -> Result<(Address, BaseTransactionReceipt)> {
        let token = self.predict_token_address(variant, salt);
        let mut init_calls = Vec::new();
        if params.initial_supply > U256::ZERO {
            init_calls.push(
                IB20::mintCall {
                    to: params.initial_supply_recipient,
                    amount: params.initial_supply,
                }
                .abi_encode()
                .into(),
            );
        }
        if params.supply_cap != U256::MAX {
            init_calls.push(
                IB20::updateSupplyCapCall { newSupplyCap: params.supply_cap }.abi_encode().into(),
            );
        }
        if !params.contract_uri.is_empty() {
            init_calls.push(
                IB20::updateContractURICall { newURI: params.contract_uri }.abi_encode().into(),
            );
        }
        let call = IB20Factory::createB20Call {
            variant: variant.abi(),
            salt,
            params: params.encoded_params,
            initCalls: init_calls,
        };
        let receipt =
            self.send_call_receipt(B20FactoryStorage::ADDRESS, call, "create B-20 token").await?;
        Ok((token, receipt))
    }

    /// Activates an activation-registry feature.
    pub async fn activate_feature(&self, feature: B256) -> Result<()> {
        self.send_call(
            ActivationRegistryStorage::ADDRESS,
            IActivationRegistry::activateCall { feature },
            "activate feature",
        )
        .await?;
        Ok(())
    }

    /// Computes the token address a factory creation call will use.
    pub fn predict_token_address(&self, variant: B20Variant, salt: B256) -> Address {
        variant.compute_address(self.signer.address(), salt).0
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
        self.send_call_receipt(to, call, label).await?;
        Ok(())
    }

    /// Signs, sends, and waits for a successful transaction receipt against `to`.
    pub async fn send_call_receipt<C>(
        &self,
        to: Address,
        call: C,
        label: &'static str,
    ) -> Result<BaseTransactionReceipt>
    where
        C: SolCall,
    {
        let receipt = self.send_and_wait(to, Bytes::from(call.abi_encode()), label).await?;
        ensure!(receipt.status(), "{label} transaction reverted");
        ensure!(receipt.inner.to == Some(to), "{label} receipt target mismatch");
        Ok(receipt)
    }

    /// Signs, sends, and polls until a receipt is available.
    ///
    /// All error messages use `label`; callers share this nonce-fetch, sign, send, and receipt
    /// polling pipeline.
    async fn send_and_wait(
        &self,
        to: Address,
        input: Bytes,
        label: &'static str,
    ) -> Result<BaseTransactionReceipt> {
        let nonce = self.provider.get_transaction_count(self.signer.address()).pending().await?;
        let (raw_tx, expected_tx_hash) = self.create_signed_tx(to, nonce, input).wrap_err(label)?;

        let pending_tx = self
            .provider
            .send_raw_transaction(&raw_tx)
            .await
            .wrap_err_with(|| format!("Failed to send {label} transaction"))?;
        let tx_hash = *pending_tx.tx_hash();
        ensure!(tx_hash == expected_tx_hash, "{label} transaction hash mismatch");

        timeout(self.receipt_timeout, async {
            loop {
                if let Some(receipt) = self.provider.get_transaction_receipt(tx_hash).await? {
                    return Ok::<_, eyre::Error>(receipt);
                }
                sleep(Duration::from_secs(1)).await;
            }
        })
        .await
        .wrap_err_with(|| format!("{label} receipt timed out"))?
        .wrap_err_with(|| format!("Failed to get {label} receipt"))
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
