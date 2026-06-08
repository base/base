use std::{
    any::Any,
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use alloy_consensus::{BlockHeader, Transaction};
use alloy_primitives::{Address, B256, U256};
use base_common_chains::Upgrades;
use base_common_consensus::{AccountChange, Eip8130Constants, Eip8130Signed};
use base_common_evm::{BaseSpecId, L1BlockInfo};
use base_common_genesis::DaFootprintGasScalarUpdate;
use parking_lot::RwLock;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_evm::ConfigureEvm;
use reth_primitives_traits::{
    Block, BlockBody, BlockTy, GotExpected, SealedBlock,
    transaction::error::InvalidTransactionError,
};
use reth_storage_api::{AccountInfoReader, BlockReaderIdExt, StateProviderFactory};
use reth_transaction_pool::{
    EthPoolTransaction, EthTransactionValidator, TransactionOrigin, TransactionValidationOutcome,
    TransactionValidator,
    error::{InvalidPoolTransactionError, PoolTransactionError},
};

use crate::BasePooledTx;

/// Base-specific transaction pool validation errors.
#[derive(Debug, thiserror::Error)]
pub enum BaseTxPoolError {
    /// The transaction's DA footprint exceeds the block gas limit.
    #[error(
        "transaction DA footprint ({transaction_da_footprint}) exceeds block gas limit ({block_gas_limit})"
    )]
    DaFootprintExceedsBlockGasLimit {
        /// The computed DA footprint of the transaction (`estimated_da_size` * `da_footprint_gas_scalar`).
        transaction_da_footprint: u64,
        /// The current block gas limit.
        block_gas_limit: u64,
    },
}

impl PoolTransactionError for BaseTxPoolError {
    fn is_bad_transaction(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Tracks additional infos for the current block.
#[derive(Debug, Default)]
pub struct BaseL1BlockInfo {
    /// The current L1 block info.
    l1_block_info: RwLock<L1BlockInfo>,
    /// Current block timestamp.
    timestamp: AtomicU64,
}

impl BaseL1BlockInfo {
    /// Returns the most recent timestamp
    pub fn timestamp(&self) -> u64 {
        self.timestamp.load(Ordering::Relaxed)
    }
}

/// Validator for Base transactions.
#[derive(Debug, Clone)]
pub struct BaseTransactionValidator<Client, Tx, Evm> {
    /// The type that performs the actual validation.
    inner: Arc<EthTransactionValidator<Client, Tx, Evm>>,
    /// Additional block info required for validation.
    block_info: Arc<BaseL1BlockInfo>,
    /// If true, ensure that the transaction's sender has enough balance to cover the L1 gas fee
    /// derived from the tracked L1 block info that is extracted from the first transaction in the
    /// L2 block.
    require_l1_data_gas_fee: bool,
}

impl<Client, Tx, Evm> BaseTransactionValidator<Client, Tx, Evm> {
    /// Returns the configured chain spec
    pub fn chain_spec(&self) -> Arc<Client::ChainSpec>
    where
        Client: ChainSpecProvider,
    {
        self.inner.chain_spec()
    }

    /// Returns the configured client
    pub fn client(&self) -> &Client {
        self.inner.client()
    }

    /// Returns the current block timestamp.
    fn block_timestamp(&self) -> u64 {
        self.block_info.timestamp.load(Ordering::Relaxed)
    }

    /// Whether to ensure that the transaction's sender has enough balance to also cover the L1 gas
    /// fee.
    pub fn require_l1_data_gas_fee(self, require_l1_data_gas_fee: bool) -> Self {
        Self { require_l1_data_gas_fee, ..self }
    }

    /// Returns whether this validator also requires the transaction's sender to have enough balance
    /// to cover the L1 gas fee.
    pub const fn requires_l1_data_gas_fee(&self) -> bool {
        self.require_l1_data_gas_fee
    }
}

impl<Client, Tx, Evm> BaseTransactionValidator<Client, Tx, Evm>
where
    Client: ChainSpecProvider<ChainSpec: Upgrades> + StateProviderFactory + BlockReaderIdExt + Sync,
    Tx: EthPoolTransaction + BasePooledTx,
    Evm: ConfigureEvm,
{
    /// Create a new [`BaseTransactionValidator`].
    pub fn new(inner: EthTransactionValidator<Client, Tx, Evm>) -> Self {
        let this = Self::with_block_info(inner, BaseL1BlockInfo::default());
        if let Ok(Some(block)) =
            this.inner.client().block_by_number_or_tag(alloy_eips::BlockNumberOrTag::Latest)
        {
            // genesis block has no txs, so we can't extract L1 info, we set the block info to empty
            // so that we will accept txs into the pool before the first block
            if block.header().number() == 0 {
                this.block_info.timestamp.store(block.header().timestamp(), Ordering::Relaxed);
            } else {
                this.update_l1_block_info(block.header(), block.body().transactions().first());
            }
        }

        this
    }

    /// Create a new [`BaseTransactionValidator`] with the given [`BaseL1BlockInfo`].
    pub fn with_block_info(
        inner: EthTransactionValidator<Client, Tx, Evm>,
        block_info: BaseL1BlockInfo,
    ) -> Self {
        Self {
            inner: Arc::new(inner),
            block_info: Arc::new(block_info),
            require_l1_data_gas_fee: true,
        }
    }

    /// Update the L1 block info for the given header and system transaction, if any.
    ///
    /// Note: this supports optional system transaction, in case this is used in a dev setup
    pub fn update_l1_block_info<H, T>(&self, header: &H, tx: Option<&T>)
    where
        H: BlockHeader,
        T: Transaction,
    {
        self.block_info.timestamp.store(header.timestamp(), Ordering::Relaxed);

        if let Some(Ok(l1_block_info)) = tx.map(base_execution_evm::extract_l1_info_from_tx) {
            *self.block_info.l1_block_info.write() = l1_block_info;
        }
    }

    /// Validates a single transaction.
    ///
    /// See also [`TransactionValidator::validate_transaction`]
    ///
    /// This behaves the same as [`BaseTransactionValidator::validate_one_with_state`], but creates
    /// a new state provider internally.
    pub async fn validate_one(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
    ) -> TransactionValidationOutcome<Tx> {
        self.validate_one_with_state(origin, transaction, &mut None).await
    }

    /// Validates a single transaction with a provided state provider.
    ///
    /// This allows reusing the same state provider across multiple transaction validations.
    ///
    /// See also [`TransactionValidator::validate_transaction`]
    ///
    /// This behaves the same as [`EthTransactionValidator::validate_one_with_state`], but in
    /// addition applies Base-specific validity checks:
    /// - ensures tx is not eip4844
    /// - for eip8130 (account abstraction): rejects local-origin submissions and enforces the
    ///   structural admission gate from [`Self::validate_eip8130_structural`] before the inner
    ///   Eth checks; verifier dispatch, account state lookups, and fork gating are not yet
    ///   performed
    /// - ensures that the account has enough balance to cover the L1 gas cost
    pub async fn validate_one_with_state(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
        state: &mut Option<Box<dyn AccountInfoReader + Send>>,
    ) -> TransactionValidationOutcome<Tx> {
        if transaction.is_eip4844() {
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidTransactionError::TxTypeNotSupported.into(),
            );
        }

        if let Some(signed) = transaction.as_eip8130()
            && let Err(err) = self.validate_eip8130_structural(origin, signed)
        {
            return TransactionValidationOutcome::Invalid(transaction, err);
        }
        let outcome = self.inner.validate_one_with_state(origin, transaction, state);
        self.apply_base_checks(outcome)
    }

    /// Runs the mempool admission checks that apply to EIP-8130 (account
    /// abstraction) transactions without requiring verifier dispatch, account
    /// state lookups, or fork activation. Mirrors the structural invariants
    /// listed in EIP-8130 § Validation and § Nonce-Free Mode.
    fn validate_eip8130_structural(
        &self,
        origin: TransactionOrigin,
        signed: &Eip8130Signed,
    ) -> Result<(), InvalidPoolTransactionError> {
        if !origin.is_external() {
            return Err(InvalidTransactionError::TxTypeNotSupported.into());
        }
        let local_chain_id = self.inner.chain_spec().chain().id();
        signed.validate_static(local_chain_id).map_err(InvalidPoolTransactionError::from)?;
        // Single read of the head-block timestamp so both branches see the
        // same value even when `on_new_head_block` updates the atomic
        // concurrently.
        let now = self.block_timestamp();
        signed.validate_timestamp(now).map_err(InvalidPoolTransactionError::from)?;
        Self::validate_sender_auth(signed)?;
        Self::validate_payer_auth(signed)?;
        Self::validate_account_changes(signed, local_chain_id)?;
        Ok(())
    }

    /// Checks the `sender_auth` field carries enough bytes for either the EOA
    /// recovery path (65-byte signature) or the configured-owner auth path
    /// (`verifier_address || verifier_payload`) and that the verifier address
    /// is not the sentinel revoked marker.
    fn validate_sender_auth(signed: &Eip8130Signed) -> Result<(), InvalidPoolTransactionError> {
        let auth = signed.sender_auth();
        if auth.is_empty() {
            return Err(InvalidTransactionError::TxTypeNotSupported.into());
        }
        if signed.explicit_sender().is_none() {
            // EOA path: must carry exactly the secp256k1 signature.
            if auth.len() != 65 {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
        } else {
            // Configured-owner path: leading 20 bytes are the verifier address.
            if auth.len() < 20 {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
            let verifier = Address::from_slice(&auth[..20]);
            if Self::verifier_out_of_range(&verifier) {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
        }
        Ok(())
    }

    /// Ensures `payer_auth` is present iff a `payer` is set, and that its
    /// verifier prefix sits in the live policy range (above the reserved
    /// floor, below the revoked sentinel).
    fn validate_payer_auth(signed: &Eip8130Signed) -> Result<(), InvalidPoolTransactionError> {
        let payer_present = signed.tx().payer.is_some();
        let auth = signed.payer_auth();
        // XOR: presence must match.
        if payer_present == auth.is_empty() {
            return Err(InvalidTransactionError::TxTypeNotSupported.into());
        }
        if payer_present {
            if auth.len() < 20 {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
            let verifier = Address::from_slice(&auth[..20]);
            if Self::verifier_out_of_range(&verifier) {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
        }
        Ok(())
    }

    /// Returns `true` when `verifier` falls outside the live mempool policy
    /// range. Mirrors the check in [`Self::validate_owner_iter`] so all three
    /// auth surfaces (`sender_auth`, `payer_auth`, `cfg.auth`, and per-owner
    /// verifiers) reject the reserved `< ECRECOVER_VERIFIER` window and the
    /// `REVOKED_VERIFIER` sentinel identically.
    fn verifier_out_of_range(verifier: &Address) -> bool {
        *verifier < Eip8130Constants::ECRECOVER_VERIFIER
            || *verifier == Eip8130Constants::REVOKED_VERIFIER
    }

    /// Walks `account_changes` and enforces structural invariants:
    /// at most one `Create` (and only as the first entry), at most one
    /// `Delegation`, `ConfigChange` count capped at
    /// [`Eip8130Constants::MAX_CONFIG_CHANGES_PER_TX`], chain-binding on
    /// config changes, and per-entry well-formedness. Verifier-address bounds
    /// and owner-id uniqueness are enforced on both `Create.initial_owners`
    /// and `ConfigChange.owner_changes` via
    /// [`Self::validate_owner_iter`].
    fn validate_account_changes(
        signed: &Eip8130Signed,
        local_chain_id: u64,
    ) -> Result<(), InvalidPoolTransactionError> {
        let mut create_count = 0usize;
        let mut delegation_count = 0usize;
        let mut config_count = 0usize;
        for (idx, change) in signed.tx().account_changes.iter().enumerate() {
            match change {
                AccountChange::Create(create) => {
                    create_count += 1;
                    if create_count > 1 || idx != 0 {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                    if create.code.is_empty() || create.initial_owners.is_empty() {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                    Self::validate_owner_iter(&create.initial_owners, |o| {
                        (&o.verifier, &o.owner_id)
                    })?;
                }
                AccountChange::ConfigChange(cfg) => {
                    config_count += 1;
                    if config_count > Eip8130Constants::MAX_CONFIG_CHANGES_PER_TX {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                    if cfg.chain_id != 0 && cfg.chain_id != local_chain_id {
                        return Err(InvalidTransactionError::ChainIdMismatch.into());
                    }
                    if cfg.auth.len() < 20 {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                    let cfg_verifier = Address::from_slice(&cfg.auth[..20]);
                    if Self::verifier_out_of_range(&cfg_verifier) {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                    Self::validate_owner_iter(&cfg.owner_changes, |o| (&o.verifier, &o.owner_id))?;
                }
                AccountChange::Delegation(_) => {
                    delegation_count += 1;
                    if delegation_count > 1 {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                }
            }
        }
        Ok(())
    }

    /// Shared validation for any owner-bearing slice (`initial_owners` on
    /// `Create`, `owner_changes` on `ConfigChange`). Enforces that the slice
    /// length is bounded by [`Eip8130Constants::MAX_OWNERS_PER_ENTRY`] (anti-DoS
    /// cap on memory + work spent on duplicate detection), that every verifier
    /// address is at or above the `ECRECOVER_VERIFIER` floor, never equals the
    /// `REVOKED_VERIFIER` sentinel, and that no two entries share the same
    /// `owner_id`.
    fn validate_owner_iter<T>(
        owners: &[T],
        project: impl Fn(&T) -> (&Address, &B256),
    ) -> Result<(), InvalidPoolTransactionError> {
        if owners.len() > Eip8130Constants::MAX_OWNERS_PER_ENTRY {
            return Err(InvalidTransactionError::TxTypeNotSupported.into());
        }
        let mut seen = BTreeSet::new();
        for entry in owners {
            let (verifier, owner_id) = project(entry);
            if Self::verifier_out_of_range(verifier) {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
            if !seen.insert(*owner_id) {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
        }
        Ok(())
    }

    /// Performs the necessary Base-specific checks based on top of the regular eth outcome.
    fn apply_base_checks(
        &self,
        outcome: TransactionValidationOutcome<Tx>,
    ) -> TransactionValidationOutcome<Tx> {
        if !self.requires_l1_data_gas_fee() {
            // no need to check L1 gas fee
            return outcome;
        }
        // ensure that the account has enough balance to cover the L1 gas cost
        if let TransactionValidationOutcome::Valid {
            balance,
            state_nonce,
            transaction: valid_tx,
            propagate,
            bytecode_hash,
            authorities,
        } = outcome
        {
            let mut l1_block_info = self.block_info.l1_block_info.read().clone();

            // Check to ensure tx doesn't exceed the DA footprint limit
            if self.chain_spec().is_jovian_active_at_timestamp(self.block_timestamp()) {
                let da_footprint = valid_tx.transaction().estimated_da_size().saturating_mul(
                    l1_block_info
                        .da_footprint_gas_scalar
                        .unwrap_or(DaFootprintGasScalarUpdate::DEFAULT_DA_FOOTPRINT_GAS_SCALAR)
                        as u64,
                );
                let block_gas_limit = self.inner.block_gas_limit();
                if da_footprint > block_gas_limit {
                    return TransactionValidationOutcome::Invalid(
                        valid_tx.into_transaction(),
                        InvalidPoolTransactionError::other(
                            BaseTxPoolError::DaFootprintExceedsBlockGasLimit {
                                transaction_da_footprint: da_footprint,
                                block_gas_limit,
                            },
                        ),
                    );
                }
            }

            let encoded = valid_tx.transaction().encoded_2718();

            // Must mirror the execution-side cost in `BaseHandler` (L1 data fee + operator fee
            // post-Isthmus); otherwise operator-fee-underfunded txs get admitted but never execute.
            let spec_id = BaseSpecId::from_timestamp(self.chain_spec(), self.block_timestamp());
            let cost_addition = l1_block_info.tx_cost(
                &encoded,
                U256::from(valid_tx.transaction().gas_limit()),
                spec_id,
            );
            let cost = valid_tx.transaction().cost().saturating_add(cost_addition);

            // Checks for max cost
            if cost > balance {
                return TransactionValidationOutcome::Invalid(
                    valid_tx.into_transaction(),
                    InvalidTransactionError::InsufficientFunds(
                        GotExpected { got: balance, expected: cost }.into(),
                    )
                    .into(),
                );
            }

            return TransactionValidationOutcome::Valid {
                balance,
                state_nonce,
                transaction: valid_tx,
                propagate,
                bytecode_hash,
                authorities,
            };
        }
        outcome
    }
}

impl<Client, Tx, Evm> TransactionValidator for BaseTransactionValidator<Client, Tx, Evm>
where
    Client: ChainSpecProvider<ChainSpec: Upgrades> + StateProviderFactory + BlockReaderIdExt + Sync,
    Tx: EthPoolTransaction + BasePooledTx,
    Evm: ConfigureEvm,
{
    type Transaction = Tx;
    type Block = BlockTy<Evm::Primitives>;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        self.validate_one(origin, transaction).await
    }

    fn on_new_head_block(&self, new_tip_block: &SealedBlock<Self::Block>) {
        self.inner.on_new_head_block(new_tip_block);
        self.update_l1_block_info(
            new_tip_block.header(),
            new_tip_block.body().transactions().first(),
        );
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{SignableTransaction, TxEip1559, transaction::SignerRecoverable};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Bytes, TxKind, U256, bytes, hex::decode};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        AccountChange, BasePrimitives, BaseTransactionSigned, BaseTxEnvelope, ConfigChange,
        CreateEntry, Delegation, Eip8130Constants, Eip8130Signed, InitialOwner, OwnerChange,
        OwnerChangeType, Scope, TxDeposit, TxEip8130,
    };
    use base_execution_chainspec::BaseChainSpec;
    use base_execution_evm::BaseEvmConfig;
    use base_test_utils::Account;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_transaction_pool::{
        TransactionOrigin, TransactionValidationOutcome, blobstore::InMemoryBlobStore,
        validate::EthTransactionValidatorBuilder,
    };

    use super::*;
    use crate::BasePooledTransaction;

    type TestValidator = BaseTransactionValidator<
        MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>,
        BasePooledTransaction,
        BaseEvmConfig,
    >;

    /// Builds a [`BaseTransactionValidator`] configured against the mainnet chain spec with
    /// no accounts seeded. Suitable for tests that exercise the EIP-8130 structural-acceptance
    /// gate, which rejects without touching account state.
    fn build_test_validator() -> TestValidator {
        let chain_spec = Arc::new(BaseChainSpec::mainnet());
        let client = MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::clone(&chain_spec))
            .with_genesis_block();
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let inner = EthTransactionValidatorBuilder::new(client, evm_config)
            .no_shanghai()
            .no_cancun()
            .build(InMemoryBlobStore::default());
        BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default())
    }

    /// Returns the chain id the [`build_test_validator`] is configured against.
    fn test_chain_id() -> u64 {
        ChainConfig::mainnet().chain_id
    }

    /// Signs `tx` as an EOA-path EIP-8130 transaction and returns the resulting
    /// [`Eip8130Signed`] with a valid 65-byte secp256k1 `sender_auth`.
    fn sign_eoa_eip8130(tx: TxEip8130) -> Eip8130Signed {
        let signer = PrivateKeySigner::random();
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let sig_bytes: Bytes = signature.as_bytes().to_vec().into();
        Eip8130Signed::new(tx, sig_bytes, Bytes::new())
    }

    /// Returns a minimal, structurally valid EOA-path [`TxEip8130`] bound to the
    /// test chain. `sender` is left as `None` so the EOA recovery path is exercised.
    fn minimal_valid_eoa_tx() -> TxEip8130 {
        TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key: U256::ZERO,
            nonce_sequence: 1,
            expiry: 0,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: 1_000,
            gas_limit: 50_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            payer: None,
        }
    }

    /// Helper: assert structural validation returns `Invalid` with `TxTypeNotSupported`.
    #[track_caller]
    fn assert_unsupported(result: Result<(), InvalidPoolTransactionError>) {
        match result {
            Err(InvalidPoolTransactionError::Consensus(
                InvalidTransactionError::TxTypeNotSupported,
            )) => {}
            other => panic!("expected TxTypeNotSupported, got {other:?}"),
        }
    }

    /// Helper: assert structural validation returns `Invalid` with `ChainIdMismatch`.
    #[track_caller]
    fn assert_chain_id_mismatch(result: Result<(), InvalidPoolTransactionError>) {
        match result {
            Err(InvalidPoolTransactionError::Consensus(
                InvalidTransactionError::ChainIdMismatch,
            )) => {}
            other => panic!("expected ChainIdMismatch, got {other:?}"),
        }
    }

    /// Helper: assert structural validation returns `Invalid` with `TipAboveFeeCap`.
    #[track_caller]
    fn assert_tip_above_fee_cap(result: Result<(), InvalidPoolTransactionError>) {
        match result {
            Err(InvalidPoolTransactionError::Consensus(
                InvalidTransactionError::TipAboveFeeCap,
            )) => {}
            other => panic!("expected TipAboveFeeCap, got {other:?}"),
        }
    }

    #[test]
    fn accepts_eip8130_with_minimum_valid_eoa_shape() {
        let validator = build_test_validator();
        let signed = sign_eoa_eip8130(minimal_valid_eoa_tx());
        assert!(
            validator.validate_eip8130_structural(TransactionOrigin::External, &signed).is_ok()
        );
    }

    #[test]
    fn rejects_eip8130_from_local_origin() {
        let validator = build_test_validator();
        let signed = sign_eoa_eip8130(minimal_valid_eoa_tx());
        assert_unsupported(
            validator.validate_eip8130_structural(TransactionOrigin::Local, &signed),
        );
        assert_unsupported(
            validator.validate_eip8130_structural(TransactionOrigin::Private, &signed),
        );
    }

    #[test]
    fn rejects_eip8130_with_wrong_chain_id() {
        let validator = build_test_validator();
        let tx = TxEip8130 { chain_id: test_chain_id() + 1, ..minimal_valid_eoa_tx() };
        let signed = sign_eoa_eip8130(tx);
        assert_chain_id_mismatch(
            validator.validate_eip8130_structural(TransactionOrigin::External, &signed),
        );
    }

    #[test]
    fn rejects_eip8130_with_tip_above_fee_cap() {
        let validator = build_test_validator();
        let tx = TxEip8130 {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 200,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert_tip_above_fee_cap(
            validator.validate_eip8130_structural(TransactionOrigin::External, &signed),
        );
    }

    #[test]
    fn rejects_eip8130_with_zero_gas_limit() {
        let validator = build_test_validator();
        let tx = TxEip8130 { gas_limit: 0, ..minimal_valid_eoa_tx() };
        let signed = sign_eoa_eip8130(tx);
        assert_unsupported(
            validator.validate_eip8130_structural(TransactionOrigin::External, &signed),
        );
    }

    #[test]
    fn rejects_eip8130_with_zero_fee_cap() {
        let validator = build_test_validator();
        let tx = TxEip8130 { max_fee_per_gas: 0, ..minimal_valid_eoa_tx() };
        let signed = sign_eoa_eip8130(tx);
        assert_unsupported(
            validator.validate_eip8130_structural(TransactionOrigin::External, &signed),
        );
    }

    #[test]
    fn rejects_eip8130_nonce_free_without_expiry() {
        let validator = build_test_validator();
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            expiry: 0,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert_unsupported(
            validator.validate_eip8130_structural(TransactionOrigin::External, &signed),
        );
    }

    #[test]
    fn rejects_eip8130_nonce_free_with_nonzero_sequence() {
        let validator = build_test_validator();
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 1,
            expiry: 5,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert_unsupported(
            validator.validate_eip8130_structural(TransactionOrigin::External, &signed),
        );
    }

    #[test]
    fn rejects_eip8130_nonce_free_already_expired() {
        // Advance the validator's tracked block timestamp to 100 so that expiry=50
        // is strictly in the past; the default fixture sits at timestamp 0 where
        // there is no way to express "already expired".
        let validator = build_test_validator();
        let header = alloy_consensus::Header { timestamp: 100, ..Default::default() };
        validator.update_l1_block_info::<_, TxEip1559>(&header, None);
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            expiry: 50,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert_unsupported(
            validator.validate_eip8130_structural(TransactionOrigin::External, &signed),
        );
    }

    #[test]
    fn rejects_eip8130_nonce_free_expiry_too_far_in_future() {
        let validator = build_test_validator();
        // block_timestamp returns 0 by default; cap is NONCE_FREE_MAX_EXPIRY_WINDOW (10).
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            expiry: Eip8130Constants::NONCE_FREE_MAX_EXPIRY_WINDOW + 1,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert_unsupported(
            validator.validate_eip8130_structural(TransactionOrigin::External, &signed),
        );
    }

    #[test]
    fn accepts_eip8130_nonce_free_at_expiry_window_edge() {
        let validator = build_test_validator();
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            expiry: Eip8130Constants::NONCE_FREE_MAX_EXPIRY_WINDOW,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert!(
            validator.validate_eip8130_structural(TransactionOrigin::External, &signed).is_ok()
        );
    }

    #[test]
    fn rejects_eip8130_with_invalid_sender_auth_length_eoa_path() {
        // EOA path requires exactly 65 bytes; anything else is rejected.
        let tx = minimal_valid_eoa_tx();
        let signed = Eip8130Signed::new(tx, Bytes::from_static(&[0u8; 32]), Bytes::new());
        assert_unsupported(TestValidator::validate_sender_auth(&signed));
    }

    #[test]
    fn rejects_eip8130_with_empty_sender_auth() {
        let tx = minimal_valid_eoa_tx();
        let signed = Eip8130Signed::new(tx, Bytes::new(), Bytes::new());
        assert_unsupported(TestValidator::validate_sender_auth(&signed));
    }

    #[test]
    fn rejects_eip8130_configured_owner_with_revoked_verifier() {
        let tx = TxEip8130 { sender: Some(Address::repeat_byte(0xaa)), ..minimal_valid_eoa_tx() };
        // 20-byte verifier prefix == REVOKED_VERIFIER, followed by an empty payload.
        let auth = Bytes::from(Eip8130Constants::REVOKED_VERIFIER.to_vec());
        let signed = Eip8130Signed::new(tx, auth, Bytes::new());
        assert_unsupported(TestValidator::validate_sender_auth(&signed));
    }

    // Regression: configured-owner path must reject the reserved verifier
    // range below `ECRECOVER_VERIFIER`, matching `validate_owner_iter`.
    // `address(0)` is the canonical reserved value.
    #[test]
    fn rejects_eip8130_configured_owner_with_reserved_verifier() {
        let tx = TxEip8130 { sender: Some(Address::repeat_byte(0xaa)), ..minimal_valid_eoa_tx() };
        let auth = Bytes::from(Address::ZERO.to_vec());
        let signed = Eip8130Signed::new(tx, auth, Bytes::new());
        assert_unsupported(TestValidator::validate_sender_auth(&signed));
    }

    #[test]
    fn rejects_eip8130_configured_owner_with_short_auth() {
        let tx = TxEip8130 { sender: Some(Address::repeat_byte(0xaa)), ..minimal_valid_eoa_tx() };
        let signed = Eip8130Signed::new(tx, Bytes::from_static(&[0u8; 5]), Bytes::new());
        assert_unsupported(TestValidator::validate_sender_auth(&signed));
    }

    #[test]
    fn rejects_eip8130_payer_present_without_auth() {
        let tx = TxEip8130 { payer: Some(Address::repeat_byte(0x11)), ..minimal_valid_eoa_tx() };
        let signed = Eip8130Signed::new(tx, Bytes::from_static(&[0u8; 65]), Bytes::new());
        assert_unsupported(TestValidator::validate_payer_auth(&signed));
    }

    #[test]
    fn rejects_eip8130_payer_absent_with_auth() {
        let tx = minimal_valid_eoa_tx();
        let signed =
            Eip8130Signed::new(tx, Bytes::from_static(&[0u8; 65]), Bytes::from_static(&[0u8; 20]));
        assert_unsupported(TestValidator::validate_payer_auth(&signed));
    }

    #[test]
    fn rejects_eip8130_payer_verifier_reserved() {
        let tx = TxEip8130 { payer: Some(Address::repeat_byte(0x11)), ..minimal_valid_eoa_tx() };
        let signed = Eip8130Signed::new(
            tx,
            Bytes::from_static(&[0u8; 65]),
            Bytes::from(Address::ZERO.to_vec()),
        );
        assert_unsupported(TestValidator::validate_payer_auth(&signed));
    }

    #[test]
    fn rejects_eip8130_payer_verifier_revoked() {
        let tx = TxEip8130 { payer: Some(Address::repeat_byte(0x11)), ..minimal_valid_eoa_tx() };
        let signed = Eip8130Signed::new(
            tx,
            Bytes::from_static(&[0u8; 65]),
            Bytes::from(Eip8130Constants::REVOKED_VERIFIER.to_vec()),
        );
        assert_unsupported(TestValidator::validate_payer_auth(&signed));
    }

    /// Returns a verifier address comfortably above the `ECRECOVER_VERIFIER` floor
    /// and distinct from the `REVOKED_VERIFIER` sentinel.
    fn ok_verifier() -> Address {
        Address::repeat_byte(0x42)
    }

    fn make_initial_owner(owner_id_byte: u8) -> InitialOwner {
        InitialOwner {
            verifier: ok_verifier(),
            owner_id: B256::repeat_byte(owner_id_byte),
            scope: Scope::UNRESTRICTED,
        }
    }

    fn make_valid_create_entry() -> CreateEntry {
        CreateEntry {
            user_salt: B256::ZERO,
            code: Bytes::from_static(&[0x60, 0x00]),
            initial_owners: vec![make_initial_owner(0x01)],
        }
    }

    #[test]
    fn rejects_eip8130_create_not_at_index_zero() {
        let tx = TxEip8130 {
            account_changes: vec![
                AccountChange::Delegation(Delegation { target: Address::repeat_byte(0x33) }),
                AccountChange::Create(make_valid_create_entry()),
            ],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_multiple_create_entries() {
        let tx = TxEip8130 {
            account_changes: vec![
                AccountChange::Create(make_valid_create_entry()),
                AccountChange::Create(make_valid_create_entry()),
            ],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_create_with_empty_code() {
        let mut entry = make_valid_create_entry();
        entry.code = Bytes::new();
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(entry)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_create_with_no_initial_owners() {
        let mut entry = make_valid_create_entry();
        entry.initial_owners.clear();
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(entry)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_create_with_duplicate_owner_ids() {
        let mut entry = make_valid_create_entry();
        entry.initial_owners.push(make_initial_owner(0x01));
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(entry)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_create_with_owner_verifier_below_floor() {
        let mut entry = make_valid_create_entry();
        entry.initial_owners[0].verifier = Address::ZERO;
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(entry)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_create_with_revoked_owner_verifier() {
        let mut entry = make_valid_create_entry();
        entry.initial_owners[0].verifier = Eip8130Constants::REVOKED_VERIFIER;
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(entry)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_create_with_too_many_initial_owners() {
        let mut entry = make_valid_create_entry();
        entry.initial_owners.clear();
        for i in 0..(Eip8130Constants::MAX_OWNERS_PER_ENTRY + 1) {
            entry.initial_owners.push(make_initial_owner(i as u8));
        }
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(entry)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn accepts_eip8130_create_with_exactly_max_initial_owners() {
        let mut entry = make_valid_create_entry();
        entry.initial_owners.clear();
        for i in 0..Eip8130Constants::MAX_OWNERS_PER_ENTRY {
            entry.initial_owners.push(make_initial_owner(i as u8));
        }
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(entry)],
            ..minimal_valid_eoa_tx()
        };
        assert!(
            TestValidator::validate_account_changes(&sign_eoa_eip8130(tx), test_chain_id()).is_ok()
        );
    }

    fn make_valid_config_change() -> ConfigChange {
        ConfigChange {
            chain_id: 0,
            sequence: 0,
            owner_changes: Vec::new(),
            auth: Bytes::from(ok_verifier().to_vec()),
        }
    }

    #[test]
    fn rejects_eip8130_config_change_with_foreign_chain_id() {
        let cfg = ConfigChange { chain_id: test_chain_id() + 1, ..make_valid_config_change() };
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::ConfigChange(cfg)],
            ..minimal_valid_eoa_tx()
        };
        let result =
            TestValidator::validate_account_changes(&sign_eoa_eip8130(tx), test_chain_id());
        match result {
            Err(InvalidPoolTransactionError::Consensus(
                InvalidTransactionError::ChainIdMismatch,
            )) => {}
            other => panic!("expected ChainIdMismatch, got {other:?}"),
        }
    }

    #[test]
    fn rejects_eip8130_config_change_with_short_auth() {
        let cfg =
            ConfigChange { auth: Bytes::from_static(&[0u8; 5]), ..make_valid_config_change() };
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::ConfigChange(cfg)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    // Mirrors the `sender_auth`/`payer_auth` rejection of a `REVOKED_VERIFIER`
    // prefix: the per-`ConfigChange` `auth` blob must use a live verifier so the
    // mempool never propagates a config-change scoped to a revoked authority.
    #[test]
    fn rejects_eip8130_config_change_with_revoked_verifier_in_auth() {
        let cfg = ConfigChange {
            auth: Bytes::from(Eip8130Constants::REVOKED_VERIFIER.to_vec()),
            ..make_valid_config_change()
        };
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::ConfigChange(cfg)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_config_change_with_revoked_verifier_in_owner_changes() {
        let cfg = ConfigChange {
            owner_changes: vec![OwnerChange {
                change_type: OwnerChangeType::Revoke,
                verifier: Eip8130Constants::REVOKED_VERIFIER,
                owner_id: B256::repeat_byte(0x01),
                scope: Scope::UNRESTRICTED,
            }],
            ..make_valid_config_change()
        };
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::ConfigChange(cfg)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_config_change_with_duplicate_owner_ids() {
        let dup_id = B256::repeat_byte(0x07);
        let mk = |id| OwnerChange {
            change_type: OwnerChangeType::Authorize,
            verifier: ok_verifier(),
            owner_id: id,
            scope: Scope::UNRESTRICTED,
        };
        let cfg = ConfigChange {
            owner_changes: vec![mk(dup_id), mk(dup_id)],
            ..make_valid_config_change()
        };
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::ConfigChange(cfg)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_config_change_with_too_many_owner_changes() {
        let owner_changes = (0..(Eip8130Constants::MAX_OWNERS_PER_ENTRY + 1))
            .map(|i| OwnerChange {
                change_type: OwnerChangeType::Authorize,
                verifier: ok_verifier(),
                owner_id: B256::repeat_byte(i as u8),
                scope: Scope::UNRESTRICTED,
            })
            .collect();
        let cfg = ConfigChange { owner_changes, ..make_valid_config_change() };
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::ConfigChange(cfg)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_too_many_config_changes() {
        let count = Eip8130Constants::MAX_CONFIG_CHANGES_PER_TX + 1;
        let account_changes =
            (0..count).map(|_| AccountChange::ConfigChange(make_valid_config_change())).collect();
        let tx = TxEip8130 { account_changes, ..minimal_valid_eoa_tx() };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn accepts_eip8130_with_exactly_max_config_changes() {
        let count = Eip8130Constants::MAX_CONFIG_CHANGES_PER_TX;
        let account_changes =
            (0..count).map(|_| AccountChange::ConfigChange(make_valid_config_change())).collect();
        let tx = TxEip8130 { account_changes, ..minimal_valid_eoa_tx() };
        assert!(
            TestValidator::validate_account_changes(&sign_eoa_eip8130(tx), test_chain_id(),)
                .is_ok()
        );
    }

    #[test]
    fn rejects_eip8130_multiple_delegations() {
        let tx = TxEip8130 {
            account_changes: vec![
                AccountChange::Delegation(Delegation { target: Address::repeat_byte(0x11) }),
                AccountChange::Delegation(Delegation { target: Address::repeat_byte(0x22) }),
            ],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn accepts_eip8130_with_create_followed_by_delegation_and_configs() {
        let mut account_changes = vec![AccountChange::Create(make_valid_create_entry())];
        for _ in 0..3 {
            account_changes.push(AccountChange::ConfigChange(make_valid_config_change()));
        }
        account_changes
            .push(AccountChange::Delegation(Delegation { target: Address::repeat_byte(0x55) }));
        let tx = TxEip8130 { account_changes, ..minimal_valid_eoa_tx() };
        assert!(
            TestValidator::validate_account_changes(&sign_eoa_eip8130(tx), test_chain_id(),)
                .is_ok()
        );
    }

    /// L1 attribute deposit calldata that activates Isthmus and seeds a non-zero
    /// `operator_fee_scalar`/`operator_fee_constant`. Mirrors the fixture used by
    /// `parse_l1_info_isthmus` in `crates/execution/evm/src/l1.rs`.
    const ISTHMUS_L1_INFO_DATA_HEX: &str = concat!(
        "098999be00000558000c5fc500000000000000030000000067a9f765",
        "0000000000000029000000000000000000000000000000000000000000000000",
        "00000000006a6d090000000000000000000000000000000000000000000000000000000000000001",
        "72fcc8e8886636bdbe96ba0e4baab67ea7e7811633f52b52e8cf7a5123213b6f",
        "000000000000000000000000d3f2c5afb2d76f5579f326b0cd7da5f5a4126c35",
        "00004e2000000000000001f4",
    );

    /// Regression test for `HackerOne` #74725.
    ///
    /// Asserts that the txpool affordability check accounts for the post-Isthmus operator fee, so a
    /// sender funded only for `tx.cost + l1_data_fee` (but not the additional operator fee) is
    /// rejected at admission instead of being accepted and later failing during execution with
    /// `LackOfFundForMaxFee`.
    #[tokio::test]
    async fn rejects_tx_underfunded_for_operator_fee_post_isthmus() {
        let chain_config = ChainConfig::mainnet();
        let chain_spec = Arc::new(BaseChainSpec::mainnet());

        let signer = Account::Alice.signer();
        let sender = signer.address();
        let tx = TxEip1559 {
            chain_id: chain_config.chain_id,
            nonce: 0,
            gas_limit: 50_000,
            max_fee_per_gas: 1_000,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(Address::random()),
            value: U256::ZERO,
            access_list: Default::default(),
            input: bytes!("FACADE"),
        };
        let gas_limit = tx.gas_limit;
        let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
        let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.clone().try_into_recovered().unwrap();
        let encoded = recovered_tx.encoded_2718();

        let isthmus_data = decode(ISTHMUS_L1_INFO_DATA_HEX).expect("valid hex fixture");
        let mut l1_block_info = base_execution_evm::parse_l1_info(&isthmus_data).unwrap();
        let l1_only_cost = base_execution_evm::RethL1BlockInfo::l1_tx_data_fee(
            &mut l1_block_info,
            Arc::clone(&chain_spec),
            chain_config.isthmus_timestamp,
            &encoded,
            false,
        )
        .unwrap();
        let full_additional_cost = l1_block_info.tx_cost(
            &encoded,
            U256::from(gas_limit),
            BaseSpecId::from_timestamp(Arc::clone(&chain_spec), chain_config.isthmus_timestamp),
        );
        let base_tx_cost = U256::from(envelope.value()).saturating_add(U256::from(
            envelope.max_fee_per_gas().saturating_mul(envelope.gas_limit() as u128),
        ));
        let balance = base_tx_cost.saturating_add(l1_only_cost);

        assert!(
            full_additional_cost > l1_only_cost,
            "fixture must produce a non-zero operator fee post-Isthmus"
        );
        assert!(
            base_tx_cost.saturating_add(full_additional_cost) > balance,
            "balance must be insufficient once the operator fee is included"
        );

        let client = MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::clone(&chain_spec))
            .with_genesis_block();
        client.add_account(sender, ExtendedAccount::new(0, balance));
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let inner = EthTransactionValidatorBuilder::new(client, evm_config)
            .no_shanghai()
            .no_cancun()
            .build(InMemoryBlobStore::default());
        let validator =
            BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default());

        let header = alloy_consensus::Header {
            timestamp: chain_config.isthmus_timestamp,
            ..Default::default()
        };
        let l1_info_tx: BaseTransactionSigned = TxDeposit {
            source_hash: Default::default(),
            from: Address::ZERO,
            to: TxKind::Create,
            mint: 0,
            value: U256::ZERO,
            gas_limit: 0,
            is_system_transaction: false,
            input: isthmus_data.into(),
        }
        .into();
        validator.update_l1_block_info(&header, Some(&l1_info_tx));

        let pooled_tx: BasePooledTransaction =
            BasePooledTransaction::new(recovered_tx, envelope.encode_2718_len());
        let outcome = validator.validate_one(TransactionOrigin::External, pooled_tx).await;

        match outcome {
            TransactionValidationOutcome::Invalid(_, err) => {
                assert!(
                    matches!(
                        err,
                        InvalidPoolTransactionError::Consensus(
                            InvalidTransactionError::InsufficientFunds(_)
                        )
                    ),
                    "expected InsufficientFunds, got: {err:?}"
                );
            }
            other => panic!(
                "expected operator-fee-underfunded tx to be rejected at admission, got {other:?}"
            ),
        }
    }
}
