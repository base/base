//! Type-erased transaction handler registry with typed handlers.
//!
//! Handlers are written against concrete transaction types. The registry stores
//! them behind an object-safe boundary and dispatches by transaction type byte.
//!
//! The registry is generic over an [`EvmTypesHost`] family and handler output, so
//! it does not force a particular transaction or receipt representation onto
//! the rest of the crate.

use alloc::sync::Arc;
use core::{error::Error, fmt, marker::PhantomData};

use alloy_consensus::transaction::Recovered;
use alloy_primitives::{Address, U256, map::HashMap};
use thiserror::Error;

use crate::{AnyError, ErrorCode, EvmTypesHost};

/// Convenience result type used by the registry and handlers.
pub type HandlerResult<T> = core::result::Result<T, HandlerError>;

/// Registry, transaction validation, and transaction handler errors.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum HandlerError {
    /// Host error propagated as a transaction handler failure.
    #[error("fatal error {0:?}")]
    Fatal(ErrorCode),
    /// Typed error supplied by a custom transaction handler.
    #[error(transparent)]
    External(AnyError),
    /// No handler is registered for the transaction type byte.
    #[error("unsupported transaction type 0x{0:02x}")]
    UnsupportedTransactionType(u8),
    /// A registered handler's extractor did not match the provided envelope.
    #[error("envelope did not contain expected transaction type 0x{expected:02x}")]
    WrongTransactionType {
        /// Expected transaction type byte.
        expected: u8,
    },
    /// Sender account does not have the expected nonce.
    #[error("invalid nonce: expected {expected}, got {got}")]
    InvalidNonce {
        /// Expected nonce.
        expected: u64,
        /// Transaction nonce.
        got: u64,
    },
    /// Transaction chain ID does not match the active chain.
    #[error("invalid chain id: expected {expected}, got {got}")]
    InvalidChainId {
        /// Active chain ID.
        expected: u64,
        /// Transaction chain ID.
        got: u64,
    },
    /// Transaction chain ID is required.
    #[error("missing chain id")]
    MissingChainId,
    /// Transaction gas limit is lower than intrinsic gas.
    #[error("intrinsic gas too low: required {required}, got {got}")]
    IntrinsicGasTooLow {
        /// Required intrinsic gas.
        required: u64,
        /// Transaction gas limit.
        got: u64,
    },
    /// Sender cannot pay value plus maximum gas cost.
    #[error("insufficient funds")]
    InsufficientFunds,
    /// Sender account has deployed code.
    #[error("caller has code")]
    RejectCallerWithCode,
    /// Transaction nonce cannot be incremented.
    #[error("nonce overflow in transaction")]
    NonceOverflow,
    /// Transaction gas limit exceeds the block gas limit.
    #[error("transaction gas limit {gas_limit} exceeds block gas limit {block_gas_limit}")]
    GasLimitMoreThanBlock {
        /// Transaction gas limit.
        gas_limit: u64,
        /// Block gas limit.
        block_gas_limit: U256,
    },
    /// Transaction gas limit exceeds the active per-transaction gas cap.
    #[error("transaction gas limit {gas_limit} exceeds cap {cap}")]
    TxGasLimitGreaterThanCap {
        /// Transaction gas limit.
        gas_limit: u64,
        /// Active transaction gas limit cap.
        cap: u64,
    },
    /// Create transaction initcode exceeds the active size limit.
    #[error("create initcode size limit exceeded: limit {limit}, got {got}")]
    CreateInitCodeSizeLimit {
        /// Maximum initcode size.
        limit: usize,
        /// Transaction initcode size.
        got: usize,
    },
    /// Sender could not transfer transaction value to the target.
    #[error("out of funds")]
    OutOfFunds,
    /// Signature recovery failed.
    #[error("could not recover signer")]
    SignerRecoveryFailed,
    /// Fee cap is lower than the block base fee.
    #[error("fee cap less than base fee: max_fee_per_gas {max_fee_per_gas}, base_fee {base_fee}")]
    FeeCapLessThanBaseFee {
        /// Maximum fee per gas.
        max_fee_per_gas: U256,
        /// Block base fee.
        base_fee: U256,
    },
    /// EIP-7702 authorization list is empty.
    #[error("EIP-7702 authorization list is empty")]
    EmptyAuthorizationList,
    /// EIP-4844 blob fee cap is lower than the block blob base fee.
    #[error(
        "blob fee cap less than blob base fee: max_fee_per_blob_gas {max_fee_per_blob_gas}, blob_base_fee {blob_base_fee}"
    )]
    BlobFeeCapLessThanBlobBaseFee {
        /// Maximum fee per blob gas.
        max_fee_per_blob_gas: U256,
        /// Block blob base fee.
        blob_base_fee: U256,
    },
    /// EIP-4844 blob transaction contains no blob hashes.
    #[error("empty blobs")]
    EmptyBlobs,
    /// EIP-4844 blob transaction contains too many blob hashes.
    #[error("too many blobs: have {have}, max {max}")]
    TooManyBlobs {
        /// Blob count in the transaction.
        have: usize,
        /// Maximum allowed blob count.
        max: usize,
    },
    /// EIP-4844 blob transaction contains an unsupported versioned hash.
    #[error("blob version not supported")]
    BlobVersionNotSupported,
    /// Priority fee is greater than max fee.
    #[error("priority fee greater than max fee")]
    PriorityFeeGreaterThanMaxFee,
    /// Unsupported caller for this handler.
    #[error("unsupported caller {0}")]
    UnsupportedCaller(Address),
}

impl HandlerError {
    /// Wraps a typed custom transaction handler error.
    pub fn external(error: impl Error + Send + Sync + 'static) -> Self {
        Self::External(AnyError::new(error))
    }

    /// Returns the typed custom error when it has type `E`.
    pub fn external_ref<E: Error + 'static>(&self) -> Option<&E> {
        match self {
            Self::External(error) => error.downcast_ref(),
            _ => None,
        }
    }
}

/// Request passed to a typed transaction handler.
#[derive(Debug)]
pub struct TxRequest<'a, 'host, T: EvmTypesHost, Tx> {
    /// Full transaction envelope passed to the registry.
    pub envelope: &'a T::Tx,
    /// Concrete transaction extracted from the envelope.
    pub tx: Recovered<&'a Tx>,
    /// Mutable host used by this handler.
    pub host: &'a mut T::Host<'host>,
    #[doc(hidden)] // Not public API. Please use an existing constructor.
    pub _non_exhaustive: (),
}

/// A typed transaction handler.
///
/// `Tx` remains concrete. This is what gives handlers strong type guarantees
/// even though the registry itself is type-erased.
pub trait TxHandler<T: EvmTypesHost, Tx, Output> {
    /// Executes the handler.
    fn call(&self, req: TxRequest<'_, '_, T, Tx>) -> HandlerResult<Output>;
}

impl<T: EvmTypesHost, Tx, Output, F> TxHandler<T, Tx, Output> for F
where
    F: for<'a, 'host> Fn(TxRequest<'a, 'host, T, Tx>) -> HandlerResult<Output>,
{
    fn call(&self, req: TxRequest<'_, '_, T, Tx>) -> HandlerResult<Output> {
        self(req)
    }
}

/// An erased transaction handler returned by [`TxRegistry`].
#[derive(Clone)]
pub struct AnyTxHandler<T: EvmTypesHost, Output> {
    inner: Arc<dyn ErasedTxHandler<T, Output>>,
}

impl<T: EvmTypesHost, Output> fmt::Debug for AnyTxHandler<T, Output> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnyTxHandler").finish_non_exhaustive()
    }
}

impl<T: EvmTypesHost, Output> AnyTxHandler<T, Output> {
    /// Executes the erased handler against an envelope and host.
    pub fn call<'host>(
        &self,
        env: &Recovered<T::Tx>,
        host: &mut T::Host<'host>,
    ) -> HandlerResult<Output> {
        self.inner.call(env, host)
    }
}

trait ErasedTxHandler<T: EvmTypesHost, Output>: Send + Sync {
    fn call<'host>(
        &self,
        env: &Recovered<T::Tx>,
        host: &mut T::Host<'host>,
    ) -> HandlerResult<Output>;
}

struct HandlerAdapter<Tx, H, F> {
    type_id: u8,
    handler: H,
    extract: F,
    _tx: PhantomData<fn() -> Tx>,
}

impl<Tx, H, F> HandlerAdapter<Tx, H, F> {
    const fn new(type_id: u8, extract: F, handler: H) -> Self {
        Self { type_id, handler, extract, _tx: PhantomData }
    }
}

impl<T, Tx, Output, H, F> ErasedTxHandler<T, Output> for HandlerAdapter<Tx, H, F>
where
    T: EvmTypesHost,
    H: TxHandler<T, Tx, Output> + Send + Sync,
    F: for<'a> Fn(&'a T::Tx) -> Option<&'a Tx> + Send + Sync,
{
    fn call<'host>(
        &self,
        env: &Recovered<T::Tx>,
        host: &mut T::Host<'host>,
    ) -> HandlerResult<Output> {
        let tx = (self.extract)(env.inner())
            .ok_or(HandlerError::WrongTransactionType { expected: self.type_id })?;
        self.handler.call(TxRequest {
            envelope: env.inner(),
            tx: Recovered::new_unchecked(tx, env.signer()),
            host,
            _non_exhaustive: (),
        })
    }
}

/// A type-erased transaction handler registry keyed by transaction type byte.
pub struct TxRegistry<T: EvmTypesHost, Output = ()> {
    handlers: HashMap<u8, Arc<dyn ErasedTxHandler<T, Output>>>,
}

impl<T: EvmTypesHost, Output> fmt::Debug for TxRegistry<T, Output> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TxRegistry").field("len", &self.handlers.len()).finish_non_exhaustive()
    }
}

impl<T: EvmTypesHost, Output> Default for TxRegistry<T, Output> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: EvmTypesHost, Output> TxRegistry<T, Output> {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self { handlers: HashMap::default() }
    }

    /// Registers a typed handler for a transaction type byte.
    ///
    /// `extract` projects `Tx` out of the envelope. The handler remains typed
    /// as `TxHandler<T, Tx, Output>`; only this registry boundary is erased.
    pub fn register<Tx, H, F>(&mut self, type_id: u8, extract: F, handler: H) -> &mut Self
    where
        Tx: 'static,
        H: TxHandler<T, Tx, Output> + Send + Sync + 'static,
        F: for<'a> Fn(&'a T::Tx) -> Option<&'a Tx> + Send + Sync + 'static,
    {
        self.handlers.insert(type_id, Arc::new(HandlerAdapter::new(type_id, extract, handler)));
        self
    }

    /// Adds a typed handler and returns the registry.
    #[must_use]
    pub fn with_handler<Tx, H, F>(mut self, type_id: u8, extract: F, handler: H) -> Self
    where
        Tx: 'static,
        H: TxHandler<T, Tx, Output> + Send + Sync + 'static,
        F: for<'a> Fn(&'a T::Tx) -> Option<&'a Tx> + Send + Sync + 'static,
    {
        self.register(type_id, extract, handler);
        self
    }

    /// Returns true if a handler is registered for `type_id`.
    pub fn contains(&self, type_id: u8) -> bool {
        self.handlers.contains_key(&type_id)
    }

    /// Returns the erased handler registered for `type_id`, if any.
    pub fn get_by_type(&self, type_id: u8) -> Option<AnyTxHandler<T, Output>> {
        self.handlers.get(&type_id).map(|inner| AnyTxHandler { inner: Arc::clone(inner) })
    }

    /// Returns the erased handler registered for `type_id`.
    pub fn try_get_by_type(&self, type_id: u8) -> HandlerResult<AnyTxHandler<T, Output>> {
        self.get_by_type(type_id).ok_or(HandlerError::UnsupportedTransactionType(type_id))
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_primitives::{Address, B256, Log};

    use super::*;
    use crate::{
        BaseEvmConfigSelector, EvmFeatures, EvmTypesHost, SpecId,
        env::{BlockEnv, BlockEnvExt, TxEnv},
        evm::{AccountLoad, SLoad, SStore, SelfDestructResult},
        interpreter::{Host, InstrStop, Message, MessageResult, Word},
    };

    #[derive(Debug, thiserror::Error)]
    #[error("typed handler error")]
    struct TestHandlerError;

    #[test]
    fn preserves_typed_external_errors() {
        let error = HandlerError::external(TestHandlerError);
        assert!(error.external_ref::<TestHandlerError>().is_some());
    }

    #[derive(Clone, Debug)]
    struct TransferTx {
        amount: u64,
    }

    #[derive(Clone, Debug)]
    struct CreateTx {
        initcode: Vec<u8>,
    }

    #[derive(Clone, Debug)]
    enum Envelope {
        Transfer(TransferTx),
        Create(CreateTx),
    }

    struct TestTypes;

    impl EvmTypesHost for TestTypes {
        type ConfigSelector = BaseEvmConfigSelector;
        type SpecId = SpecId;
        type Tx = Envelope;
        type EvmExt = ();
        type MessageExt = ();
        type MessageResultExt = ();
        type TxEnvExt = ();
        type TxResultExt = ();
        type BlockEnvExt = ();
        type Host<'a> = TestHost;
    }

    struct TestHost {
        block: BlockEnv<TestTypes>,
    }

    impl Host<TestTypes> for TestHost {
        fn spec_id(&self) -> SpecId {
            SpecId::default()
        }

        fn block_env(&mut self) -> &BlockEnv<TestTypes> {
            &self.block
        }

        fn load_account(
            &mut self,
            _address: &Address,
            _load_code: bool,
            _skip_cold_load: bool,
        ) -> Result<AccountLoad, InstrStop> {
            unimplemented!()
        }

        fn target_is_empty_for_new_account_gas(
            &mut self,
            _address: &Address,
            _features: EvmFeatures,
        ) -> Result<bool, InstrStop> {
            unimplemented!()
        }

        fn block_hash(&mut self, _number: &Word) -> Result<B256, InstrStop> {
            unimplemented!()
        }

        fn sload(
            &mut self,
            _address: &Address,
            _key: &Word,
            _skip_cold_load: bool,
        ) -> Result<SLoad, InstrStop> {
            unimplemented!()
        }

        fn sstore(
            &mut self,
            _address: &Address,
            _key: &Word,
            _value: &Word,
            _skip_cold_load: bool,
        ) -> Result<SStore, InstrStop> {
            unimplemented!()
        }

        fn tload(&mut self, _address: &Address, _key: &Word) -> Word {
            unimplemented!()
        }

        fn tstore(&mut self, _address: &Address, _key: &Word, _value: &Word) {
            unimplemented!()
        }

        fn log(&mut self, _log: Log) {
            unimplemented!()
        }

        fn execute_message(
            &mut self,
            _tx_env: &TxEnv<TestTypes>,
            _message: &mut Message<TestTypes>,
        ) -> MessageResult<TestTypes> {
            unimplemented!()
        }

        fn selfdestruct(
            &mut self,
            _contract: &Address,
            _target: &Address,
            _skip_cold_load: bool,
        ) -> Result<SelfDestructResult, InstrStop> {
            unimplemented!()
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Receipt {
        success: bool,
        cumulative_gas_used: u64,
    }

    fn transfer(env: &Envelope) -> Option<&TransferTx> {
        match env {
            Envelope::Transfer(tx) => Some(tx),
            Envelope::Create(_) => None,
        }
    }

    fn create(env: &Envelope) -> Option<&CreateTx> {
        match env {
            Envelope::Create(tx) => Some(tx),
            Envelope::Transfer(_) => None,
        }
    }

    fn receipt(cumulative_gas_used: u64) -> Receipt {
        Receipt { success: true, cumulative_gas_used }
    }

    fn handle_transfer(req: TxRequest<'_, '_, TestTypes, TransferTx>) -> HandlerResult<Receipt> {
        assert!(matches!(req.envelope, Envelope::Transfer(_)));
        let gas_used = 21_000 + req.tx.amount;
        Ok(receipt(gas_used))
    }

    fn handle_create(req: TxRequest<'_, '_, TestTypes, CreateTx>) -> HandlerResult<Receipt> {
        let gas_used = 53_000 + req.tx.initcode.len() as u64;
        Ok(receipt(gas_used))
    }

    fn call_registered(
        registry: &TxRegistry<TestTypes, Receipt>,
        type_id: u8,
        env: &Envelope,
    ) -> HandlerResult<Receipt> {
        registry.try_get_by_type(type_id)?.call(
            &Recovered::new_unchecked(env.clone(), Address::ZERO),
            &mut TestHost { block: BlockEnvExt::default() },
        )
    }

    #[test]
    fn dispatches_to_typed_handlers_from_erased_registry() {
        let mut registry = TxRegistry::<TestTypes, Receipt>::new();
        registry.register(0x01, transfer, handle_transfer);
        registry.register(0x02, create, handle_create);

        let transfer_receipt =
            call_registered(&registry, 0x01, &Envelope::Transfer(TransferTx { amount: 7 }))
                .expect("transfer handler is registered");
        assert_eq!(transfer_receipt, receipt(21_007));

        let create_receipt =
            call_registered(&registry, 0x02, &Envelope::Create(CreateTx { initcode: Vec::new() }))
                .expect("create handler is registered");
        assert_eq!(create_receipt, receipt(53_000));
    }

    #[test]
    fn reports_unsupported_and_mismatched_types() {
        let mut registry = TxRegistry::<TestTypes, Receipt>::new();
        registry.register(0x01, transfer, handle_transfer);

        assert_eq!(
            call_registered(&registry, 0xff, &Envelope::Transfer(TransferTx { amount: 7 })),
            Err(HandlerError::UnsupportedTransactionType(0xff))
        );
        assert_eq!(
            call_registered(&registry, 0x01, &Envelope::Create(CreateTx { initcode: Vec::new() })),
            Err(HandlerError::WrongTransactionType { expected: 0x01 })
        );
    }
}
