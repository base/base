//! Versioned resource-metering schedules and their transaction-cost evaluator.
//!
//! Resource metering reweights named observations into independent resource-unit
//! dimensions. Simulated `meterBundle` data is a candidate pre-filter.
//! Committed payload usage is accounted from executed observations when they
//! exist: actual gas used and net post-state effects such as
//! [`ResourceSample::STATE_NEW_STORAGE_SLOT`] replace simulated `STATE_*`
//! rows, while other simulated opcode and precompile rows are kept.
//! Production execution does not attach opcode bags. Throttling excludes a
//! transaction when an enforced dimension exceeds a budget. A dimension with
//! [`ResourceMeteringDimension::dry_run`] set is observed without excluding.
//! Block-scope excludes skip only the current payload scan. Transaction-scope
//! excludes (simulated or executed) are permanent pool evictions: the
//! transaction cannot fit any block. Neither changes protocol gas, fees, or
//! validity.

use std::{collections::HashMap, fmt, fs, path::Path};

use alloy_primitives::{Address, TxHash};
use base_bundles::{MeterBundleResponse, OpcodeGas};
use revm::state::EvmState;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const CURRENT_SCHEDULE_VERSION: u32 = 1;
const MAX_DIMENSIONS: usize = 128;
const MAX_DIMENSION_NAME_LENGTH: usize = 64;
const MAX_OPERATIONS_PER_DIMENSION: usize = 512;
const MAX_OPERATION_NAME_LENGTH: usize = 128;

/// A resource-metering schedule used by the builder.
///
/// JSON files are parsed through a private DTO. Omitted `transactionLimit`
/// becomes [`ResourceMeteringDimension::block_limit`] during
/// [`Self::compile`]. The inverted operation index is rebuilt in that same
/// step and is not part of the file format.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResourceMeteringSchedule {
    /// Schedule schema version.
    pub version: u32,
    /// Independently budgeted resource dimensions.
    pub dimensions: Vec<ResourceMeteringDimension>,
    #[serde(skip)]
    operation_index: HashMap<String, Vec<(usize, u64, u64)>>,
}

impl Default for ResourceMeteringSchedule {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// One resource-metering dimension.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResourceMeteringDimension {
    /// Stable operator-facing dimension name.
    pub name: String,
    /// Cumulative resource-unit budget for a block.
    pub block_limit: u64,
    /// Resource-unit budget for one transaction.
    ///
    /// Omitted JSON `transactionLimit` becomes [`Self::block_limit`] during
    /// parse/compile. Serializing a live schedule always writes this field.
    pub transaction_limit: u64,
    /// Resource units charged per unit of actual transaction gas used.
    pub base_gas_weight: u64,
    /// Additional prices for measured opcodes, precompiles, and pseudo-opcodes.
    pub operations: Vec<ResourceMeteringOperation>,
    /// Observe over-budget usage without excluding the transaction.
    ///
    /// Limits take effect unless this flag is set. Dry-run is the explicit
    /// opt-out used while rolling out a dimension.
    pub dry_run: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResourceMeteringScheduleFile {
    version: u32,
    #[serde(default)]
    dimensions: Vec<ResourceMeteringDimensionFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResourceMeteringDimensionFile {
    name: String,
    block_limit: u64,
    #[serde(default)]
    transaction_limit: Option<u64>,
    #[serde(default)]
    base_gas_weight: u64,
    #[serde(default)]
    operations: Vec<ResourceMeteringOperation>,
    #[serde(default)]
    dry_run: bool,
}

/// A price applied to one measured operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceMeteringOperation {
    /// Opcode, precompile, or pseudo-opcode name.
    pub name: String,
    /// Resource units charged per measured gas unit.
    #[serde(default)]
    pub gas_used_weight: u64,
    /// Resource units charged per execution/count occurrence.
    #[serde(default)]
    pub count_cost: u64,
}

/// Resource-unit usage aligned with a schedule's dimensions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceMeteringUsage {
    /// Resource units used by each schedule dimension.
    pub values: Vec<u128>,
}

impl ResourceMeteringUsage {
    /// Creates zero usage for a schedule with `dimension_count` dimensions.
    pub fn zero(dimension_count: usize) -> Self {
        Self { values: vec![0; dimension_count] }
    }

    /// Returns the usage for one dimension, treating missing values as zero.
    pub fn get(&self, index: usize) -> u128 {
        self.values.get(index).copied().unwrap_or_default()
    }

    /// Returns whether this usage can be added to `cumulative` without overflow.
    pub fn fits_in(&self, cumulative: &[u128]) -> Result<(), ResourceMeteringError> {
        for (index, value) in self.values.iter().copied().enumerate() {
            let current = cumulative.get(index).copied().unwrap_or_default();
            current.checked_add(value).ok_or(ResourceMeteringError::ArithmeticOverflow)?;
        }
        Ok(())
    }

    /// Adds this usage to cumulative block usage.
    ///
    /// The write is atomic: overflow leaves `cumulative` unchanged.
    pub fn add_to(&self, cumulative: &mut Vec<u128>) -> Result<(), ResourceMeteringError> {
        let mut next = cumulative.clone();
        if next.len() < self.values.len() {
            next.resize(self.values.len(), 0);
        }
        for (index, value) in self.values.iter().copied().enumerate() {
            next[index] =
                next[index].checked_add(value).ok_or(ResourceMeteringError::ArithmeticOverflow)?;
        }
        *cumulative = next;
        Ok(())
    }
}

/// Named resource observations for one transaction.
///
/// `gas_used` and net post-state effects are taken from builder execution when
/// that result is available. Opcode and precompile counts stay on the simulated
/// `meterBundle` bag. Production execution does not attach opcode bags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceSample {
    /// Transaction gas used for `baseGasWeight`.
    pub gas_used: u64,
    /// Opcode, precompile, pseudo-opcode, and post-state effect observations.
    pub operations: Vec<OpcodeGas>,
}

impl ResourceSample {
    /// Zero-to-nonzero storage transitions observed in post-EVM account state.
    ///
    /// Duplicate writes to one fresh slot count once. This is not `SSTORE`.
    pub const STATE_NEW_STORAGE_SLOT: &'static str = "STATE_NEW_STORAGE_SLOT";

    /// Storage slots whose present value differs from the original value.
    ///
    /// Includes new slots, in-place updates, and clears. Loaded but unwritten
    /// slots are omitted. This is a superset of
    /// [`Self::STATE_NEW_STORAGE_SLOT`] and [`Self::STATE_CLEARED_STORAGE_SLOT`].
    pub const STATE_CHANGED_STORAGE_SLOT: &'static str = "STATE_CHANGED_STORAGE_SLOT";

    /// Nonzero-to-zero storage transitions observed in post-EVM account state.
    ///
    /// Duplicate writes to one cleared slot count once. This is not `SSTORE`.
    pub const STATE_CLEARED_STORAGE_SLOT: &'static str = "STATE_CLEARED_STORAGE_SLOT";

    /// Accounts marked touched in post-EVM state.
    ///
    /// This is not `state.len()`: loaded but unwritten accounts are omitted.
    pub const STATE_TOUCHED_ACCOUNT: &'static str = "STATE_TOUCHED_ACCOUNT";

    /// Accounts whose balance, nonce, or code changed from the original info.
    ///
    /// revm 42 `Account::is_changed()` compares `AccountInfo` only. Storage-only
    /// writes do not set it; those are [`Self::STATE_CHANGED_STORAGE_SLOT`],
    /// [`Self::STATE_NEW_STORAGE_SLOT`], and
    /// [`Self::STATE_CLEARED_STORAGE_SLOT`]. Touch without an info change is
    /// [`Self::STATE_TOUCHED_ACCOUNT`].
    pub const STATE_CHANGED_ACCOUNT: &'static str = "STATE_CHANGED_ACCOUNT";

    const EXECUTED_STATE_OPERATIONS: [&'static str; 5] = [
        Self::STATE_NEW_STORAGE_SLOT,
        Self::STATE_CHANGED_STORAGE_SLOT,
        Self::STATE_CLEARED_STORAGE_SLOT,
        Self::STATE_TOUCHED_ACCOUNT,
        Self::STATE_CHANGED_ACCOUNT,
    ];

    /// Builds a simulated sample from the matching `meterBundle` transaction.
    ///
    /// Returns `None` when the response has no result for `tx_hash` so callers
    /// can fail open instead of attributing bundle totals to the wrong tx.
    pub fn from_meter(meter: &MeterBundleResponse, tx_hash: &TxHash) -> Option<Self> {
        meter
            .results
            .iter()
            .find(|result| result.tx_hash == *tx_hash)
            .map(|result| Self { gas_used: result.gas_used, operations: result.opcode_gas.clone() })
    }

    /// Builds an executed-preferred sample.
    ///
    /// Actual gas and `STATE_*` counts from post-state replace simulated
    /// `STATE_*` rows. Other simulated opcode and precompile rows are kept.
    /// Production execution does not attach opcode bags.
    pub fn from_execution(gas_used: u64, state: &EvmState, simulated: Option<&Self>) -> Self {
        let mut operations = simulated.map(|sample| sample.operations.clone()).unwrap_or_default();
        operations.retain(|entry| {
            let name = ResourceMeteringSchedule::normalize_operation_name(&entry.opcode);
            !Self::EXECUTED_STATE_OPERATIONS.iter().any(|operation| name == *operation)
        });
        Self::push_count(
            &mut operations,
            Self::STATE_NEW_STORAGE_SLOT,
            Self::count_new_storage_slots(state),
        );
        Self::push_count(
            &mut operations,
            Self::STATE_CHANGED_STORAGE_SLOT,
            Self::count_changed_storage_slots(state),
        );
        Self::push_count(
            &mut operations,
            Self::STATE_CLEARED_STORAGE_SLOT,
            Self::count_cleared_storage_slots(state),
        );
        Self::push_count(
            &mut operations,
            Self::STATE_TOUCHED_ACCOUNT,
            Self::count_touched_accounts(state),
        );
        Self::push_count(
            &mut operations,
            Self::STATE_CHANGED_ACCOUNT,
            Self::count_changed_accounts(state),
        );
        Self { gas_used, operations }
    }

    /// Counts changed storage slots whose value transitions from zero to non-zero.
    pub fn count_new_storage_slots(state: &EvmState) -> u64 {
        state
            .values()
            .flat_map(|account| account.storage.values())
            .filter(|slot| slot.original_value().is_zero() && !slot.present_value().is_zero())
            .fold(0, |count, _| count.saturating_add(1))
    }

    /// Counts storage slots whose present value differs from the original value.
    pub fn count_changed_storage_slots(state: &EvmState) -> u64 {
        state
            .values()
            .flat_map(|account| account.storage.values())
            .filter(|slot| slot.is_changed())
            .fold(0, |count, _| count.saturating_add(1))
    }

    /// Counts changed storage slots whose value transitions from non-zero to zero.
    pub fn count_cleared_storage_slots(state: &EvmState) -> u64 {
        state
            .values()
            .flat_map(|account| account.storage.values())
            .filter(|slot| !slot.original_value().is_zero() && slot.present_value().is_zero())
            .fold(0, |count, _| count.saturating_add(1))
    }

    /// Counts accounts marked touched in post-EVM state.
    pub fn count_touched_accounts(state: &EvmState) -> u64 {
        state
            .values()
            .filter(|account| account.is_touched())
            .fold(0, |count, _| count.saturating_add(1))
    }

    /// Counts accounts whose balance, nonce, or code changed from the original info.
    pub fn count_changed_accounts(state: &EvmState) -> u64 {
        state
            .values()
            .filter(|account| account.is_changed())
            .fold(0, |count, _| count.saturating_add(1))
    }

    fn push_count(operations: &mut Vec<OpcodeGas>, name: &'static str, count: u64) {
        if count == 0 {
            return;
        }
        operations.push(OpcodeGas {
            contract_address: Address::ZERO,
            opcode: name.to_string(),
            count,
            gas_used: 0,
        });
    }
}

impl ResourceMeteringSchedule {
    /// Creates an uncompiled schedule with the current schema version.
    ///
    /// Call [`Self::compile`] before [`Self::evaluate`]. [`Self::from_json`]
    /// and [`Self::from_file`] compile automatically.
    pub fn new(dimensions: Vec<ResourceMeteringDimension>) -> Self {
        Self { version: CURRENT_SCHEDULE_VERSION, dimensions, operation_index: HashMap::new() }
    }

    /// Validates this schedule and builds the inverted operation index.
    ///
    /// Hand-built schedules must go through this step before
    /// [`Self::evaluate`]. JSON loaders call it automatically.
    pub fn compile(mut self) -> Result<Self, ResourceMeteringError> {
        self.validate()?;

        let mut operation_index: HashMap<String, Vec<(usize, u64, u64)>> = HashMap::new();
        for (dimension_index, dimension) in self.dimensions.iter_mut().enumerate() {
            dimension.name = dimension.name.trim().to_string();
            for operation in &dimension.operations {
                operation_index
                    .entry(Self::normalize_operation_name(&operation.name))
                    .or_default()
                    .push((dimension_index, operation.gas_used_weight, operation.count_cost));
            }
        }
        self.operation_index = operation_index;
        Ok(self)
    }

    /// Returns whether the schedule has no metering dimensions.
    pub const fn is_empty(&self) -> bool {
        self.dimensions.is_empty()
    }

    /// Operation names priced by this schedule, including post-state effects.
    pub fn priced_operation_names(&self) -> impl Iterator<Item = &str> {
        self.operation_index.keys().map(String::as_str)
    }

    /// Calculates all dimension costs for one metered transaction.
    pub fn evaluate(
        &self,
        gas_used: u64,
        opcode_gas: &[OpcodeGas],
    ) -> Result<ResourceMeteringUsage, ResourceMeteringError> {
        let mut values = vec![0; self.dimensions.len()];

        for (index, dimension) in self.dimensions.iter().enumerate() {
            values[index] = u128::from(dimension.base_gas_weight)
                .checked_mul(u128::from(gas_used))
                .ok_or(ResourceMeteringError::ArithmeticOverflow)?;
        }

        for entry in opcode_gas {
            let operation_name = Self::normalize_operation_name(&entry.opcode);
            let Some(prices) = self.operation_index.get(&operation_name) else {
                continue;
            };

            for &(dimension_index, gas_used_weight, count_price) in prices {
                let gas_cost = u128::from(gas_used_weight)
                    .checked_mul(u128::from(entry.gas_used))
                    .ok_or(ResourceMeteringError::ArithmeticOverflow)?;
                let count_cost = u128::from(count_price)
                    .checked_mul(u128::from(entry.count))
                    .ok_or(ResourceMeteringError::ArithmeticOverflow)?;
                let operation_cost = gas_cost
                    .checked_add(count_cost)
                    .ok_or(ResourceMeteringError::ArithmeticOverflow)?;
                values[dimension_index] = values[dimension_index]
                    .checked_add(operation_cost)
                    .ok_or(ResourceMeteringError::ArithmeticOverflow)?;
            }
        }

        Ok(ResourceMeteringUsage { values })
    }

    /// Checks transaction and cumulative block budgets for one transaction.
    ///
    /// Overruns are ranked so a later dimension can still exclude the
    /// transaction: enforced transaction-scope, then enforced block-scope,
    /// then the first dry-run overrun in schedule order. Arithmetic overflow
    /// is returned immediately.
    pub fn check(
        &self,
        usage: &ResourceMeteringUsage,
        cumulative: &[u128],
    ) -> Result<(), ResourceThrottlingCheckError> {
        let mut enforced_transaction = None;
        let mut enforced_block = None;
        let mut dry_run_error = None;

        for (index, dimension) in self.dimensions.iter().enumerate() {
            let transaction_cost = usage.get(index);

            // Check the per-tx budget before adding to the block cumulative so
            // an overflow cannot fail-open a transaction that cannot fit any
            // empty block.
            if transaction_cost > u128::from(dimension.transaction_limit) {
                let error = ResourceThrottlingLimitExceeded {
                    dimension: dimension.name.clone(),
                    scope: ResourceThrottlingLimitScope::Transaction,
                    used: transaction_cost,
                    transaction_cost,
                    limit: dimension.transaction_limit,
                    dry_run: dimension.dry_run,
                };
                if dimension.dry_run {
                    if dry_run_error.is_none() {
                        dry_run_error = Some(error);
                    }
                } else if enforced_transaction.is_none() {
                    enforced_transaction = Some(error);
                }
                continue;
            }

            let used = cumulative
                .get(index)
                .copied()
                .unwrap_or_default()
                .checked_add(transaction_cost)
                .ok_or(ResourceThrottlingCheckError::ArithmeticOverflow)?;
            if used > u128::from(dimension.block_limit) {
                let error = ResourceThrottlingLimitExceeded {
                    dimension: dimension.name.clone(),
                    scope: ResourceThrottlingLimitScope::Block,
                    used,
                    transaction_cost,
                    limit: dimension.block_limit,
                    dry_run: dimension.dry_run,
                };
                if dimension.dry_run {
                    if dry_run_error.is_none() {
                        dry_run_error = Some(error);
                    }
                } else if enforced_block.is_none() {
                    enforced_block = Some(error);
                }
            }
        }

        if let Some(error) = enforced_transaction.or(enforced_block).or(dry_run_error) {
            return Err(ResourceThrottlingCheckError::LimitExceeded(error));
        }

        Ok(())
    }
}

/// Scope of a resource-throttling budget violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceThrottlingLimitScope {
    /// The transaction exceeded its individual budget.
    Transaction,
    /// Adding the transaction would exceed the block budget.
    Block,
}

impl fmt::Display for ResourceThrottlingLimitScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transaction => formatter.write_str("transaction"),
            Self::Block => formatter.write_str("block"),
        }
    }
}

/// Details of a resource-throttling budget violation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "resource throttling {scope} limit exceeded: dimension={dimension} used={used} \
     transaction_cost={transaction_cost} limit={limit}"
)]
pub struct ResourceThrottlingLimitExceeded {
    /// Dimension whose budget was exceeded.
    pub dimension: String,
    /// Budget scope.
    pub scope: ResourceThrottlingLimitScope,
    /// Usage after adding the transaction, or transaction usage for a transaction budget.
    pub used: u128,
    /// Resource units charged by this transaction.
    pub transaction_cost: u128,
    /// Configured budget.
    pub limit: u64,
    /// Whether this overrun should be observed without excluding the transaction.
    pub dry_run: bool,
}

/// Failure while checking a resource-throttling budget.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResourceThrottlingCheckError {
    /// A configured budget was exceeded.
    #[error("{0}")]
    LimitExceeded(ResourceThrottlingLimitExceeded),
    /// Cumulative usage overflowed the evaluator's integer range.
    #[error("resource throttling arithmetic overflow")]
    ArithmeticOverflow,
}

/// Failure while validating or evaluating a resource-metering schedule.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResourceMeteringError {
    /// The schedule schema version is not supported.
    #[error("unsupported resource metering schedule version: {0}")]
    UnsupportedVersion(u32),
    /// A schedule contains more dimensions than the bounded configuration allows.
    #[error("resource metering schedule has too many dimensions: {0}")]
    TooManyDimensions(usize),
    /// A dimension name is empty.
    #[error("resource metering dimension name is empty")]
    EmptyDimensionName,
    /// A dimension or operation name is too long or contains whitespace.
    #[error("invalid resource metering name: {0}")]
    InvalidName(String),
    /// Two dimensions have the same case-insensitive name.
    #[error("duplicate resource metering dimension: {0}")]
    DuplicateDimension(String),
    /// A dimension has no usable price.
    #[error("resource metering dimension has no non-zero price: {0}")]
    NoopDimension(String),
    /// An operation rule has no non-zero price.
    #[error(
        "resource metering operation has no non-zero price: dimension={dimension} operation={operation}"
    )]
    NoopOperation {
        /// Dimension containing the no-op rule.
        dimension: String,
        /// Operation with the no-op price.
        operation: String,
    },
    /// A dimension has no positive budget.
    #[error("resource metering dimension has a zero block limit: {0}")]
    ZeroBlockLimit(String),
    /// A transaction budget is larger than the block budget.
    #[error("resource metering transaction limit exceeds block limit: dimension={dimension}")]
    TransactionLimitExceedsBlock {
        /// Dimension with the invalid transaction limit.
        dimension: String,
    },
    /// A dimension has too many operation rules.
    #[error(
        "resource metering dimension has too many operations: dimension={dimension} count={count}"
    )]
    TooManyOperations {
        /// Dimension containing too many operation rules.
        dimension: String,
        /// Number of operation rules.
        count: usize,
    },
    /// Two operation rules in one dimension have the same name.
    #[error("duplicate resource metering operation: dimension={dimension} operation={operation}")]
    DuplicateOperation {
        /// Dimension containing the duplicate rule.
        dimension: String,
        /// Duplicate operation name.
        operation: String,
    },
    /// An arithmetic operation overflowed.
    #[error("resource metering arithmetic overflow")]
    ArithmeticOverflow,
    /// The schedule file could not be read.
    #[error("failed to read resource metering schedule: {0}")]
    ReadFile(String),
    /// The schedule file was not valid JSON.
    #[error("failed to parse resource metering schedule JSON: {0}")]
    ParseJson(String),
}

impl ResourceMeteringSchedule {
    /// Returns the current supported schema version.
    pub const fn current_version() -> u32 {
        CURRENT_SCHEDULE_VERSION
    }

    /// Parses a schedule from JSON and compiles it.
    pub fn from_json(json: &str) -> Result<Self, ResourceMeteringError> {
        let file: ResourceMeteringScheduleFile = serde_json::from_str(json)
            .map_err(|error| ResourceMeteringError::ParseJson(error.to_string()))?;
        Self::from_file_dto(file)
    }

    /// Loads a schedule from a JSON file and compiles it.
    pub fn from_file(path: &Path) -> Result<Self, ResourceMeteringError> {
        let json = fs::read_to_string(path)
            .map_err(|error| ResourceMeteringError::ReadFile(error.to_string()))?;
        Self::from_json(&json)
    }

    fn from_file_dto(file: ResourceMeteringScheduleFile) -> Result<Self, ResourceMeteringError> {
        Self {
            version: file.version,
            dimensions: file
                .dimensions
                .into_iter()
                .map(|dimension| {
                    let block_limit = dimension.block_limit;
                    ResourceMeteringDimension {
                        name: dimension.name,
                        block_limit,
                        transaction_limit: dimension.transaction_limit.unwrap_or(block_limit),
                        base_gas_weight: dimension.base_gas_weight,
                        operations: dimension.operations,
                        dry_run: dimension.dry_run,
                    }
                })
                .collect(),
            operation_index: HashMap::new(),
        }
        .compile()
    }

    /// Validates names, limits, and prices.
    ///
    /// [`Self::compile`] calls this before building the operation index.
    pub fn validate(&self) -> Result<(), ResourceMeteringError> {
        if self.version != CURRENT_SCHEDULE_VERSION {
            return Err(ResourceMeteringError::UnsupportedVersion(self.version));
        }
        if self.dimensions.len() > MAX_DIMENSIONS {
            return Err(ResourceMeteringError::TooManyDimensions(self.dimensions.len()));
        }

        let mut dimension_names = HashMap::with_capacity(self.dimensions.len());
        for dimension in &self.dimensions {
            Self::validate_name(&dimension.name, MAX_DIMENSION_NAME_LENGTH)?;
            let dimension_name = dimension.name.trim().to_ascii_lowercase();
            if dimension_names.insert(dimension_name, ()).is_some() {
                return Err(ResourceMeteringError::DuplicateDimension(dimension.name.clone()));
            }
            if dimension.block_limit == 0 {
                return Err(ResourceMeteringError::ZeroBlockLimit(dimension.name.clone()));
            }
            if dimension.transaction_limit == 0
                || dimension.transaction_limit > dimension.block_limit
            {
                return if dimension.transaction_limit == 0 {
                    Err(ResourceMeteringError::NoopDimension(dimension.name.clone()))
                } else {
                    Err(ResourceMeteringError::TransactionLimitExceedsBlock {
                        dimension: dimension.name.clone(),
                    })
                };
            }
            if dimension.operations.len() > MAX_OPERATIONS_PER_DIMENSION {
                return Err(ResourceMeteringError::TooManyOperations {
                    dimension: dimension.name.clone(),
                    count: dimension.operations.len(),
                });
            }

            let mut operation_names = HashMap::with_capacity(dimension.operations.len());
            let has_base_price = dimension.base_gas_weight > 0;
            let mut has_operation_price = false;
            for operation in &dimension.operations {
                Self::validate_name(&operation.name, MAX_OPERATION_NAME_LENGTH)?;
                if operation.gas_used_weight == 0 && operation.count_cost == 0 {
                    return Err(ResourceMeteringError::NoopOperation {
                        dimension: dimension.name.clone(),
                        operation: operation.name.clone(),
                    });
                }
                let operation_name = Self::normalize_operation_name(&operation.name);
                if operation_names.insert(operation_name, ()).is_some() {
                    return Err(ResourceMeteringError::DuplicateOperation {
                        dimension: dimension.name.clone(),
                        operation: operation.name.clone(),
                    });
                }
                has_operation_price |= operation.gas_used_weight > 0 || operation.count_cost > 0;
            }
            if !has_base_price && !has_operation_price {
                return Err(ResourceMeteringError::NoopDimension(dimension.name.clone()));
            }
        }

        Ok(())
    }

    /// Validates a dimension or operation identifier.
    pub fn validate_name(name: &str, max_length: usize) -> Result<(), ResourceMeteringError> {
        let trimmed = name.trim();
        if trimmed.is_empty()
            || name != trimmed
            || trimmed.len() > max_length
            || trimmed.chars().any(char::is_whitespace)
        {
            return Err(if trimmed.is_empty() {
                ResourceMeteringError::EmptyDimensionName
            } else {
                ResourceMeteringError::InvalidName(name.to_string())
            });
        }
        Ok(())
    }

    /// Normalizes an operation name for case-insensitive schedule matching.
    pub fn normalize_operation_name(name: &str) -> String {
        name.trim().to_ascii_uppercase()
    }
}

/// Builder throttling decision for one transaction's metered resource usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceThrottlingDecision {
    /// The transaction may be included. Usage is zero when metering data is missing.
    Allow(ResourceMeteringUsage),
    /// The transaction exceeds a configured budget and should be throttled.
    Throttle {
        /// Budget that was exceeded.
        error: ResourceThrottlingLimitExceeded,
        /// Usage that produced the throttle.
        usage: ResourceMeteringUsage,
    },
    /// Usage could not be calculated from the available metering data.
    CalculationFailed,
}

impl ResourceThrottlingDecision {
    /// Returns whether this decision should exclude the transaction from the payload.
    ///
    /// [`Self::CalculationFailed`] fails open. [`Self::Throttle`] excludes unless
    /// the exceeded dimension is marked [`ResourceMeteringDimension::dry_run`].
    pub const fn should_exclude(&self) -> bool {
        match self {
            Self::Allow(_) | Self::CalculationFailed => false,
            Self::Throttle { error, .. } => !error.dry_run,
        }
    }

    /// Returns `true` if this decision should permanently evict the transaction.
    ///
    /// True only when the transaction should be excluded
    /// ([`Self::should_exclude`]) and the overrun is transaction-scope. Dry-run
    /// transaction-scope observations, block-scope throttles, and
    /// [`Self::CalculationFailed`] are not permanent pool evictions.
    pub const fn is_permanent(&self) -> bool {
        match self {
            Self::Throttle { error, .. } => {
                self.should_exclude()
                    && matches!(error.scope, ResourceThrottlingLimitScope::Transaction)
            }
            Self::Allow(_) | Self::CalculationFailed => false,
        }
    }

    /// Usage to add when this decision is included in the payload.
    ///
    /// Dry-run throttles still return usage. Those transactions are included
    /// (`should_exclude` is false), so later enforce dimensions must see the
    /// real cumulative remaining budget rather than pretending the over-budget
    /// transaction was never in the block.
    pub fn committed_usage(self) -> Option<ResourceMeteringUsage> {
        match self {
            Self::Allow(usage) | Self::Throttle { usage, .. } => Some(usage),
            Self::CalculationFailed => None,
        }
    }
}

impl ResourceMeteringSchedule {
    /// Simulated admission check from `meterBundle` data.
    ///
    /// Missing metering data fails open with zero simulated usage so the
    /// transaction can still execute and be accounted from actual results.
    pub fn evaluate_transaction(
        &self,
        meter: Option<&MeterBundleResponse>,
        tx_hash: &TxHash,
        cumulative: &[u128],
    ) -> ResourceThrottlingDecision {
        let Some(sample) = meter.and_then(|meter| ResourceSample::from_meter(meter, tx_hash))
        else {
            return ResourceThrottlingDecision::Allow(ResourceMeteringUsage::zero(
                self.dimensions.len(),
            ));
        };
        self.decide_sample(&sample, cumulative)
    }

    /// Evaluates a sample against transaction and cumulative block budgets.
    pub fn decide_sample(
        &self,
        sample: &ResourceSample,
        cumulative: &[u128],
    ) -> ResourceThrottlingDecision {
        let usage = match self.evaluate(sample.gas_used, &sample.operations) {
            Ok(usage) => usage,
            Err(_) => return ResourceThrottlingDecision::CalculationFailed,
        };

        match self.check(&usage, cumulative) {
            Ok(()) => ResourceThrottlingDecision::Allow(usage),
            Err(ResourceThrottlingCheckError::LimitExceeded(error)) => {
                ResourceThrottlingDecision::Throttle { error, usage }
            }
            Err(ResourceThrottlingCheckError::ArithmeticOverflow) => {
                ResourceThrottlingDecision::CalculationFailed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use revm::state::{Account, EvmStorageSlot, TransactionId};

    use super::*;

    fn operation(name: &str, gas_used_weight: u64, count_cost: u64) -> ResourceMeteringOperation {
        ResourceMeteringOperation { name: name.to_string(), gas_used_weight, count_cost }
    }

    fn dimension(
        name: &str,
        block_limit: u64,
        transaction_limit: Option<u64>,
        base_gas_weight: u64,
        operations: Vec<ResourceMeteringOperation>,
    ) -> ResourceMeteringDimension {
        ResourceMeteringDimension {
            name: name.to_string(),
            block_limit,
            transaction_limit: transaction_limit.unwrap_or(block_limit),
            base_gas_weight,
            operations,
            dry_run: false,
        }
    }

    fn opcode_gas(opcode: &str, count: u64, gas_used: u64) -> OpcodeGas {
        OpcodeGas { contract_address: Address::ZERO, opcode: opcode.to_string(), count, gas_used }
    }

    #[test]
    fn default_schedule_is_empty_and_current() {
        let schedule = ResourceMeteringSchedule::default();
        assert_eq!(schedule.version, ResourceMeteringSchedule::current_version());
        assert!(schedule.dimensions.is_empty());
        assert!(schedule.compile().unwrap().is_empty());
    }

    #[test]
    fn evaluates_base_gas_and_operation_gas_and_count() {
        let schedule = ResourceMeteringSchedule {
            dimensions: vec![dimension(
                "execution",
                1_000,
                Some(500),
                2,
                vec![operation("SSTORE", 3, 5)],
            )],
            ..Default::default()
        };
        let compiled = schedule.compile().unwrap();
        let usage = compiled.evaluate(100, &[opcode_gas("sstore", 4, 10)]).unwrap();

        assert_eq!(usage.values, vec![250]);
    }

    #[test]
    fn prices_zero_gas_events_by_count() {
        let schedule = ResourceMeteringSchedule {
            dimensions: vec![dimension(
                "new-account",
                100,
                None,
                0,
                vec![operation("TX_EFFECT_ETH_TRANSFER_TO_NONEXISTENT_ACCOUNT", 0, 25)],
            )],
            ..Default::default()
        };
        let compiled = schedule.compile().unwrap();
        let usage = compiled
            .evaluate(0, &[opcode_gas("TX_EFFECT_ETH_TRANSFER_TO_NONEXISTENT_ACCOUNT", 1, 0)])
            .unwrap();

        assert_eq!(usage.values, vec![25]);
    }

    #[test]
    fn one_operation_can_price_multiple_dimensions() {
        let schedule = ResourceMeteringSchedule {
            dimensions: vec![
                dimension("cpu", 100, None, 0, vec![operation("CALL", 2, 0)]),
                dimension("proof", 100, None, 0, vec![operation("CALL", 0, 3)]),
            ],
            ..Default::default()
        };
        let compiled = schedule.compile().unwrap();
        let usage = compiled.evaluate(0, &[opcode_gas("CALL", 4, 10)]).unwrap();

        assert_eq!(usage.values, vec![20, 12]);
    }

    #[test]
    fn checks_transaction_and_block_limits() {
        let schedule = ResourceMeteringSchedule {
            dimensions: vec![dimension("cpu", 100, Some(30), 1, Vec::new())],
            ..Default::default()
        };
        let compiled = schedule.compile().unwrap();
        let usage = compiled.evaluate(40, &[]).unwrap();

        let error = compiled.check(&usage, &[]).unwrap_err();
        assert!(matches!(
            error,
            ResourceThrottlingCheckError::LimitExceeded(ResourceThrottlingLimitExceeded {
                scope: ResourceThrottlingLimitScope::Transaction,
                ..
            })
        ));

        let usage = compiled.evaluate(20, &[]).unwrap();
        let error = compiled.check(&usage, &[90]).unwrap_err();
        assert!(matches!(
            error,
            ResourceThrottlingCheckError::LimitExceeded(ResourceThrottlingLimitExceeded {
                scope: ResourceThrottlingLimitScope::Block,
                ..
            })
        ));
    }

    #[test]
    fn rejects_invalid_schedules() {
        let duplicate = ResourceMeteringSchedule {
            dimensions: vec![
                dimension("CPU", 100, None, 1, Vec::new()),
                dimension("cpu", 100, None, 1, Vec::new()),
            ],
            ..Default::default()
        };
        assert!(matches!(duplicate.validate(), Err(ResourceMeteringError::DuplicateDimension(_))));

        let noop = ResourceMeteringSchedule {
            dimensions: vec![dimension("cpu", 100, None, 0, vec![operation("CALL", 0, 0)])],
            ..Default::default()
        };
        assert!(matches!(noop.validate(), Err(ResourceMeteringError::NoopOperation { .. })));

        let noop_dimension = ResourceMeteringSchedule {
            dimensions: vec![dimension("cpu", 100, None, 0, Vec::new())],
            ..Default::default()
        };
        assert!(matches!(noop_dimension.validate(), Err(ResourceMeteringError::NoopDimension(_))));

        let oversized_transaction_limit = ResourceMeteringSchedule {
            dimensions: vec![dimension("cpu", 10, Some(11), 1, Vec::new())],
            ..Default::default()
        };
        assert!(matches!(
            oversized_transaction_limit.validate(),
            Err(ResourceMeteringError::TransactionLimitExceedsBlock { .. })
        ));
    }

    #[test]
    fn detects_evaluation_overflow() {
        let schedule = ResourceMeteringSchedule {
            dimensions: vec![dimension(
                "overflow",
                u64::MAX,
                None,
                u64::MAX,
                vec![operation("CALL", u64::MAX, 0)],
            )],
            ..Default::default()
        };
        let compiled = schedule.compile().unwrap();

        assert_eq!(
            compiled.evaluate(u64::MAX, &[opcode_gas("CALL", 1, u64::MAX)]),
            Err(ResourceMeteringError::ArithmeticOverflow)
        );
    }

    #[test]
    fn parses_versioned_camel_case_json() {
        let schedule = ResourceMeteringSchedule::from_json(
            r#"{
                "version": 1,
                "dimensions": [{
                    "name": "execution",
                    "blockLimit": 100,
                    "transactionLimit": 50,
                    "baseGasWeight": 2,
                    "operations": [{
                        "name": "SSTORE",
                        "gasUsedWeight": 3,
                        "countCost": 4
                    }]
                }]
            }"#,
        )
        .unwrap();

        assert_eq!(schedule.dimensions[0].transaction_limit, 50);
        assert_eq!(schedule.dimensions[0].operations[0].count_cost, 4);
        assert!(!schedule.dimensions[0].dry_run);
    }

    #[test]
    fn parses_explicit_dry_run_flag() {
        let schedule = ResourceMeteringSchedule::from_json(
            r#"{
                "version": 1,
                "dimensions": [{
                    "name": "execution",
                    "blockLimit": 100,
                    "baseGasWeight": 1,
                    "dryRun": true
                }]
            }"#,
        )
        .unwrap();

        assert!(schedule.dimensions[0].dry_run);
    }

    #[test]
    fn rejects_unknown_json_fields() {
        let error = ResourceMeteringSchedule::from_json(
            r#"{
                "version": 1,
                "unknownField": true
            }"#,
        )
        .unwrap_err();
        assert!(matches!(error, ResourceMeteringError::ParseJson(_)));
    }

    #[test]
    fn from_json_rejects_explicit_zero_transaction_limit() {
        let error = ResourceMeteringSchedule::from_json(
            r#"{
                "version": 1,
                "dimensions": [{
                    "name": "cpu",
                    "blockLimit": 100,
                    "transactionLimit": 0,
                    "baseGasWeight": 1
                }]
            }"#,
        )
        .unwrap_err();
        assert!(matches!(error, ResourceMeteringError::NoopDimension(_)));
    }

    #[test]
    fn missing_metering_data_fails_open_with_zero_usage() {
        let schedule = ResourceMeteringSchedule {
            dimensions: vec![dimension("cpu", 100, None, 1, Vec::new())],
            ..Default::default()
        };
        let compiled = schedule.compile().unwrap();
        let decision = compiled.evaluate_transaction(None, &TxHash::ZERO, &[]);
        assert_eq!(decision, ResourceThrottlingDecision::Allow(ResourceMeteringUsage::zero(1)));
        assert!(!decision.should_exclude());
    }

    fn state_with_slots(slots: &[(U256, U256, U256)]) -> EvmState {
        state_with_account(Address::ZERO, Account::default(), slots)
    }

    fn state_with_account(
        address: Address,
        mut account: Account,
        slots: &[(U256, U256, U256)],
    ) -> EvmState {
        for (key, original, present) in slots {
            account.storage.insert(
                *key,
                EvmStorageSlot::new_changed(*original, *present, TransactionId::ZERO),
            );
        }
        let mut state = EvmState::default();
        state.insert(address, account);
        state
    }

    fn operation_count(sample: &ResourceSample, name: &str) -> Option<u64> {
        sample.operations.iter().find(|entry| entry.opcode == name).map(|entry| entry.count)
    }

    #[test]
    fn counts_only_zero_to_nonzero_storage_transitions() {
        let state = state_with_slots(&[
            (U256::from(1), U256::ZERO, U256::from(7)),
            (U256::from(2), U256::from(1), U256::from(2)),
            (U256::from(3), U256::from(4), U256::ZERO),
        ]);
        assert_eq!(ResourceSample::count_new_storage_slots(&state), 1);
        assert_eq!(ResourceSample::count_changed_storage_slots(&state), 3);
        assert_eq!(ResourceSample::count_cleared_storage_slots(&state), 1);
    }

    #[test]
    fn duplicate_writes_to_one_fresh_slot_count_once() {
        let state = state_with_slots(&[(U256::from(1), U256::ZERO, U256::from(9))]);
        assert_eq!(ResourceSample::count_new_storage_slots(&state), 1);
        assert_eq!(ResourceSample::count_changed_storage_slots(&state), 1);
    }

    #[test]
    fn ignores_loaded_but_unwritten_slots_and_accounts() {
        let mut account = Account::default();
        account
            .storage
            .insert(U256::from(1), EvmStorageSlot::new(U256::from(5), TransactionId::ZERO));
        let mut state = EvmState::default();
        state.insert(Address::ZERO, account);

        assert_eq!(ResourceSample::count_new_storage_slots(&state), 0);
        assert_eq!(ResourceSample::count_changed_storage_slots(&state), 0);
        assert_eq!(ResourceSample::count_cleared_storage_slots(&state), 0);
        assert_eq!(ResourceSample::count_touched_accounts(&state), 0);
        assert_eq!(ResourceSample::count_changed_accounts(&state), 0);
        let sample = ResourceSample::from_execution(21_000, &state, None);
        assert!(sample.operations.is_empty());
    }

    #[test]
    fn counts_touched_and_changed_accounts_without_journal_loads() {
        let mut loaded = Account::default();
        loaded
            .storage
            .insert(U256::from(1), EvmStorageSlot::new(U256::from(5), TransactionId::ZERO));

        let mut touched = Account::default();
        touched.mark_touch();

        let mut changed = Account::default();
        changed.info.balance = U256::from(1);

        let mut both = Account::default();
        both.mark_touch();
        both.info.nonce = 1;

        let mut state = EvmState::default();
        state.insert(Address::repeat_byte(0x01), loaded);
        state.insert(Address::repeat_byte(0x02), touched);
        state.insert(Address::repeat_byte(0x03), changed);
        state.insert(Address::repeat_byte(0x04), both);

        assert_eq!(ResourceSample::count_touched_accounts(&state), 2);
        assert_eq!(ResourceSample::count_changed_accounts(&state), 2);
        assert_eq!(state.len(), 4);
    }

    #[test]
    fn executed_sample_prefers_actual_gas_and_state_effects_over_simulation() {
        let simulated = ResourceSample {
            gas_used: 99_999,
            operations: vec![
                opcode_gas("SSTORE", 6, 200_000),
                opcode_gas(ResourceSample::STATE_NEW_STORAGE_SLOT, 40, 0),
                opcode_gas(ResourceSample::STATE_CHANGED_STORAGE_SLOT, 99, 0),
                opcode_gas(ResourceSample::STATE_TOUCHED_ACCOUNT, 7, 0),
            ],
        };
        let mut account = Account::default();
        account.mark_touch();
        let state = state_with_account(
            Address::ZERO,
            account,
            &[
                (U256::from(1), U256::ZERO, U256::from(1)),
                (U256::from(2), U256::from(4), U256::ZERO),
            ],
        );
        let sample = ResourceSample::from_execution(21_000, &state, Some(&simulated));

        assert_eq!(sample.gas_used, 21_000);
        assert_eq!(operation_count(&sample, "SSTORE"), Some(6));
        assert_eq!(operation_count(&sample, ResourceSample::STATE_NEW_STORAGE_SLOT), Some(1));
        assert_eq!(operation_count(&sample, ResourceSample::STATE_CHANGED_STORAGE_SLOT), Some(2));
        assert_eq!(operation_count(&sample, ResourceSample::STATE_CLEARED_STORAGE_SLOT), Some(1));
        assert_eq!(operation_count(&sample, ResourceSample::STATE_TOUCHED_ACCOUNT), Some(1));
        assert!(operation_count(&sample, ResourceSample::STATE_CHANGED_ACCOUNT).is_none());
    }

    #[test]
    fn executed_slots_are_accounted_without_simulated_data() {
        let schedule = ResourceMeteringSchedule {
            dimensions: vec![dimension(
                "stateRoot",
                50,
                Some(40),
                0,
                vec![operation(ResourceSample::STATE_NEW_STORAGE_SLOT, 0, 10)],
            )],
            ..Default::default()
        };
        let compiled = schedule.compile().unwrap();
        let state = state_with_slots(&[
            (U256::from(1), U256::ZERO, U256::from(1)),
            (U256::from(2), U256::ZERO, U256::from(1)),
            (U256::from(3), U256::ZERO, U256::from(1)),
            (U256::from(4), U256::ZERO, U256::from(1)),
            (U256::from(5), U256::ZERO, U256::from(1)),
            (U256::from(6), U256::ZERO, U256::from(1)),
        ]);
        let sample = ResourceSample::from_execution(21_000, &state, None);
        let decision = compiled.decide_sample(&sample, &[]);
        assert!(matches!(
            decision,
            ResourceThrottlingDecision::Throttle {
                error: ResourceThrottlingLimitExceeded {
                    scope: ResourceThrottlingLimitScope::Transaction,
                    ..
                },
                ..
            }
        ));
        assert!(decision.should_exclude());
        let mut dry_run = ResourceMeteringSchedule {
            dimensions: vec![dimension(
                "stateRoot",
                50,
                Some(40),
                0,
                vec![operation(ResourceSample::STATE_NEW_STORAGE_SLOT, 0, 10)],
            )],
            ..Default::default()
        };
        dry_run.dimensions[0].dry_run = true;
        let compiled = dry_run.compile().unwrap();
        let decision = compiled.decide_sample(&sample, &[]);
        assert!(matches!(decision, ResourceThrottlingDecision::Throttle { .. }));
        assert!(!decision.should_exclude());
    }

    fn meter_response(
        tx_hash: TxHash,
        gas_used: u64,
        operations: Vec<OpcodeGas>,
    ) -> MeterBundleResponse {
        MeterBundleResponse {
            total_gas_used: gas_used.saturating_mul(2),
            results: vec![base_bundles::TransactionResult {
                coinbase_diff: U256::ZERO,
                eth_sent_to_coinbase: U256::ZERO,
                from_address: Address::ZERO,
                gas_fees: U256::ZERO,
                gas_price: U256::ZERO,
                gas_used,
                to_address: None,
                tx_hash,
                value: U256::ZERO,
                execution_time_us: 0,
                opcode_gas: operations,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn from_meter_returns_none_when_the_response_lacks_the_transaction() {
        let meter =
            meter_response(TxHash::repeat_byte(0x11), 21_000, vec![opcode_gas("SSTORE", 1, 10)]);
        assert!(ResourceSample::from_meter(&meter, &TxHash::repeat_byte(0x22)).is_none());
    }

    #[test]
    fn missing_transaction_result_fails_open_like_missing_meter_data() {
        let schedule = ResourceMeteringSchedule {
            dimensions: vec![dimension("cpu", 100, None, 1, Vec::new())],
            ..Default::default()
        };
        let compiled = schedule.compile().unwrap();
        let meter = meter_response(TxHash::repeat_byte(0x11), 90, Vec::new());
        let decision = compiled.evaluate_transaction(Some(&meter), &TxHash::repeat_byte(0x22), &[]);
        assert_eq!(decision, ResourceThrottlingDecision::Allow(ResourceMeteringUsage::zero(1)));
        assert!(!decision.should_exclude());
    }

    #[test]
    fn calculation_failures_fail_open() {
        let decision = ResourceThrottlingDecision::CalculationFailed;
        assert!(!decision.should_exclude());
        assert!(!decision.is_permanent());
        assert!(ResourceThrottlingDecision::CalculationFailed.committed_usage().is_none());
    }

    #[test]
    fn transaction_scope_throttle_is_permanent() {
        let transaction_scope = ResourceThrottlingDecision::Throttle {
            error: ResourceThrottlingLimitExceeded {
                dimension: "cpu".to_string(),
                scope: ResourceThrottlingLimitScope::Transaction,
                used: 50,
                transaction_cost: 50,
                limit: 30,
                dry_run: false,
            },
            usage: ResourceMeteringUsage { values: vec![50] },
        };
        assert!(transaction_scope.should_exclude());
        assert!(transaction_scope.is_permanent());

        let block_scope = ResourceThrottlingDecision::Throttle {
            error: ResourceThrottlingLimitExceeded {
                dimension: "cpu".to_string(),
                scope: ResourceThrottlingLimitScope::Block,
                used: 120,
                transaction_cost: 30,
                limit: 100,
                dry_run: false,
            },
            usage: ResourceMeteringUsage { values: vec![30] },
        };
        assert!(block_scope.should_exclude());
        assert!(!block_scope.is_permanent());

        let dry_run = ResourceThrottlingDecision::Throttle {
            error: ResourceThrottlingLimitExceeded {
                dimension: "cpu".to_string(),
                scope: ResourceThrottlingLimitScope::Transaction,
                used: 50,
                transaction_cost: 50,
                limit: 30,
                dry_run: true,
            },
            usage: ResourceMeteringUsage { values: vec![50] },
        };
        assert!(!dry_run.should_exclude());
        assert!(!dry_run.is_permanent());

        assert!(!ResourceThrottlingDecision::Allow(ResourceMeteringUsage::zero(0)).is_permanent());
    }

    #[test]
    fn add_to_rejects_overflow_without_partial_writes() {
        let usage = ResourceMeteringUsage { values: vec![1, 2] };
        let mut cumulative = vec![u128::MAX, 0];
        assert_eq!(usage.add_to(&mut cumulative), Err(ResourceMeteringError::ArithmeticOverflow));
        assert_eq!(cumulative, vec![u128::MAX, 0]);
    }

    #[test]
    fn enforced_dimension_excludes_even_when_an_earlier_dry_run_dimension_exceeds() {
        let schedule = ResourceMeteringSchedule {
            dimensions: vec![
                {
                    let mut cpu = dimension("cpu", 10, None, 1, Vec::new());
                    cpu.dry_run = true;
                    cpu
                },
                dimension("storage", 10, None, 1, Vec::new()),
            ],
            ..Default::default()
        };
        let compiled = schedule.compile().unwrap();
        let decision = compiled
            .decide_sample(&ResourceSample { gas_used: 21_000, operations: Vec::new() }, &[]);
        assert!(matches!(
            decision,
            ResourceThrottlingDecision::Throttle {
                error: ResourceThrottlingLimitExceeded { ref dimension, dry_run: false, .. },
                ..
            } if dimension == "storage"
        ));
        assert!(decision.should_exclude());
    }

    #[test]
    fn omitted_transaction_limit_compiles_to_block_limit() {
        let schedule = ResourceMeteringSchedule::from_json(
            r#"{
                "version": 1,
                "dimensions": [{
                    "name": "cpu",
                    "blockLimit": 100,
                    "baseGasWeight": 1
                }]
            }"#,
        )
        .unwrap();
        assert_eq!(schedule.dimensions[0].transaction_limit, 100);
        assert_eq!(schedule.dimensions[0].transaction_limit, schedule.dimensions[0].block_limit);
    }

    #[test]
    fn omitted_transaction_limit_overrun_is_transaction_scope() {
        let schedule = ResourceMeteringSchedule {
            dimensions: vec![dimension("cpu", 50, None, 1, Vec::new())],
            ..Default::default()
        };
        let compiled = schedule.compile().unwrap();
        assert_eq!(compiled.dimensions[0].transaction_limit, 50);
        let decision =
            compiled.decide_sample(&ResourceSample { gas_used: 60, operations: Vec::new() }, &[]);
        assert!(matches!(
            decision,
            ResourceThrottlingDecision::Throttle {
                error: ResourceThrottlingLimitExceeded {
                    scope: ResourceThrottlingLimitScope::Transaction,
                    dry_run: false,
                    ..
                },
                ..
            }
        ));
        assert!(decision.should_exclude());
        assert!(decision.is_permanent());
    }

    #[test]
    fn explicit_zero_transaction_limit_fails_validate() {
        let schedule = ResourceMeteringSchedule {
            dimensions: vec![dimension("cpu", 100, Some(0), 1, Vec::new())],
            ..Default::default()
        };
        assert!(matches!(schedule.validate(), Err(ResourceMeteringError::NoopDimension(_))));
    }

    #[test]
    fn enforced_transaction_scope_is_not_masked_by_earlier_enforced_block_scope() {
        let schedule = ResourceMeteringSchedule {
            dimensions: vec![
                dimension("cpu", 100, None, 1, Vec::new()),
                dimension("storage", 100, Some(30), 1, Vec::new()),
            ],
            ..Default::default()
        };
        let compiled = schedule.compile().unwrap();
        let decision = compiled
            .decide_sample(&ResourceSample { gas_used: 40, operations: Vec::new() }, &[90, 0]);
        assert!(matches!(
            decision,
            ResourceThrottlingDecision::Throttle {
                error: ResourceThrottlingLimitExceeded {
                    ref dimension,
                    scope: ResourceThrottlingLimitScope::Transaction,
                    dry_run: false,
                    ..
                },
                ..
            } if dimension == "storage"
        ));
        assert!(decision.should_exclude());
        assert!(decision.is_permanent());
    }

    #[test]
    fn mark_touch_and_storage_slot_changes_do_not_count_as_changed_account() {
        let mut account = Account::default();
        account.mark_touch();
        let state = state_with_account(
            Address::ZERO,
            account,
            &[(U256::from(1), U256::ZERO, U256::from(7))],
        );

        assert_eq!(ResourceSample::count_touched_accounts(&state), 1);
        assert_eq!(ResourceSample::count_changed_storage_slots(&state), 1);
        assert_eq!(ResourceSample::count_changed_accounts(&state), 0);

        let sample = ResourceSample::from_execution(21_000, &state, None);
        assert_eq!(operation_count(&sample, ResourceSample::STATE_TOUCHED_ACCOUNT), Some(1));
        assert_eq!(operation_count(&sample, ResourceSample::STATE_CHANGED_STORAGE_SLOT), Some(1));
        assert!(operation_count(&sample, ResourceSample::STATE_CHANGED_ACCOUNT).is_none());
    }

    #[test]
    fn from_execution_drops_simulated_state_rows_but_keeps_sstore() {
        let simulated = ResourceSample {
            gas_used: 99_999,
            operations: vec![
                opcode_gas("SSTORE", 4, 50_000),
                opcode_gas(ResourceSample::STATE_CHANGED_STORAGE_SLOT, 99, 0),
            ],
        };
        let mut account = Account::default();
        account.mark_touch();
        let state = state_with_account(
            Address::ZERO,
            account,
            &[(U256::from(1), U256::from(1), U256::from(2))],
        );
        let sample = ResourceSample::from_execution(21_000, &state, Some(&simulated));

        assert_eq!(sample.gas_used, 21_000);
        assert_eq!(operation_count(&sample, "SSTORE"), Some(4));
        assert_eq!(operation_count(&sample, ResourceSample::STATE_CHANGED_STORAGE_SLOT), Some(1));
        assert!(operation_count(&sample, ResourceSample::STATE_CHANGED_ACCOUNT).is_none());
        assert_eq!(
            sample
                .operations
                .iter()
                .filter(|entry| entry.opcode == ResourceSample::STATE_CHANGED_STORAGE_SLOT)
                .count(),
            1
        );
    }

    #[test]
    fn from_meter_uses_matching_tx_row_gas_used_not_bundle_total() {
        let first = TxHash::repeat_byte(0x11);
        let second = TxHash::repeat_byte(0x22);
        let mut meter = meter_response(first, 21_000, vec![opcode_gas("SSTORE", 1, 10)]);
        meter.total_gas_used = 99_999;
        meter.results.push(base_bundles::TransactionResult {
            coinbase_diff: U256::ZERO,
            eth_sent_to_coinbase: U256::ZERO,
            from_address: Address::ZERO,
            gas_fees: U256::ZERO,
            gas_price: U256::ZERO,
            gas_used: 50_000,
            to_address: None,
            tx_hash: second,
            value: U256::ZERO,
            execution_time_us: 0,
            opcode_gas: vec![opcode_gas("SLOAD", 2, 20)],
        });

        let first_sample = ResourceSample::from_meter(&meter, &first).unwrap();
        assert_eq!(first_sample.gas_used, 21_000);
        assert_eq!(first_sample.operations, vec![opcode_gas("SSTORE", 1, 10)]);

        let second_sample = ResourceSample::from_meter(&meter, &second).unwrap();
        assert_eq!(second_sample.gas_used, 50_000);
        assert_eq!(second_sample.operations, vec![opcode_gas("SLOAD", 2, 20)]);
        assert_ne!(first_sample.gas_used, meter.total_gas_used);
        assert_ne!(second_sample.gas_used, meter.total_gas_used);
    }

    #[test]
    fn evaluate_ignores_unknown_opcode_observation_names() {
        let schedule = ResourceMeteringSchedule {
            dimensions: vec![dimension("cpu", 1_000, None, 0, vec![operation("SSTORE", 2, 0)])],
            ..Default::default()
        };
        let compiled = schedule.compile().unwrap();
        let usage = compiled
            .evaluate(
                0,
                &[
                    opcode_gas("SSTORE", 1, 10),
                    opcode_gas("UNKNOWN_OPCODE", 99, 1_000),
                    opcode_gas("ECREC", 3, 50),
                ],
            )
            .unwrap();

        assert_eq!(usage.values, vec![20]);
    }
}
