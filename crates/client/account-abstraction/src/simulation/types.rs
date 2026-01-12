//! Core types for UserOperation simulation and validation
//!
//! This module provides the common types used across the simulation system,
//! including validation results, stake information, and ERC-7562 rule violations.

use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_trace::geth::erc7562::Erc7562Frame;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Output from UserOperation validation
///
/// This is a normalized structure that works across all EntryPoint versions (v0.6, v0.7, v0.8).
/// Version-specific results are converted to this common format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationOutput {
    /// Whether the UserOp passed validation (simulation succeeded and no rule violations)
    pub valid: bool,

    /// Return info from simulateValidation
    pub return_info: ReturnInfo,

    /// Sender (account) stake information
    pub sender_info: StakeInfo,

    /// Factory stake information (if factory was used)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub factory_info: Option<StakeInfo>,

    /// Paymaster stake information (if paymaster was used)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paymaster_info: Option<StakeInfo>,

    /// Aggregator stake information (if aggregator was used)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregator_info: Option<AggregatorStakeInfo>,

    /// Code hashes for all entities at validation time
    ///
    /// Used for:
    /// - COD-010: Detecting code changes between 1st and 2nd validation
    /// - Mempool inclusion: Invalidating UserOps if entity code changes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_hashes: Option<EntityCodeHashes>,

    /// ERC-7562 rule violations (empty if compliant)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<RuleViolation>,

    /// The ERC-7562 trace frame (optional, for debugging)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<Box<Erc7562Frame>>,
}

/// Code hashes for all entities involved in a UserOperation
///
/// Captured at validation time to support:
/// - COD-010: Code change detection between 1st and 2nd validation
/// - Mempool invalidation when entity code is modified
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityCodeHashes {
    /// Sender (account) code hash
    /// Will be zero/empty hash if account doesn't exist yet (deployment)
    pub sender: B256,

    /// Factory code hash (if factory is used)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub factory: Option<B256>,

    /// Paymaster code hash (if paymaster is used)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paymaster: Option<B256>,

    /// Aggregator code hash (if aggregator is used)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregator: Option<B256>,
}

impl EntityCodeHashes {
    /// Check if any code hash has changed compared to another snapshot
    ///
    /// Returns the address of the first entity whose code changed, if any.
    /// Used for COD-010 validation.
    pub fn find_changed_code(&self, other: &EntityCodeHashes, entities: &Entities) -> Option<Address> {
        if self.sender != other.sender {
            return Some(entities.sender);
        }
        if self.factory != other.factory {
            if let Some(factory) = entities.factory {
                return Some(factory);
            }
        }
        if self.paymaster != other.paymaster {
            if let Some(paymaster) = entities.paymaster {
                return Some(paymaster);
            }
        }
        if self.aggregator != other.aggregator {
            if let Some(aggregator) = entities.aggregator {
                return Some(aggregator);
            }
        }
        None
    }
}

impl ValidationOutput {
    /// Check if there are any ERC-7562 rule violations
    pub fn has_violations(&self) -> bool {
        !self.violations.is_empty()
    }

    /// Check if signature validation failed
    pub fn signature_failed(&self) -> bool {
        self.return_info.sig_failed
    }

    /// Check if the UserOp is within its validity time window
    pub fn is_time_valid(&self, current_time: u64) -> bool {
        let valid_after = self.return_info.valid_after;
        let valid_until = self.return_info.valid_until;

        // validUntil == 0 means no expiry
        let not_expired = valid_until == 0 || current_time <= valid_until;
        let not_too_early = current_time >= valid_after;

        not_expired && not_too_early
    }
}

/// Return information from validation simulation
///
/// Normalized from v0.6 and v0.7+ ReturnInfo structs
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReturnInfo {
    /// Gas used during pre-operation phase (validation + setup)
    pub pre_op_gas: u64,

    /// Amount the account needs to prefund for this operation
    pub prefund: U256,

    /// Whether signature validation failed
    /// - v0.6: directly from `sigFailed` field
    /// - v0.7+: extracted from `validationData` (aggregator == address(1))
    pub sig_failed: bool,

    /// Earliest timestamp this operation is valid (0 = no restriction)
    /// - v0.6: directly from `validAfter` field
    /// - v0.7+: extracted from `validationData` (lowest 6 bytes)
    pub valid_after: u64,

    /// Latest timestamp this operation is valid (0 = no expiry)
    /// - v0.6: directly from `validUntil` field
    /// - v0.7+: extracted from `validationData` (bytes 6-12)
    pub valid_until: u64,

    /// Paymaster context (returned by paymaster.validatePaymasterUserOp)
    #[serde(default, skip_serializing_if = "is_bytes_empty")]
    pub paymaster_context: Bytes,
}

/// Helper function to check if Bytes is empty (for serde skip_serializing_if)
fn is_bytes_empty(bytes: &Bytes) -> bool {
    bytes.is_empty()
}

impl Default for ReturnInfo {
    fn default() -> Self {
        Self {
            pre_op_gas: 0,
            prefund: U256::ZERO,
            sig_failed: false,
            valid_after: 0,
            valid_until: 0,
            paymaster_context: Bytes::default(),
        }
    }
}

/// Stake information for an entity (account, factory, paymaster)
///
/// Retrieved from EntryPoint.getDepositInfo(address)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StakeInfo {
    /// The entity address
    pub address: Address,

    /// Amount of ETH staked
    pub stake: U256,

    /// Unstake delay in seconds
    pub unstake_delay_sec: u64,

    /// Amount of ETH deposited (for gas payment)
    pub deposit: U256,

    /// Whether the entity is considered "staked" (meets MIN_STAKE_VALUE and MIN_UNSTAKE_DELAY)
    /// This is computed based on chain-specific staking requirements
    pub is_staked: bool,
}

impl StakeInfo {
    /// Create a new StakeInfo for an address with no stake
    pub fn unstaked(address: Address) -> Self {
        Self {
            address,
            stake: U256::ZERO,
            unstake_delay_sec: 0,
            deposit: U256::ZERO,
            is_staked: false,
        }
    }

    /// Check if this entity meets the staking requirements
    pub fn meets_stake_requirements(&self, min_stake: U256, min_unstake_delay: u64) -> bool {
        self.stake >= min_stake && self.unstake_delay_sec >= min_unstake_delay
    }
}

/// Aggregator stake information
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregatorStakeInfo {
    /// The aggregator address
    pub aggregator: Address,

    /// The aggregator's stake info
    pub stake_info: StakeInfo,
}

/// ERC-7562 staking configuration
#[derive(Debug, Clone)]
pub struct StakingConfig {
    /// Minimum stake value (in wei) - ~$1000 equivalent
    pub min_stake_value: U256,

    /// Minimum unstake delay in seconds (default: 86400 = 1 day)
    pub min_unstake_delay: u64,
}

impl Default for StakingConfig {
    fn default() -> Self {
        Self {
            // Default to 0.1 ETH as minimum stake
            min_stake_value: U256::from(100_000_000_000_000_000u128), // 0.1 ETH
            min_unstake_delay: 86400, // 1 day
        }
    }
}

/// ERC-7562 rule identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Erc7562Rule {
    // Opcode rules (OP-*)
    /// OP-011: Banned opcode used
    Op011BannedOpcode,
    /// OP-012: GAS opcode not followed by *CALL
    Op012GasNotFollowedByCall,
    /// OP-013: Unassigned opcode used
    Op013UnassignedOpcode,
    /// OP-020: Out of gas (forbidden)
    Op020OutOfGas,
    /// OP-031: CREATE2 used more than once or not for sender
    Op031InvalidCreate2,
    /// OP-041: EXTCODE* on address without code
    Op041ExtCodeOnEmptyAddress,
    /// OP-042: Exception - sender address access allowed in factory
    Op042SenderAccessInFactory,
    /// OP-051-054: Invalid EntryPoint access
    Op05xInvalidEntryPointAccess,
    /// OP-061: CALL with value (not to EntryPoint)
    Op061CallWithValue,
    /// OP-062: Invalid precompile access
    Op062InvalidPrecompile,
    /// OP-070: Transient storage rules (same as STO-*)
    Op070TransientStorage,

    // Storage rules (STO-*)
    /// STO-010: Account storage access (always allowed)
    Sto010AccountStorage,
    /// STO-021: Associated storage without existing account
    Sto021AssociatedStorageNoAccount,
    /// STO-022: Associated storage needs staked factory
    Sto022AssociatedStorageNeedsStakedFactory,
    /// STO-031: Entity storage (needs stake)
    Sto031EntityStorageNeedsStake,
    /// STO-032: Associated storage in non-entity (needs stake)
    Sto032AssociatedNonEntityNeedsStake,
    /// STO-033: Read-only non-entity storage (needs stake)
    Sto033ReadOnlyNonEntityNeedsStake,
    /// STO-040: Entity address used as account elsewhere
    Sto040EntityUsedAsAccount,
    /// STO-041: Associated storage conflict with other sender
    Sto041AssociatedStorageConflict,

    // Code rules (COD-*)
    /// COD-010: Code changed between validations
    Cod010CodeChanged,

    // EIP-7702 Authorization rules (AUTH-*)
    /// AUTH-010: UserOp may only contain a single EIP-7702 authorization tuple
    Auth010MultipleAuthorizations,
    /// AUTH-020: Account with EIP-7702 delegation can only be used as Sender
    Auth020DelegationNotSender,
    /// AUTH-030: Account with EIP-7702 delegation can only be accessed if it's the Sender
    Auth030DelegationAccessNotSender,
    /// AUTH-040: Multiple UserOps from same sender must have same delegate (local/mempool rule)
    Auth040DifferentDelegates,

    // Unknown/custom rule
    /// Custom or unknown rule
    Unknown,
}

impl Erc7562Rule {
    /// Get the rule ID string (e.g., "OP-011")
    pub fn id(&self) -> &'static str {
        match self {
            Self::Op011BannedOpcode => "OP-011",
            Self::Op012GasNotFollowedByCall => "OP-012",
            Self::Op013UnassignedOpcode => "OP-013",
            Self::Op020OutOfGas => "OP-020",
            Self::Op031InvalidCreate2 => "OP-031",
            Self::Op041ExtCodeOnEmptyAddress => "OP-041",
            Self::Op042SenderAccessInFactory => "OP-042",
            Self::Op05xInvalidEntryPointAccess => "OP-05x",
            Self::Op061CallWithValue => "OP-061",
            Self::Op062InvalidPrecompile => "OP-062",
            Self::Op070TransientStorage => "OP-070",
            Self::Sto010AccountStorage => "STO-010",
            Self::Sto021AssociatedStorageNoAccount => "STO-021",
            Self::Sto022AssociatedStorageNeedsStakedFactory => "STO-022",
            Self::Sto031EntityStorageNeedsStake => "STO-031",
            Self::Sto032AssociatedNonEntityNeedsStake => "STO-032",
            Self::Sto033ReadOnlyNonEntityNeedsStake => "STO-033",
            Self::Sto040EntityUsedAsAccount => "STO-040",
            Self::Sto041AssociatedStorageConflict => "STO-041",
            Self::Cod010CodeChanged => "COD-010",
            Self::Auth010MultipleAuthorizations => "AUTH-010",
            Self::Auth020DelegationNotSender => "AUTH-020",
            Self::Auth030DelegationAccessNotSender => "AUTH-030",
            Self::Auth040DifferentDelegates => "AUTH-040",
            Self::Unknown => "UNKNOWN",
        }
    }

    /// Check if this is a "hard" network rule (vs "soft" local rule)
    ///
    /// Local rules depend on mempool state and can differ between bundlers:
    /// - STO-040, STO-041: Depend on other UserOps in local mempool
    /// - AUTH-040: Depends on other UserOps from same sender in mempool
    pub fn is_network_rule(&self) -> bool {
        !matches!(
            self,
            Self::Sto040EntityUsedAsAccount
                | Self::Sto041AssociatedStorageConflict
                | Self::Auth040DifferentDelegates
        )
    }
}

impl fmt::Display for Erc7562Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.id())
    }
}

/// ERC-7562 rule violation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleViolation {
    /// The rule that was violated
    pub rule: Erc7562Rule,

    /// The entity (address) that caused the violation
    pub entity: Address,

    /// Human-readable description
    pub description: String,

    /// Additional details about the violation
    #[serde(flatten)]
    pub details: ViolationDetails,
}

impl RuleViolation {
    /// Create a new rule violation
    pub fn new(
        rule: Erc7562Rule,
        entity: Address,
        description: impl Into<String>,
        details: ViolationDetails,
    ) -> Self {
        Self {
            rule,
            entity,
            description: description.into(),
            details,
        }
    }

    /// Create a banned opcode violation
    pub fn banned_opcode(entity: Address, opcode: u8, opcode_name: &str) -> Self {
        Self::new(
            Erc7562Rule::Op011BannedOpcode,
            entity,
            format!("Banned opcode {} (0x{:02x}) used", opcode_name, opcode),
            ViolationDetails::BannedOpcode { opcode },
        )
    }

    /// Create an out of gas violation
    pub fn out_of_gas(entity: Address) -> Self {
        Self::new(
            Erc7562Rule::Op020OutOfGas,
            entity,
            "Validation ran out of gas",
            ViolationDetails::OutOfGas,
        )
    }

    /// Create a storage access violation
    pub fn storage_access(
        rule: Erc7562Rule,
        entity: Address,
        slot: B256,
        description: impl Into<String>,
    ) -> Self {
        Self::new(rule, entity, description, ViolationDetails::StorageAccess { slot })
    }

    /// Create an ext code access violation
    pub fn ext_code_access(entity: Address, target: Address) -> Self {
        Self::new(
            Erc7562Rule::Op041ExtCodeOnEmptyAddress,
            entity,
            format!("EXTCODE* on address {} without code", target),
            ViolationDetails::ExtCodeAccess { address: target },
        )
    }

    /// Create an invalid CREATE2 violation (OP-031)
    pub fn invalid_create2(entity: Address, count: usize, description: impl Into<String>) -> Self {
        Self::new(
            Erc7562Rule::Op031InvalidCreate2,
            entity,
            description,
            ViolationDetails::InvalidCreate2 { count },
        )
    }

    /// Create a CALL with value violation (OP-061)
    pub fn call_with_value(entity: Address, target: Address, value: U256) -> Self {
        Self::new(
            Erc7562Rule::Op061CallWithValue,
            entity,
            format!(
                "CALL with value {} to non-EntryPoint address {}",
                value, target
            ),
            ViolationDetails::CallWithValue { target, value },
        )
    }

    /// Create an invalid EntryPoint access violation (OP-05x)
    pub fn invalid_entrypoint_access(entity: Address, description: impl Into<String>) -> Self {
        Self::new(
            Erc7562Rule::Op05xInvalidEntryPointAccess,
            entity,
            description,
            ViolationDetails::None,
        )
    }

    /// Create a multiple authorization tuples violation (AUTH-010)
    pub fn multiple_authorizations(entity: Address, count: usize) -> Self {
        Self::new(
            Erc7562Rule::Auth010MultipleAuthorizations,
            entity,
            format!("UserOp contains {} EIP-7702 authorization tuples, only 1 allowed", count),
            ViolationDetails::Authorization {
                count: Some(count),
                delegated_address: None,
                delegate: None,
            },
        )
    }

    /// Create a delegation not sender violation (AUTH-020)
    pub fn delegation_not_sender(entity: Address, delegated_address: Address) -> Self {
        Self::new(
            Erc7562Rule::Auth020DelegationNotSender,
            entity,
            format!(
                "Account {} with EIP-7702 delegation used as entity (not sender)",
                delegated_address
            ),
            ViolationDetails::Authorization {
                count: None,
                delegated_address: Some(delegated_address),
                delegate: None,
            },
        )
    }

    /// Create a delegation access not sender violation (AUTH-030)
    pub fn delegation_access_not_sender(entity: Address, delegated_address: Address) -> Self {
        Self::new(
            Erc7562Rule::Auth030DelegationAccessNotSender,
            entity,
            format!(
                "Account {} with EIP-7702 delegation accessed but is not the sender",
                delegated_address
            ),
            ViolationDetails::Authorization {
                count: None,
                delegated_address: Some(delegated_address),
                delegate: None,
            },
        )
    }

    /// Create a different delegates violation (AUTH-040, local rule)
    pub fn different_delegates(entity: Address, delegate1: Address, delegate2: Address) -> Self {
        Self::new(
            Erc7562Rule::Auth040DifferentDelegates,
            entity,
            format!(
                "Multiple UserOps from same sender have different delegates: {} vs {}",
                delegate1, delegate2
            ),
            ViolationDetails::Authorization {
                count: None,
                delegated_address: None,
                delegate: Some(delegate1),
            },
        )
    }

    /// Create an invalid precompile access violation (OP-062)
    pub fn invalid_precompile(entity: Address, precompile: Address) -> Self {
        Self::new(
            Erc7562Rule::Op062InvalidPrecompile,
            entity,
            format!("Access to forbidden precompile {}", precompile),
            ViolationDetails::ExtCodeAccess { address: precompile },
        )
    }
}

impl fmt::Display for RuleViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}: {}", self.rule.id(), self.entity, self.description)
    }
}

/// Details about a rule violation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ViolationDetails {
    /// A banned opcode was used
    #[serde(rename_all = "camelCase")]
    BannedOpcode {
        /// The opcode that was used
        opcode: u8,
    },

    /// Storage was accessed in violation of rules
    #[serde(rename_all = "camelCase")]
    StorageAccess {
        /// The storage slot that was accessed
        slot: B256,
    },

    /// EXTCODE* was called on an address without code
    #[serde(rename_all = "camelCase")]
    ExtCodeAccess {
        /// The address that was accessed
        address: Address,
    },

    /// Validation ran out of gas
    OutOfGas,

    /// Invalid CREATE2 usage
    #[serde(rename_all = "camelCase")]
    InvalidCreate2 {
        /// Number of CREATE2 calls
        count: usize,
    },

    /// CALL with value not to EntryPoint
    #[serde(rename_all = "camelCase")]
    CallWithValue {
        /// Target of the call
        target: Address,
        /// Value sent
        value: U256,
    },

    /// EIP-7702 authorization issues
    #[serde(rename_all = "camelCase")]
    Authorization {
        /// Number of authorization tuples (for AUTH-010)
        #[serde(skip_serializing_if = "Option::is_none")]
        count: Option<usize>,
        /// Address with delegation that violated rules
        #[serde(skip_serializing_if = "Option::is_none")]
        delegated_address: Option<Address>,
        /// The delegate address (for AUTH-040 comparison)
        #[serde(skip_serializing_if = "Option::is_none")]
        delegate: Option<Address>,
    },

    /// No specific details
    None,
}

/// Entities involved in a UserOperation
#[derive(Debug, Clone, Default)]
pub struct Entities {
    /// The sender (account) address
    pub sender: Address,

    /// The factory address (if initCode is present)
    pub factory: Option<Address>,

    /// The paymaster address (if paymasterAndData is present)
    pub paymaster: Option<Address>,

    /// The aggregator address (if signature aggregation is used)
    pub aggregator: Option<Address>,
}

impl Entities {
    /// Get all entity addresses (excluding sender which is always present)
    pub fn all_entities(&self) -> Vec<Address> {
        let mut entities = vec![self.sender];
        if let Some(factory) = self.factory {
            entities.push(factory);
        }
        if let Some(paymaster) = self.paymaster {
            entities.push(paymaster);
        }
        if let Some(aggregator) = self.aggregator {
            entities.push(aggregator);
        }
        entities
    }

    /// Check if an address is an entity in this UserOp
    pub fn is_entity(&self, address: Address) -> bool {
        address == self.sender
            || self.factory == Some(address)
            || self.paymaster == Some(address)
            || self.aggregator == Some(address)
    }
}

/// Collected stake information for all entities
#[derive(Debug, Clone, Default)]
pub struct EntitiesInfo {
    /// Sender stake info
    pub sender: StakeInfo,

    /// Factory stake info (if present)
    pub factory: Option<StakeInfo>,

    /// Paymaster stake info (if present)
    pub paymaster: Option<StakeInfo>,

    /// Aggregator stake info (if present)
    pub aggregator: Option<AggregatorStakeInfo>,
}

/// Validation error
#[derive(Debug, Clone, thiserror::Error)]
pub enum ValidationError {
    /// Unsupported EntryPoint address
    #[error("Unsupported EntryPoint: {0}")]
    UnsupportedEntryPoint(Address),

    /// Precheck failed (fast validation before simulation)
    #[error("Precheck failed: {0}")]
    PrecheckFailed(super::precheck::PrecheckViolation),

    /// Simulation failed
    #[error("Simulation failed: {0}")]
    SimulationFailed(String),

    /// Validation reverted
    #[error("Validation reverted: {0}")]
    ValidationReverted(String),

    /// Provider error
    #[error("Provider error: {0}")]
    ProviderError(String),

    /// Decoding error
    #[error("Failed to decode: {0}")]
    DecodingError(String),

    /// Tracing error
    #[error("Tracing error: {0}")]
    TracingError(String),
}

