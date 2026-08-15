//! Low-cardinality metadata for observing Beryl-native precompile calls.

use alloc::{borrow::Cow, string::ToString};
#[cfg(feature = "std")]
use std::time::Instant;

use alloy_primitives::Bytes;
use alloy_sol_types::{SolCall, SolError};
use base_precompile_storage::{BasePrecompileError, Result, StorageCtx};
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};

use crate::{IActivationRegistry, IB20, IB20Asset, IB20Factory, IB20Stablecoin, IPolicyRegistry};

/// Low-cardinality metadata for one Beryl-native precompile call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrecompileCallMetric {
    /// Beryl precompile surface, such as `factory`, `activation`, `policy`, or `b20`.
    pub precompile: &'static str,
    /// ABI method name or `unknown`.
    pub method: Cow<'static, str>,
    /// Optional variant label. Dynamic B-20 calls use `asset` or `stablecoin`.
    pub variant: Option<&'static str>,
    /// Calldata byte length.
    pub input_bytes: usize,
}

impl PrecompileCallMetric {
    /// Creates a call metric descriptor.
    pub fn new(
        precompile: &'static str,
        method: impl Into<Cow<'static, str>>,
        variant: Option<&'static str>,
        input_bytes: usize,
    ) -> Self {
        Self { precompile, method: method.into(), variant, input_bytes }
    }

    /// Creates a call metric descriptor without a variant.
    pub fn singleton(
        precompile: &'static str,
        method: impl Into<Cow<'static, str>>,
        input_bytes: usize,
    ) -> Self {
        Self::new(precompile, method, None, input_bytes)
    }

    /// Creates a call metric descriptor for a dynamic B-20 token call.
    pub fn b20(
        variant: &'static str,
        method: impl Into<Cow<'static, str>>,
        input_bytes: usize,
    ) -> Self {
        Self::new("b20", method, Some(variant), input_bytes)
    }

    /// Returns the variant label used by metrics backends for calls without a variant.
    pub fn variant_label(&self) -> &'static str {
        self.variant.unwrap_or("none")
    }
}

/// Bounded terminal status for a native precompile call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrecompileCallStatus {
    /// The call completed successfully.
    Success,
    /// The call returned an EVM revert.
    Revert,
    /// The call halted without returning an EVM revert.
    Halt,
    /// The call returned a fatal precompile error.
    Fatal,
}

impl PrecompileCallStatus {
    /// Returns the metric label for this status.
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Revert => "revert",
            Self::Halt => "halt",
            Self::Fatal => "fatal",
        }
    }

    /// Classifies a [`PrecompileResult`] into a bounded status.
    pub const fn from_result(result: &PrecompileResult) -> Self {
        match result {
            Ok(output) if output.is_success() => Self::Success,
            Ok(output) if output.is_revert() => Self::Revert,
            Ok(_) => Self::Halt,
            Err(_) => Self::Fatal,
        }
    }
}

/// Backwards-compatible alias for Beryl call status labels.
pub type BerylCallOutcome = PrecompileCallStatus;

/// Final outcome for one Beryl-native precompile call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PrecompileCallOutcome {
    /// Terminal call status.
    pub status: PrecompileCallStatus,
    /// Regular gas used by the precompile.
    pub gas_used: u64,
    /// State gas used by the precompile.
    pub state_gas_used: u64,
    /// Gas refunded by the precompile.
    pub gas_refunded: i64,
    /// Total time spent in the Beryl dispatch wrapper, in seconds.
    pub duration_seconds: Option<f64>,
    /// Optional bounded error class.
    pub error: Option<BerylErrorKind>,
}

impl PrecompileCallOutcome {
    /// Creates a call outcome from a [`PrecompileResult`].
    pub fn from_result(
        result: &PrecompileResult,
        duration_seconds: Option<f64>,
        error: Option<BerylErrorKind>,
    ) -> Self {
        match result {
            Ok(output) => Self::from_output(output, duration_seconds, error),
            Err(error) => Self {
                status: PrecompileCallStatus::Fatal,
                gas_used: 0,
                state_gas_used: 0,
                gas_refunded: 0,
                duration_seconds,
                error: Some(BerylErrorKind::from_precompile_error(error)),
            },
        }
    }

    /// Creates a call outcome from a successful, reverted, or halted precompile output.
    pub fn from_output(
        output: &PrecompileOutput,
        duration_seconds: Option<f64>,
        mut error: Option<BerylErrorKind>,
    ) -> Self {
        let status = if output.is_success() {
            PrecompileCallStatus::Success
        } else if output.is_revert() {
            if error.is_none() {
                error = Some(BerylErrorKind::from_revert_bytes(&output.bytes));
            }
            PrecompileCallStatus::Revert
        } else {
            PrecompileCallStatus::Halt
        };

        Self {
            status,
            gas_used: output.gas_used,
            state_gas_used: output.state_gas_used,
            gas_refunded: output.gas_refunded,
            duration_seconds,
            error,
        }
    }
}

/// Bounded error-class label for precompile failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BerylErrorKind {
    /// State mutation was attempted during a static call.
    StaticWrite,
    /// Calldata did not contain a known selector.
    UnknownSelector,
    /// Calldata used a known selector but failed ABI decoding.
    AbiDecode,
    /// The feature was inactive in the activation registry.
    FeatureInactive,
    /// The caller did not have the required permission.
    Unauthorized,
    /// A policy denied the attempted operation.
    PolicyDenied,
    /// A referenced policy does not exist.
    PolicyMissing,
    /// The token feature was paused.
    Paused,
    /// Factory creation targeted an already-created token address.
    DuplicateCreate,
    /// Inputs were invalid.
    InvalidInput,
    /// An internal call failed.
    InternalCallFailed,
    /// The precompile ran out of gas.
    OutOfGas,
    /// The precompile returned a Solidity panic.
    Panic,
    /// The precompile hit an unrecoverable internal error.
    Fatal,
    /// The precompile hit a storage-slot overflow.
    SlotOverflow,
    /// The revert did not match a more specific bounded class.
    OtherRevert,
}

impl BerylErrorKind {
    /// Returns the metric label for this error kind.
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::StaticWrite => "static_write",
            Self::UnknownSelector => "unknown_selector",
            Self::AbiDecode => "abi_decode",
            Self::FeatureInactive => "feature_inactive",
            Self::Unauthorized => "unauthorized",
            Self::PolicyDenied => "policy_denied",
            Self::PolicyMissing => "policy_missing",
            Self::Paused => "paused",
            Self::DuplicateCreate => "duplicate_create",
            Self::InvalidInput => "invalid_input",
            Self::InternalCallFailed => "internal_call_failed",
            Self::OutOfGas => "out_of_gas",
            Self::Panic => "panic",
            Self::Fatal => "fatal",
            Self::SlotOverflow => "slot_overflow",
            Self::OtherRevert => "other_revert",
        }
    }

    /// Classifies a Base precompile error into a bounded metric label.
    pub fn from_base_error(error: &BasePrecompileError) -> Self {
        match error {
            BasePrecompileError::StaticCallViolation => Self::StaticWrite,
            BasePrecompileError::UnknownFunctionSelector(_) => Self::UnknownSelector,
            BasePrecompileError::AbiDecodeFailed { .. } => Self::AbiDecode,
            BasePrecompileError::OutOfGas => Self::OutOfGas,
            BasePrecompileError::Panic(_) => Self::Panic,
            BasePrecompileError::Fatal(_) => Self::Fatal,
            BasePrecompileError::SlotOverflow => Self::SlotOverflow,
            BasePrecompileError::Revert(bytes) => Self::from_revert_bytes(bytes),
        }
    }

    /// Classifies a raw precompile error into a bounded metric label.
    pub const fn from_precompile_error(error: &PrecompileError) -> Self {
        match error {
            PrecompileError::Fatal(_) | PrecompileError::FatalAny(_) => Self::Fatal,
        }
    }

    /// Classifies ABI-encoded revert bytes into a bounded metric label.
    pub fn from_revert_bytes(bytes: &Bytes) -> Self {
        let Some(selector) = BerylSelector::selector(bytes.as_ref()) else {
            return Self::OtherRevert;
        };

        if BerylErrorClassifier::is_error_selector::<IActivationRegistry::StaticCallNotAllowed>(
            selector,
        ) {
            return Self::StaticWrite;
        }
        if BerylErrorClassifier::is_error_selector::<IActivationRegistry::FeatureNotActivated>(
            selector,
        ) {
            return Self::FeatureInactive;
        }
        if BerylErrorClassifier::is_error_selector::<IActivationRegistry::Unauthorized>(selector)
            || BerylErrorClassifier::is_error_selector::<IPolicyRegistry::Unauthorized>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::Unauthorized>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::AccessControlUnauthorizedAccount>(
                selector,
            )
        {
            return Self::Unauthorized;
        }
        if BerylErrorClassifier::is_error_selector::<IB20::PolicyForbids>(selector) {
            return Self::PolicyDenied;
        }
        if BerylErrorClassifier::is_error_selector::<IPolicyRegistry::PolicyNotFound>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::PolicyNotFound>(selector)
        {
            return Self::PolicyMissing;
        }
        if BerylErrorClassifier::is_error_selector::<IB20::ContractPaused>(selector) {
            return Self::Paused;
        }
        if BerylErrorClassifier::is_error_selector::<IB20Factory::TokenAlreadyExists>(selector) {
            return Self::DuplicateCreate;
        }
        if BerylErrorClassifier::is_error_selector::<IB20Factory::InitCallFailed>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20Asset::InternalCallFailed>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20Asset::InternalCallMalformed>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20Asset::AnnouncementInProgress>(
                selector,
            )
        {
            return Self::InternalCallFailed;
        }
        if BerylErrorClassifier::is_error_selector::<IB20Factory::InvalidVariant>(selector)
            || BerylErrorClassifier::is_error_selector::<IActivationRegistry::AdminStorageNotEnabled>(
                selector,
            )
            || BerylErrorClassifier::is_error_selector::<IActivationRegistry::ZeroAdminAddress>(
                selector,
            )
            || BerylErrorClassifier::is_error_selector::<IB20Factory::UnsupportedVersion>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20Factory::MissingRequiredField>(
                selector,
            )
            || BerylErrorClassifier::is_error_selector::<IB20Factory::InvalidCurrency>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20Factory::InvalidDecimals>(selector)
            || BerylErrorClassifier::is_error_selector::<IPolicyRegistry::IncompatiblePolicyType>(
                selector,
            )
            || BerylErrorClassifier::is_error_selector::<IPolicyRegistry::ZeroAddress>(selector)
            || BerylErrorClassifier::is_error_selector::<IPolicyRegistry::BatchSizeTooLarge>(
                selector,
            )
            || BerylErrorClassifier::is_error_selector::<IPolicyRegistry::NoPendingAdmin>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20Asset::AnnouncementIdAlreadyUsed>(
                selector,
            )
            || BerylErrorClassifier::is_error_selector::<IB20Asset::InvalidMetadataKey>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20Asset::LengthMismatch>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20Asset::EmptyBatch>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::InvalidSender>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::InvalidReceiver>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::InvalidApprover>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::InvalidSpender>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::InvalidAmount>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::EmptyFeatureSet>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::InvalidSupplyCap>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::SupplyCapExceeded>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::InsufficientAllowance>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::InsufficientBalance>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::AccountNotBlocked>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::ExpiredSignature>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::InvalidSigner>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::LastAdminCannotRenounce>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::NotSoleAdmin>(selector)
            || BerylErrorClassifier::is_error_selector::<IB20::AccessControlBadConfirmation>(
                selector,
            )
            || BerylErrorClassifier::is_error_selector::<IB20::UnsupportedPolicyType>(selector)
        {
            return Self::InvalidInput;
        }

        Self::OtherRevert
    }
}

/// Helpers for extracting ABI selectors from encoded data.
#[derive(Debug, Clone, Copy)]
pub struct BerylSelector;

impl BerylSelector {
    /// Returns the ABI selector from encoded data, if present.
    pub fn selector(bytes: &[u8]) -> Option<[u8; 4]> {
        let bytes = bytes.get(..4)?;
        let mut selector = [0u8; 4];
        selector.copy_from_slice(bytes);
        Some(selector)
    }
}

/// Helpers for working with ABI error selectors.
#[derive(Debug, Clone, Copy)]
pub struct BerylErrorClassifier;

impl BerylErrorClassifier {
    /// Returns whether `selector` belongs to the ABI error type `E`.
    pub fn is_error_selector<E>(selector: [u8; 4]) -> bool
    where
        E: SolError,
    {
        selector == E::SELECTOR
    }
}

/// Method-label helpers for Beryl call observation.
#[derive(Debug, Clone, Copy)]
pub struct BerylMetricLabels;

impl BerylMetricLabels {
    /// Returns a B-20 method label from an existing stable call label.
    pub fn b20_method(label: &'static str) -> Cow<'static, str> {
        Cow::Borrowed(
            label
                .strip_prefix("precompile-b20-asset-")
                .or_else(|| label.strip_prefix("precompile-b20-stablecoin-"))
                .or_else(|| label.strip_prefix("precompile-b20-"))
                .unwrap_or(label),
        )
    }

    /// Returns the metric method label for an unknown method.
    pub const fn unknown() -> Cow<'static, str> {
        Cow::Borrowed("unknown")
    }

    /// Returns call metadata for factory calldata.
    pub fn factory_call(calldata: &[u8]) -> PrecompileCallMetric {
        PrecompileCallMetric::singleton("factory", Self::factory_method(calldata), calldata.len())
    }

    /// Returns the metric method label for factory calldata.
    pub fn factory_method(calldata: &[u8]) -> Cow<'static, str> {
        match BerylSelector::selector(calldata) {
            Some(IB20Factory::createB20Call::SELECTOR) => Cow::Borrowed("createB20"),
            Some(IB20Factory::getB20AddressCall::SELECTOR) => Cow::Borrowed("getB20Address"),
            Some(IB20Factory::isB20Call::SELECTOR) => Cow::Borrowed("isB20"),
            Some(IB20Factory::isB20InitializedCall::SELECTOR) => Cow::Borrowed("isB20Initialized"),
            _ => Self::unknown(),
        }
    }

    /// Returns call metadata for activation-registry calldata.
    pub fn activation_call(calldata: &[u8]) -> PrecompileCallMetric {
        PrecompileCallMetric::singleton(
            "activation",
            Self::activation_method(calldata),
            calldata.len(),
        )
    }

    /// Returns the metric method label for activation-registry calldata.
    pub fn activation_method(calldata: &[u8]) -> Cow<'static, str> {
        match BerylSelector::selector(calldata) {
            Some(IActivationRegistry::isActivatedCall::SELECTOR) => Cow::Borrowed("isActivated"),
            Some(IActivationRegistry::checkActivatedCall::SELECTOR) => {
                Cow::Borrowed("checkActivated")
            }
            Some(IActivationRegistry::adminCall::SELECTOR) => Cow::Borrowed("admin"),
            Some(IActivationRegistry::setAdminCall::SELECTOR) => Cow::Borrowed("setAdmin"),
            Some(IActivationRegistry::activateCall::SELECTOR) => Cow::Borrowed("activate"),
            Some(IActivationRegistry::deactivateCall::SELECTOR) => Cow::Borrowed("deactivate"),
            _ => Self::unknown(),
        }
    }

    /// Returns call metadata for policy-registry calldata.
    pub fn policy_call(calldata: &[u8]) -> PrecompileCallMetric {
        PrecompileCallMetric::singleton("policy", Self::policy_method(calldata), calldata.len())
    }

    /// Returns the metric method label for policy-registry calldata.
    pub fn policy_method(calldata: &[u8]) -> Cow<'static, str> {
        match BerylSelector::selector(calldata) {
            Some(IPolicyRegistry::createPolicyCall::SELECTOR) => Cow::Borrowed("createPolicy"),
            Some(IPolicyRegistry::createPolicyWithAccountsCall::SELECTOR) => {
                Cow::Borrowed("createPolicyWithAccounts")
            }
            Some(IPolicyRegistry::stageUpdateAdminCall::SELECTOR) => {
                Cow::Borrowed("stageUpdateAdmin")
            }
            Some(IPolicyRegistry::finalizeUpdateAdminCall::SELECTOR) => {
                Cow::Borrowed("finalizeUpdateAdmin")
            }
            Some(IPolicyRegistry::renounceAdminCall::SELECTOR) => Cow::Borrowed("renounceAdmin"),
            Some(IPolicyRegistry::updateAllowlistCall::SELECTOR) => {
                Cow::Borrowed("updateAllowlist")
            }
            Some(IPolicyRegistry::updateBlocklistCall::SELECTOR) => {
                Cow::Borrowed("updateBlocklist")
            }
            Some(IPolicyRegistry::isAuthorizedCall::SELECTOR) => Cow::Borrowed("isAuthorized"),
            Some(IPolicyRegistry::policyExistsCall::SELECTOR) => Cow::Borrowed("policyExists"),
            Some(IPolicyRegistry::policyAdminCall::SELECTOR) => Cow::Borrowed("policyAdmin"),
            Some(IPolicyRegistry::pendingPolicyAdminCall::SELECTOR) => {
                Cow::Borrowed("pendingPolicyAdmin")
            }
            _ => Self::unknown(),
        }
    }

    /// Returns call metadata for asset B-20 calldata.
    pub fn b20_asset_call(calldata: &[u8]) -> PrecompileCallMetric {
        PrecompileCallMetric::b20("asset", Self::b20_asset_method(calldata), calldata.len())
    }

    /// Returns the metric method label for asset B-20 calldata.
    pub fn b20_asset_method(calldata: &[u8]) -> Cow<'static, str> {
        let Some(selector) = BerylSelector::selector(calldata) else {
            return Self::unknown();
        };
        if let Some(method) = IB20Asset::IB20AssetCalls::name_by_selector(selector) {
            return Cow::Owned(method.to_string());
        }
        if let Some(method) = IB20::IB20Calls::name_by_selector(selector) {
            return Cow::Owned(method.to_string());
        }
        Self::unknown()
    }

    /// Returns call metadata for stablecoin B-20 calldata.
    pub fn b20_stablecoin_call(calldata: &[u8]) -> PrecompileCallMetric {
        PrecompileCallMetric::b20(
            "stablecoin",
            Self::b20_stablecoin_method(calldata),
            calldata.len(),
        )
    }

    /// Returns the metric method label for stablecoin B-20 calldata.
    pub fn b20_stablecoin_method(calldata: &[u8]) -> Cow<'static, str> {
        let Some(selector) = BerylSelector::selector(calldata) else {
            return Self::unknown();
        };
        if let Some(method) = IB20Stablecoin::IB20StablecoinCalls::name_by_selector(selector) {
            return Cow::Owned(method.to_string());
        }
        if let Some(method) = IB20::IB20Calls::name_by_selector(selector) {
            return Cow::Owned(method.to_string());
        }
        Self::unknown()
    }
}

/// Call timer used by Beryl call recorders.
#[derive(Debug)]
pub struct BerylCallTimer {
    #[cfg(feature = "std")]
    start: Instant,
}

impl BerylCallTimer {
    /// Starts a new call timer.
    #[cfg(feature = "std")]
    pub fn start() -> Self {
        Self { start: Instant::now() }
    }

    /// Starts a new no-op call timer.
    #[cfg(not(feature = "std"))]
    pub const fn start() -> Self {
        Self {}
    }

    /// Returns elapsed wall-clock time in seconds when std timing is available.
    #[cfg(feature = "std")]
    pub fn elapsed_seconds(&self) -> Option<f64> {
        Some(self.start.elapsed().as_secs_f64())
    }

    /// Returns no elapsed wall-clock time when std timing is unavailable.
    #[cfg(not(feature = "std"))]
    pub const fn elapsed_seconds(&self) -> Option<f64> {
        None
    }
}

/// Per-word calldata ingestion cost charged by Beryl native precompile dispatchers.
///
/// Emulates the cost a Solidity predeploy would incur reading its calldata:
/// `G_copy` (3 gas/word) + `G_memory` (3 gas/word) = 6 gas/word.
/// Part of the receipts/gas-used commitment: must be identical across all Base execution clients.
pub const CALLDATA_WORD_GAS: u64 = 6;

/// Per-call recorder for Beryl precompile observations.
#[derive(Debug)]
pub struct BerylCallRecorder<O> {
    observer: O,
    timer: BerylCallTimer,
    call: PrecompileCallMetric,
    error: Option<BerylErrorKind>,
}

impl<O> BerylCallRecorder<O>
where
    O: crate::PrecompileCallObserver,
{
    /// Starts a recorder for a precompile call.
    #[cfg(feature = "std")]
    pub fn start(observer: O, call: PrecompileCallMetric) -> Self {
        Self { observer, timer: BerylCallTimer::start(), call, error: None }
    }

    /// Starts a recorder for a precompile call.
    #[cfg(not(feature = "std"))]
    pub const fn start(observer: O, call: PrecompileCallMetric) -> Self {
        Self { observer, timer: BerylCallTimer::start(), call, error: None }
    }

    /// Updates the current call metadata.
    pub fn set_call(&mut self, call: PrecompileCallMetric) {
        self.call = call;
    }

    /// Returns the current call metadata.
    pub const fn call(&self) -> &PrecompileCallMetric {
        &self.call
    }

    /// Computes the calldata gas cost for the given calldata slice.
    pub const fn calldata_gas_cost(calldata: &[u8]) -> u64 {
        (calldata.len() as u64).div_ceil(32).saturating_mul(CALLDATA_WORD_GAS)
    }

    /// Deducts the common calldata gas charged by Beryl precompile dispatch.
    pub fn deduct_calldata_gas(&self, ctx: StorageCtx<'_>, calldata: &[u8]) -> Result<()> {
        ctx.deduct_gas(Self::calldata_gas_cost(calldata))
    }

    /// Records a Base precompile error before it is converted to a [`PrecompileResult`].
    pub fn record_base_error(&mut self, error: &BasePrecompileError) {
        self.error = Some(BerylErrorKind::from_base_error(error));
    }

    /// Records the final result of the precompile call.
    pub fn record_result(&mut self, result: &PrecompileResult) {
        let outcome =
            PrecompileCallOutcome::from_result(result, self.timer.elapsed_seconds(), self.error);
        self.observer.record_call(&self.call, &outcome);
    }

    /// Converts and records a Base precompile result.
    pub fn record_base_result<T>(
        &mut self,
        ctx: StorageCtx<'_>,
        result: Result<T>,
        encode_ok: impl FnOnce(T) -> Bytes,
    ) -> PrecompileResult {
        if let Err(error) = &result {
            self.record_base_error(error);
        }
        let result = ctx.result_output(result, encode_ok);
        self.record_result(&result);
        result
    }

    /// Converts and records a Base precompile error.
    pub fn record_base_error_result(
        &mut self,
        ctx: StorageCtx<'_>,
        error: BasePrecompileError,
    ) -> PrecompileResult {
        self.record_base_error(&error);
        let result = error.into_precompile_result(ctx.gas_used(), ctx.state_gas_used());
        self.record_result(&result);
        result
    }
}

/// Helper methods for observer-only Beryl metric families.
#[derive(Debug, Clone, Copy)]
pub struct BerylAuxiliaryMetrics;

impl BerylAuxiliaryMetrics {
    /// Creates a singleton call descriptor for auxiliary metric recording.
    pub fn singleton(precompile: &'static str, method: &'static str) -> PrecompileCallMetric {
        PrecompileCallMetric::singleton(precompile, method, 0)
    }

    /// Creates a B-20 call descriptor for auxiliary metric recording.
    pub fn b20(variant: &'static str, method: &'static str) -> PrecompileCallMetric {
        PrecompileCallMetric::b20(variant, method, 0)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256};
    use alloy_sol_types::{SolCall, SolError};
    use base_precompile_storage::BasePrecompileError;

    use crate::{
        BerylCallOutcome, BerylCallRecorder, BerylErrorKind, BerylMetricLabels, BerylSelector,
        CALLDATA_WORD_GAS, IActivationRegistry, IB20, IB20Factory, IPolicyRegistry,
        NoopPrecompileCallObserver,
    };

    #[test]
    fn b20_method_labels_strip_precompile_prefixes() {
        assert_eq!(BerylMetricLabels::b20_method("precompile-b20-transfer"), "transfer");
        assert_eq!(BerylMetricLabels::b20_method("precompile-b20-stablecoin-currency"), "currency");
    }

    #[test]
    fn selector_method_labels_are_stable() {
        let factory = IB20Factory::getB20AddressCall {
            variant: IB20Factory::B20Variant::ASSET,
            sender: Address::ZERO,
            salt: B256::ZERO,
        }
        .abi_encode();
        assert_eq!(BerylMetricLabels::factory_method(&factory), "getB20Address");

        let b20 = IB20::transferCall { to: Address::ZERO, amount: U256::ZERO }.abi_encode();
        assert_eq!(BerylMetricLabels::b20_asset_method(&b20), "transfer");
    }

    #[test]
    fn base_errors_are_classified() {
        assert_eq!(
            BerylErrorKind::from_base_error(&BasePrecompileError::UnknownFunctionSelector([0; 4])),
            BerylErrorKind::UnknownSelector
        );
        assert_eq!(
            BerylErrorKind::from_base_error(&BasePrecompileError::StaticCallViolation),
            BerylErrorKind::StaticWrite
        );
    }

    #[test]
    fn revert_bytes_are_classified() {
        let unauthorized = IB20::Unauthorized {}.abi_encode().into();
        assert_eq!(BerylErrorKind::from_revert_bytes(&unauthorized), BerylErrorKind::Unauthorized);

        let policy_not_found = IPolicyRegistry::PolicyNotFound {}.abi_encode().into();
        assert_eq!(
            BerylErrorKind::from_revert_bytes(&policy_not_found),
            BerylErrorKind::PolicyMissing
        );

        let admin_storage_disabled =
            IActivationRegistry::AdminStorageNotEnabled {}.abi_encode().into();
        assert_eq!(
            BerylErrorKind::from_revert_bytes(&admin_storage_disabled),
            BerylErrorKind::InvalidInput
        );

        let zero_admin = IActivationRegistry::ZeroAdminAddress {}.abi_encode().into();
        assert_eq!(BerylErrorKind::from_revert_bytes(&zero_admin), BerylErrorKind::InvalidInput);
    }

    #[test]
    fn selector_extracts_revert_selector() {
        let bytes = IB20::Unauthorized {}.abi_encode();
        assert_eq!(
            BerylSelector::selector(&bytes),
            Some(<IB20::Unauthorized as SolError>::SELECTOR)
        );
    }

    #[test]
    fn result_outcomes_are_classified() {
        let success = Ok(revm::precompile::PrecompileOutput::new(1, Default::default(), 0));
        let revert = Ok(revm::precompile::PrecompileOutput::revert(1, Default::default(), 0));
        let fatal = Err(revm::precompile::PrecompileError::Fatal("boom".into()));

        assert_eq!(BerylCallOutcome::from_result(&success), BerylCallOutcome::Success);
        assert_eq!(BerylCallOutcome::from_result(&revert), BerylCallOutcome::Revert);
        assert_eq!(BerylCallOutcome::from_result(&fatal), BerylCallOutcome::Fatal);
    }

    #[test]
    fn deduct_calldata_cost_gas_formula() {
        type Recorder = BerylCallRecorder<NoopPrecompileCallObserver>;

        // 32 bytes = 1 word => CALLDATA_WORD_GAS
        assert_eq!(Recorder::calldata_gas_cost(&[0u8; 32]), CALLDATA_WORD_GAS);

        // 36 bytes (4-byte selector + 32-byte arg) = ceil(36/32) = 2 words => 2 * CALLDATA_WORD_GAS
        assert_eq!(Recorder::calldata_gas_cost(&[0u8; 36]), 2 * CALLDATA_WORD_GAS);
    }
}
