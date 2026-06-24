//! Reth compatibility implementations for RPC types.

use core::convert::Infallible;

use alloc::vec;

use alloy_consensus::{SignableTransaction, error::ValueError};
use alloy_evm::{
    EvmEnv, FromRecoveredTx,
    env::BlockEnvironment,
    rpc::{EthTxEnvError, TryIntoTxEnv},
};
use alloy_network::TxSigner;
use alloy_primitives::{Address, Bytes, U256};
use alloy_signer::Signature;
use base_common_consensus::{BaseTransactionInfo, BaseTxEnvelope, Eip8130Signed, TxEip8130};
use base_common_evm::{BaseTransaction as BaseRevm, Eip8130ExecutionMode};
use reth_rpc_convert::{FromConsensusTx, SignTxRequestError, SignableTxRequest, TryIntoSimTx};
use revm::context::TxEnv;

use crate::{BaseTransactionRequest, Transaction};

/// Length of the stub secp256k1 `sender_auth` blob used for EIP-8130 estimation.
/// Matches the bare-signature (default-EOA) wire form so the intrinsic-gas
/// schedule prices k1 authentication; the bytes are never recovered.
const STUB_K1_SENDER_AUTH_LEN: usize = 65;

impl BaseTransactionRequest {
    /// Builds the unsigned simulation transaction for an EIP-8130
    /// `eth_estimateGas` / `eth_call` request, or `None` when the request
    /// carries no EIP-8130 fields or omits the required `from` sender.
    ///
    /// Estimation runs without a signature: a stub 65-byte k1 `sender_auth` blob
    /// stands in for the bare-signature EOA wire form so the intrinsic schedule
    /// prices authentication gas from its shape. The blob is never recovered —
    /// [`base_common_evm::Eip8130Executor::simulate`] simulates from `from`
    /// without verification. `gas_limit_cap` bounds execution when the request
    /// omits `gas`.
    ///
    /// `from` is mandatory for EIP-8130: the sender identity drives actor
    /// resolution, policy lookup, and auto-delegation, so a missing `from`
    /// returns `None` (surfaced as `INVALID_PARAMS`) rather than silently
    /// falling back to the zero address.
    pub fn to_eip8130_simulation_tx(
        &self,
        chain_id: u64,
        gas_limit_cap: u64,
    ) -> Option<BaseRevm<TxEnv>> {
        let aa = self.as_eip8130()?;
        let req = self.as_ref();
        let from = req.from?;

        let tx = TxEip8130 {
            chain_id,
            // Default-EOA path: the sender is `from`, resolved from the request
            // rather than recovered from the (absent) signature.
            sender: None,
            nonce_key: aa.nonce_key.unwrap_or(U256::ZERO),
            nonce_sequence: 0,
            expiry: aa.expiry.unwrap_or_default(),
            max_priority_fee_per_gas: req.max_priority_fee_per_gas.unwrap_or_default(),
            max_fee_per_gas: req.max_fee_per_gas.unwrap_or_default(),
            gas_limit: req.gas.unwrap_or(gas_limit_cap),
            account_changes: aa.account_changes.clone().unwrap_or_default(),
            calls: aa.calls.clone().unwrap_or_default(),
            metadata: aa.metadata.clone().unwrap_or_default(),
            payer: None,
        };

        let stub_auth = Bytes::from(vec![0u8; STUB_K1_SENDER_AUTH_LEN]);
        let envelope = BaseTxEnvelope::Eip8130(Eip8130Signed::new(tx, stub_auth, Bytes::new()));
        let mut sim_tx = BaseRevm::from_recovered_tx(&envelope, from);
        // Route to the unverified `Eip8130Executor::simulate` path rather than
        // the verifying `execute` path.
        if let Some(parts) = sim_tx.eip8130.as_mut() {
            parts.mode = Eip8130ExecutionMode::Simulate;
        }
        Some(sim_tx)
    }
}

impl FromConsensusTx<BaseTxEnvelope> for Transaction {
    type TxInfo = BaseTransactionInfo;
    type Err = Infallible;

    fn from_consensus_tx(
        tx: BaseTxEnvelope,
        signer: Address,
        tx_info: BaseTransactionInfo,
    ) -> Result<Self, Infallible> {
        Ok(Self::from_transaction(
            alloy_consensus::transaction::Recovered::new_unchecked(tx, signer),
            tx_info,
        ))
    }
}

impl<Spec, Block: BlockEnvironment> TryIntoTxEnv<BaseRevm<TxEnv>, Spec, Block>
    for BaseTransactionRequest
{
    type Err = EthTxEnvError;

    fn try_into_tx_env(self, evm_env: &EvmEnv<Spec, Block>) -> Result<BaseRevm<TxEnv>, Self::Err> {
        Ok(BaseRevm {
            base: self.as_ref().clone().try_into_tx_env(evm_env)?,
            enveloped_tx: Some(Bytes::new()),
            deposit: Default::default(),
            eip8130: None,
        })
    }
}

impl TryIntoSimTx<BaseTxEnvelope> for BaseTransactionRequest {
    fn try_into_sim_tx(self) -> Result<BaseTxEnvelope, ValueError<Self>> {
        let tx = self
            .build_typed_tx()
            .map_err(|request| ValueError::new(request, "Required fields missing"))?;

        // Create an empty signature for the transaction.
        let signature = Signature::new(Default::default(), Default::default(), false);

        Ok(tx.into_signed(signature).into())
    }
}

impl SignableTxRequest<BaseTxEnvelope> for BaseTransactionRequest {
    async fn try_build_and_sign(
        self,
        signer: impl TxSigner<Signature> + Send,
    ) -> Result<BaseTxEnvelope, SignTxRequestError> {
        let mut tx =
            self.build_typed_tx().map_err(|_| SignTxRequestError::InvalidTransactionRequest)?;

        // sanity check: deposit transactions must not be signed by the user
        if tx.is_deposit() {
            return Err(SignTxRequestError::InvalidTransactionRequest);
        }

        let signature = signer.sign_transaction(&mut tx).await?;

        Ok(tx.into_signed(signature).into())
    }
}
