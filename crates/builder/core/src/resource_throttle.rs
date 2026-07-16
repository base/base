//! Versioned resource-throttle schedules and their transaction-cost evaluator.

use std::{collections::HashMap, fmt, fs, path::Path, sync::Arc};

use base_bundles::OpcodeGas;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const CURRENT_SCHEDULE_VERSION: u32 = 1;
const MAX_DIMENSIONS: usize = 128;
const MAX_DIMENSION_NAME_LENGTH: usize = 64;
const MAX_OPERATIONS_PER_DIMENSION: usize = 512;
const MAX_OPERATION_NAME_LENGTH: usize = 128;

/// A serializable resource-throttle schedule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceThrottleSchedule {
    /// Schedule schema version.
    pub version: u32,
    /// Independently budgeted resource dimensions.
    #[serde(default)]
    pub dimensions: Vec<ResourceThrottleDimension>,
}

impl Default for ResourceThrottleSchedule {
    fn default() -> Self {
        Self { version: CURRENT_SCHEDULE_VERSION, dimensions: Vec::new() }
    }
}

/// One resource-throttle dimension.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceThrottleDimension {
    /// Stable operator-facing dimension name.
    pub name: String,
    /// Cumulative resource-unit budget for a block.
    pub block_limit: u64,
    /// Optional resource-unit budget for one transaction.
    #[serde(default)]
    pub transaction_limit: Option<u64>,
    /// Resource units charged per unit of actual transaction gas used.
    #[serde(default)]
    pub base_gas_weight: u64,
    /// Additional prices for measured opcodes, precompiles, and pseudo-opcodes.
    #[serde(default)]
    pub operations: Vec<ResourceThrottleOperation>,
}

/// A price applied to one measured operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceThrottleOperation {
    /// Opcode, precompile, or pseudo-opcode name.
    pub name: String,
    /// Resource units charged per measured gas unit.
    #[serde(default)]
    pub gas_used_weight: u64,
    /// Resource units charged per execution/count occurrence.
    #[serde(default)]
    pub count_cost: u64,
}

/// A schedule together with the revision at which it became active.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VersionedResourceThrottleSchedule {
    /// Monotonically increasing schedule revision.
    pub revision: u64,
    /// Active schedule.
    pub schedule: ResourceThrottleSchedule,
}

/// Resource-unit usage aligned with a compiled schedule's dimensions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceThrottleUsage {
    /// Resource units used by each schedule dimension.
    pub values: Vec<u128>,
}

impl ResourceThrottleUsage {
    /// Creates zero usage for a schedule with `dimension_count` dimensions.
    pub fn zero(dimension_count: usize) -> Self {
        Self { values: vec![0; dimension_count] }
    }

    /// Returns the usage for one dimension, treating missing values as zero.
    pub fn get(&self, index: usize) -> u128 {
        self.values.get(index).copied().unwrap_or_default()
    }

    /// Adds this usage to cumulative block usage.
    pub fn add_to(&self, cumulative: &mut Vec<u128>) -> Result<(), ResourceThrottleError> {
        for (index, value) in self.values.iter().copied().enumerate() {
            let current = cumulative.get(index).copied().unwrap_or_default();
            current.checked_add(value).ok_or(ResourceThrottleError::ArithmeticOverflow)?;
        }

        if cumulative.len() < self.values.len() {
            cumulative.resize(self.values.len(), 0);
        }
        for (index, value) in self.values.iter().copied().enumerate() {
            cumulative[index] += value;
        }
        Ok(())
    }
}

/// A compiled schedule used by the builder's hot path.
#[derive(Debug, Clone)]
pub struct CompiledResourceThrottleSchedule {
    /// Revision of the source schedule.
    pub revision: u64,
    /// Validated source schedule.
    pub schedule: ResourceThrottleSchedule,
    /// Compiled dimensions in stable schedule order.
    pub dimensions: Vec<CompiledResourceThrottleDimension>,
    operation_index: HashMap<String, Vec<(usize, u64, u64)>>,
}

/// A compiled resource-throttle dimension.
#[derive(Debug, Clone)]
pub struct CompiledResourceThrottleDimension {
    /// Stable operator-facing dimension name.
    pub name: String,
    /// Cumulative resource-unit budget for a block.
    pub block_limit: u64,
    /// Optional resource-unit budget for one transaction.
    pub transaction_limit: Option<u64>,
    /// Resource units charged per unit of actual transaction gas used.
    pub base_gas_weight: u64,
}

impl CompiledResourceThrottleSchedule {
    /// Compiles a schedule at the supplied revision.
    pub fn compile(
        schedule: ResourceThrottleSchedule,
        revision: u64,
    ) -> Result<Self, ResourceThrottleError> {
        schedule.validate()?;

        let mut dimensions = Vec::with_capacity(schedule.dimensions.len());
        let mut operation_index: HashMap<String, Vec<(usize, u64, u64)>> = HashMap::new();

        for (dimension_index, dimension) in schedule.dimensions.iter().enumerate() {
            dimensions.push(CompiledResourceThrottleDimension {
                name: dimension.name.trim().to_string(),
                block_limit: dimension.block_limit,
                transaction_limit: dimension.transaction_limit,
                base_gas_weight: dimension.base_gas_weight,
            });

            for operation in &dimension.operations {
                operation_index
                    .entry(ResourceThrottleSchedule::normalize_operation_name(&operation.name))
                    .or_default()
                    .push((dimension_index, operation.gas_used_weight, operation.count_cost));
            }
        }

        Ok(Self { revision, schedule, dimensions, operation_index })
    }

    /// Returns whether the schedule has no throttling dimensions.
    pub const fn is_empty(&self) -> bool {
        self.dimensions.is_empty()
    }

    /// Returns the raw schedule and revision.
    pub fn versioned(&self) -> VersionedResourceThrottleSchedule {
        VersionedResourceThrottleSchedule {
            revision: self.revision,
            schedule: self.schedule.clone(),
        }
    }

    /// Calculates all dimension costs for one metered transaction.
    pub fn evaluate(
        &self,
        gas_used: u64,
        opcode_gas: &[OpcodeGas],
    ) -> Result<ResourceThrottleUsage, ResourceThrottleError> {
        let mut values = vec![0; self.dimensions.len()];

        for (index, dimension) in self.dimensions.iter().enumerate() {
            values[index] = u128::from(dimension.base_gas_weight)
                .checked_mul(u128::from(gas_used))
                .ok_or(ResourceThrottleError::ArithmeticOverflow)?;
        }

        for entry in opcode_gas {
            let operation_name = ResourceThrottleSchedule::normalize_operation_name(&entry.opcode);
            let Some(prices) = self.operation_index.get(&operation_name) else {
                continue;
            };

            for &(dimension_index, gas_used_weight, count_price) in prices {
                let gas_cost = u128::from(gas_used_weight)
                    .checked_mul(u128::from(entry.gas_used))
                    .ok_or(ResourceThrottleError::ArithmeticOverflow)?;
                let count_cost = u128::from(count_price)
                    .checked_mul(u128::from(entry.count))
                    .ok_or(ResourceThrottleError::ArithmeticOverflow)?;
                let operation_cost = gas_cost
                    .checked_add(count_cost)
                    .ok_or(ResourceThrottleError::ArithmeticOverflow)?;
                values[dimension_index] = values[dimension_index]
                    .checked_add(operation_cost)
                    .ok_or(ResourceThrottleError::ArithmeticOverflow)?;
            }
        }

        Ok(ResourceThrottleUsage { values })
    }

    /// Checks transaction and cumulative block budgets for one transaction.
    pub fn check(
        &self,
        usage: &ResourceThrottleUsage,
        cumulative: &[u128],
    ) -> Result<(), ResourceThrottleCheckError> {
        for (index, dimension) in self.dimensions.iter().enumerate() {
            let transaction_cost = usage.get(index);

            if let Some(transaction_limit) = dimension.transaction_limit
                && transaction_cost > u128::from(transaction_limit)
            {
                return Err(ResourceThrottleCheckError::LimitExceeded(
                    ResourceThrottleLimitExceeded {
                        dimension: dimension.name.clone(),
                        scope: ResourceThrottleLimitScope::Transaction,
                        used: transaction_cost,
                        transaction_cost,
                        limit: transaction_limit,
                        revision: self.revision,
                    },
                ));
            }

            let used = cumulative
                .get(index)
                .copied()
                .unwrap_or_default()
                .checked_add(transaction_cost)
                .ok_or(ResourceThrottleCheckError::ArithmeticOverflow)?;
            if used > u128::from(dimension.block_limit) {
                return Err(ResourceThrottleCheckError::LimitExceeded(
                    ResourceThrottleLimitExceeded {
                        dimension: dimension.name.clone(),
                        scope: ResourceThrottleLimitScope::Block,
                        used,
                        transaction_cost,
                        limit: dimension.block_limit,
                        revision: self.revision,
                    },
                ));
            }
        }

        Ok(())
    }
}

/// Scope of a resource-throttle budget violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceThrottleLimitScope {
    /// The transaction exceeded its optional individual budget.
    Transaction,
    /// Adding the transaction would exceed the block budget.
    Block,
}

impl fmt::Display for ResourceThrottleLimitScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transaction => formatter.write_str("transaction"),
            Self::Block => formatter.write_str("block"),
        }
    }
}

/// Details of a resource-throttle budget violation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "resource throttle {scope} limit exceeded: dimension={dimension} used={used} \
     transaction_cost={transaction_cost} limit={limit} revision={revision}"
)]
pub struct ResourceThrottleLimitExceeded {
    /// Dimension whose budget was exceeded.
    pub dimension: String,
    /// Budget scope.
    pub scope: ResourceThrottleLimitScope,
    /// Usage after adding the transaction, or transaction usage for a transaction budget.
    pub used: u128,
    /// Resource units charged by this transaction.
    pub transaction_cost: u128,
    /// Configured budget.
    pub limit: u64,
    /// Schedule revision used for the decision.
    pub revision: u64,
}

/// Failure while checking a resource-throttle budget.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResourceThrottleCheckError {
    /// A configured budget was exceeded.
    #[error("{0}")]
    LimitExceeded(ResourceThrottleLimitExceeded),
    /// Cumulative usage overflowed the evaluator's integer range.
    #[error("resource throttle arithmetic overflow")]
    ArithmeticOverflow,
}

/// Failure while validating or evaluating a resource-throttle schedule.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResourceThrottleError {
    /// The schedule schema version is not supported.
    #[error("unsupported resource throttle schedule version: {0}")]
    UnsupportedVersion(u32),
    /// A schedule contains more dimensions than the bounded configuration allows.
    #[error("resource throttle schedule has too many dimensions: {0}")]
    TooManyDimensions(usize),
    /// A dimension name is empty.
    #[error("resource throttle dimension name is empty")]
    EmptyDimensionName,
    /// A dimension or operation name is too long or contains whitespace.
    #[error("invalid resource throttle name: {0}")]
    InvalidName(String),
    /// Two dimensions have the same case-insensitive name.
    #[error("duplicate resource throttle dimension: {0}")]
    DuplicateDimension(String),
    /// A dimension has no usable price.
    #[error("resource throttle dimension has no non-zero price: {0}")]
    NoopDimension(String),
    /// An operation rule has no non-zero price.
    #[error(
        "resource throttle operation has no non-zero price: dimension={dimension} operation={operation}"
    )]
    NoopOperation {
        /// Dimension containing the no-op rule.
        dimension: String,
        /// Operation with the no-op price.
        operation: String,
    },
    /// A dimension has no positive budget.
    #[error("resource throttle dimension has a zero block limit: {0}")]
    ZeroBlockLimit(String),
    /// A transaction budget is larger than the block budget.
    #[error("resource throttle transaction limit exceeds block limit: dimension={dimension}")]
    TransactionLimitExceedsBlock {
        /// Dimension with the invalid transaction limit.
        dimension: String,
    },
    /// A dimension has too many operation rules.
    #[error(
        "resource throttle dimension has too many operations: dimension={dimension} count={count}"
    )]
    TooManyOperations {
        /// Dimension containing too many operation rules.
        dimension: String,
        /// Number of operation rules.
        count: usize,
    },
    /// Two operation rules in one dimension have the same name.
    #[error("duplicate resource throttle operation: dimension={dimension} operation={operation}")]
    DuplicateOperation {
        /// Dimension containing the duplicate rule.
        dimension: String,
        /// Duplicate operation name.
        operation: String,
    },
    /// An arithmetic operation overflowed.
    #[error("resource throttle arithmetic overflow")]
    ArithmeticOverflow,
}

/// Failure while replacing a shared resource-throttle schedule.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResourceThrottleStoreError {
    /// The replacement schedule failed validation.
    #[error("{0}")]
    InvalidSchedule(#[from] ResourceThrottleError),
    /// The caller attempted to replace a schedule based on a stale revision.
    #[error("resource throttle schedule revision conflict: expected={expected} actual={actual}")]
    RevisionConflict {
        /// Revision supplied by the caller.
        expected: u64,
        /// Current active revision.
        actual: u64,
    },
    /// The active revision could not be incremented.
    #[error("resource throttle schedule revision overflow")]
    RevisionOverflow,
    /// The schedule file could not be read.
    #[error("failed to read resource throttle schedule: {0}")]
    ReadFile(String),
    /// The schedule file was not valid JSON.
    #[error("failed to parse resource throttle schedule JSON: {0}")]
    ParseJson(String),
}

/// Shared, atomically replaceable resource-throttle schedule.
#[derive(Debug)]
pub struct ResourceThrottleStore {
    active: RwLock<Arc<CompiledResourceThrottleSchedule>>,
}

impl ResourceThrottleStore {
    /// Creates a store with an empty schedule at revision zero.
    pub fn empty() -> Self {
        Self::from_schedule(ResourceThrottleSchedule::default())
            .expect("the default resource throttle schedule is valid")
    }

    /// Creates a store from a validated startup schedule.
    pub fn from_schedule(
        schedule: ResourceThrottleSchedule,
    ) -> Result<Self, ResourceThrottleStoreError> {
        let compiled = CompiledResourceThrottleSchedule::compile(schedule, 0)?;
        Ok(Self { active: RwLock::new(Arc::new(compiled)) })
    }

    /// Creates a store from a JSON schedule.
    pub fn from_json(json: &str) -> Result<Self, ResourceThrottleStoreError> {
        let schedule = serde_json::from_str(json)
            .map_err(|error| ResourceThrottleStoreError::ParseJson(error.to_string()))?;
        Self::from_schedule(schedule)
    }

    /// Loads a store from a JSON schedule file.
    pub fn from_file(path: &Path) -> Result<Self, ResourceThrottleStoreError> {
        let json = fs::read_to_string(path)
            .map_err(|error| ResourceThrottleStoreError::ReadFile(error.to_string()))?;
        Self::from_json(&json)
    }

    /// Returns an immutable schedule snapshot.
    pub fn snapshot(&self) -> Arc<CompiledResourceThrottleSchedule> {
        self.active.read().clone()
    }

    /// Returns the active raw schedule and revision.
    pub fn get(&self) -> VersionedResourceThrottleSchedule {
        self.active.read().versioned()
    }

    /// Returns the active schedule revision.
    pub fn revision(&self) -> u64 {
        self.active.read().revision
    }

    /// Atomically replaces the schedule, optionally checking its current revision.
    pub fn replace(
        &self,
        schedule: ResourceThrottleSchedule,
        expected_revision: Option<u64>,
    ) -> Result<u64, ResourceThrottleStoreError> {
        // Compile before taking the write lock so schedule validation and indexing do not block
        // payload jobs that only need to snapshot the active schedule.
        let mut compiled = CompiledResourceThrottleSchedule::compile(schedule, 0)?;
        let mut active = self.active.write();
        let actual = active.revision;
        if let Some(expected) = expected_revision
            && expected != actual
        {
            return Err(ResourceThrottleStoreError::RevisionConflict { expected, actual });
        }

        let revision = actual.checked_add(1).ok_or(ResourceThrottleStoreError::RevisionOverflow)?;
        compiled.revision = revision;
        *active = Arc::new(compiled);
        Ok(revision)
    }
}

impl Default for ResourceThrottleStore {
    fn default() -> Self {
        Self::empty()
    }
}

/// Type-erased shared resource-throttle store.
pub type SharedResourceThrottleStore = Arc<ResourceThrottleStore>;

impl ResourceThrottleSchedule {
    /// Returns the current supported schema version.
    pub const fn current_version() -> u32 {
        CURRENT_SCHEDULE_VERSION
    }

    /// Validates the schedule before it is compiled or activated.
    pub fn validate(&self) -> Result<(), ResourceThrottleError> {
        if self.version != CURRENT_SCHEDULE_VERSION {
            return Err(ResourceThrottleError::UnsupportedVersion(self.version));
        }
        if self.dimensions.len() > MAX_DIMENSIONS {
            return Err(ResourceThrottleError::TooManyDimensions(self.dimensions.len()));
        }

        let mut dimension_names = HashMap::with_capacity(self.dimensions.len());
        for dimension in &self.dimensions {
            Self::validate_name(&dimension.name, MAX_DIMENSION_NAME_LENGTH)?;
            let dimension_name = dimension.name.trim().to_ascii_lowercase();
            if dimension_names.insert(dimension_name, ()).is_some() {
                return Err(ResourceThrottleError::DuplicateDimension(dimension.name.clone()));
            }
            if dimension.block_limit == 0 {
                return Err(ResourceThrottleError::ZeroBlockLimit(dimension.name.clone()));
            }
            if let Some(transaction_limit) = dimension.transaction_limit
                && (transaction_limit == 0 || transaction_limit > dimension.block_limit)
            {
                return if transaction_limit == 0 {
                    Err(ResourceThrottleError::NoopDimension(dimension.name.clone()))
                } else {
                    Err(ResourceThrottleError::TransactionLimitExceedsBlock {
                        dimension: dimension.name.clone(),
                    })
                };
            }
            if dimension.operations.len() > MAX_OPERATIONS_PER_DIMENSION {
                return Err(ResourceThrottleError::TooManyOperations {
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
                    return Err(ResourceThrottleError::NoopOperation {
                        dimension: dimension.name.clone(),
                        operation: operation.name.clone(),
                    });
                }
                let operation_name = Self::normalize_operation_name(&operation.name);
                if operation_names.insert(operation_name, ()).is_some() {
                    return Err(ResourceThrottleError::DuplicateOperation {
                        dimension: dimension.name.clone(),
                        operation: operation.name.clone(),
                    });
                }
                has_operation_price |= operation.gas_used_weight > 0 || operation.count_cost > 0;
            }
            if !has_base_price && !has_operation_price {
                return Err(ResourceThrottleError::NoopDimension(dimension.name.clone()));
            }
        }

        Ok(())
    }
    /// Validates a dimension or operation identifier.
    pub fn validate_name(name: &str, max_length: usize) -> Result<(), ResourceThrottleError> {
        let trimmed = name.trim();
        if trimmed.is_empty()
            || name != trimmed
            || trimmed.len() > max_length
            || trimmed.chars().any(char::is_whitespace)
        {
            return Err(if trimmed.is_empty() {
                ResourceThrottleError::EmptyDimensionName
            } else {
                ResourceThrottleError::InvalidName(name.to_string())
            });
        }
        Ok(())
    }

    /// Normalizes an operation name for case-insensitive schedule matching.
    pub fn normalize_operation_name(name: &str) -> String {
        name.trim().to_ascii_uppercase()
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, thread};

    use alloy_primitives::Address;

    use super::*;

    fn operation(name: &str, gas_used_weight: u64, count_cost: u64) -> ResourceThrottleOperation {
        ResourceThrottleOperation { name: name.to_string(), gas_used_weight, count_cost }
    }

    fn dimension(
        name: &str,
        block_limit: u64,
        transaction_limit: Option<u64>,
        base_gas_weight: u64,
        operations: Vec<ResourceThrottleOperation>,
    ) -> ResourceThrottleDimension {
        ResourceThrottleDimension {
            name: name.to_string(),
            block_limit,
            transaction_limit,
            base_gas_weight,
            operations,
        }
    }

    fn opcode_gas(opcode: &str, count: u64, gas_used: u64) -> OpcodeGas {
        OpcodeGas { contract_address: Address::ZERO, opcode: opcode.to_string(), count, gas_used }
    }

    #[test]
    fn default_schedule_is_empty_and_current() {
        let schedule = ResourceThrottleSchedule::default();
        assert_eq!(schedule.version, ResourceThrottleSchedule::current_version());
        assert!(schedule.dimensions.is_empty());
        assert!(ResourceThrottleStore::default().snapshot().is_empty());
    }

    #[test]
    fn evaluates_base_gas_and_operation_gas_and_count() {
        let schedule = ResourceThrottleSchedule {
            dimensions: vec![dimension(
                "execution",
                1_000,
                Some(500),
                2,
                vec![operation("SSTORE", 3, 5)],
            )],
            ..Default::default()
        };
        let compiled = CompiledResourceThrottleSchedule::compile(schedule, 7).unwrap();
        let usage = compiled.evaluate(100, &[opcode_gas("sstore", 4, 10)]).unwrap();

        assert_eq!(usage.values, vec![250]);
    }

    #[test]
    fn prices_zero_gas_events_by_count() {
        let schedule = ResourceThrottleSchedule {
            dimensions: vec![dimension(
                "new-account",
                100,
                None,
                0,
                vec![operation("TX_EFFECT_ETH_TRANSFER_TO_NONEXISTENT_ACCOUNT", 0, 25)],
            )],
            ..Default::default()
        };
        let compiled = CompiledResourceThrottleSchedule::compile(schedule, 0).unwrap();
        let usage = compiled
            .evaluate(0, &[opcode_gas("TX_EFFECT_ETH_TRANSFER_TO_NONEXISTENT_ACCOUNT", 1, 0)])
            .unwrap();

        assert_eq!(usage.values, vec![25]);
    }

    #[test]
    fn one_operation_can_price_multiple_dimensions() {
        let schedule = ResourceThrottleSchedule {
            dimensions: vec![
                dimension("cpu", 100, None, 0, vec![operation("CALL", 2, 0)]),
                dimension("proof", 100, None, 0, vec![operation("CALL", 0, 3)]),
            ],
            ..Default::default()
        };
        let compiled = CompiledResourceThrottleSchedule::compile(schedule, 0).unwrap();
        let usage = compiled.evaluate(0, &[opcode_gas("CALL", 4, 10)]).unwrap();

        assert_eq!(usage.values, vec![20, 12]);
    }

    #[test]
    fn checks_transaction_and_block_limits() {
        let schedule = ResourceThrottleSchedule {
            dimensions: vec![dimension("cpu", 100, Some(30), 1, Vec::new())],
            ..Default::default()
        };
        let compiled = CompiledResourceThrottleSchedule::compile(schedule, 4).unwrap();
        let usage = compiled.evaluate(40, &[]).unwrap();

        let error = compiled.check(&usage, &[]).unwrap_err();
        assert!(matches!(
            error,
            ResourceThrottleCheckError::LimitExceeded(ResourceThrottleLimitExceeded {
                scope: ResourceThrottleLimitScope::Transaction,
                ..
            })
        ));

        let usage = compiled.evaluate(20, &[]).unwrap();
        let error = compiled.check(&usage, &[90]).unwrap_err();
        assert!(matches!(
            error,
            ResourceThrottleCheckError::LimitExceeded(ResourceThrottleLimitExceeded {
                scope: ResourceThrottleLimitScope::Block,
                ..
            })
        ));
    }

    #[test]
    fn rejects_invalid_schedules() {
        let duplicate = ResourceThrottleSchedule {
            dimensions: vec![
                dimension("CPU", 100, None, 1, Vec::new()),
                dimension("cpu", 100, None, 1, Vec::new()),
            ],
            ..Default::default()
        };
        assert!(matches!(duplicate.validate(), Err(ResourceThrottleError::DuplicateDimension(_))));

        let noop = ResourceThrottleSchedule {
            dimensions: vec![dimension("cpu", 100, None, 0, vec![operation("CALL", 0, 0)])],
            ..Default::default()
        };
        assert!(matches!(noop.validate(), Err(ResourceThrottleError::NoopOperation { .. })));

        let noop_dimension = ResourceThrottleSchedule {
            dimensions: vec![dimension("cpu", 100, None, 0, Vec::new())],
            ..Default::default()
        };
        assert!(matches!(noop_dimension.validate(), Err(ResourceThrottleError::NoopDimension(_))));

        let oversized_transaction_limit = ResourceThrottleSchedule {
            dimensions: vec![dimension("cpu", 10, Some(11), 1, Vec::new())],
            ..Default::default()
        };
        assert!(matches!(
            oversized_transaction_limit.validate(),
            Err(ResourceThrottleError::TransactionLimitExceedsBlock { .. })
        ));
    }

    #[test]
    fn detects_evaluation_overflow() {
        let schedule = ResourceThrottleSchedule {
            dimensions: vec![dimension(
                "overflow",
                u64::MAX,
                None,
                u64::MAX,
                vec![operation("CALL", u64::MAX, 0)],
            )],
            ..Default::default()
        };
        let compiled = CompiledResourceThrottleSchedule::compile(schedule, 0).unwrap();

        assert_eq!(
            compiled.evaluate(u64::MAX, &[opcode_gas("CALL", 1, u64::MAX)]),
            Err(ResourceThrottleError::ArithmeticOverflow)
        );
    }

    #[test]
    fn replaces_schedule_atomically_with_revision_check() {
        let store = ResourceThrottleStore::default();
        let old_snapshot = store.snapshot();
        let schedule = ResourceThrottleSchedule {
            dimensions: vec![dimension("cpu", 100, None, 1, Vec::new())],
            ..Default::default()
        };

        assert_eq!(store.revision(), 0);
        assert_eq!(store.replace(schedule.clone(), Some(0)).unwrap(), 1);
        assert_eq!(store.get().revision, 1);
        assert_eq!(store.snapshot().dimensions[0].name, "cpu");
        assert!(old_snapshot.is_empty());
        assert!(matches!(
            store.replace(schedule, Some(0)),
            Err(ResourceThrottleStoreError::RevisionConflict { expected: 0, actual: 1 })
        ));
    }

    #[test]
    fn concurrent_replacements_allow_only_one_revision_match() {
        let store = Arc::new(ResourceThrottleStore::default());
        let schedule = ResourceThrottleSchedule {
            dimensions: vec![dimension("cpu", 100, None, 1, Vec::new())],
            ..Default::default()
        };

        let first_store = Arc::clone(&store);
        let first_schedule = schedule.clone();
        let first = thread::spawn(move || first_store.replace(first_schedule, Some(0)));
        let second_store = Arc::clone(&store);
        let second = thread::spawn(move || second_store.replace(schedule, Some(0)));

        let results = [first.join().unwrap(), second.join().unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| {
                    matches!(
                        result,
                        Err(ResourceThrottleStoreError::RevisionConflict {
                            expected: 0,
                            actual: 1
                        })
                    )
                })
                .count(),
            1
        );
        assert_eq!(store.revision(), 1);
    }

    #[test]
    fn parses_versioned_camel_case_json() {
        let store = ResourceThrottleStore::from_json(
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

        let active = store.get();
        assert_eq!(active.revision, 0);
        assert_eq!(active.schedule.dimensions[0].transaction_limit, Some(50));
        assert_eq!(active.schedule.dimensions[0].operations[0].count_cost, 4);
    }
}
