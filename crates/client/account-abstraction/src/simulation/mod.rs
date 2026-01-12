//! UserOperation Simulation and Validation
//!
//! This module provides functionality for simulating and validating UserOperations
//! according to ERC-4337 and ERC-7562 rules.
//!
//! # Overview
//!
//! The simulation module is responsible for:
//! - Running `simulateValidation` on UserOperations
//! - Tracing execution with ERC-7562 tracer
//! - Checking ERC-7562 validation scope rules
//! - Collecting entity stake/deposit information
//!
//! # Components
//!
//! - [`UserOperationValidator`] - Main entry point for validation
//! - [`EntityInfoProvider`] - Queries stake/deposit info from EntryPoint
//! - [`Erc7562RuleChecker`] - Checks ERC-7562 rules against traced execution
//!
//! # Usage
//!
//! ```ignore
//! use base_account_abstraction::simulation::{UserOperationValidator, ValidationConfig};
//!
//! let validator = UserOperationValidator::new(provider);
//! let result = validator.validate(&user_op, entry_point).await?;
//!
//! if result.valid {
//!     // UserOp passed validation
//! } else {
//!     // Check result.violations for ERC-7562 rule violations
//!     // Check result.return_info.sig_failed for signature failures
//! }
//! ```

mod entity;
mod precheck;
mod rules;
mod types;
mod validator;

pub use entity::{EntityInfoProvider, is_entry_point, entry_point_address};
pub use precheck::{Prechecker, PrecheckConfig, PrecheckResult, PrecheckViolation};
pub use rules::{Erc7562RuleChecker, BANNED_OPCODES, ALLOWED_PRECOMPILES, opcode_name};
pub use types::{
    AggregatorStakeInfo,
    Entities,
    EntitiesInfo,
    EntityCodeHashes,
    Erc7562Rule,
    ReturnInfo,
    RuleViolation,
    StakeInfo,
    StakingConfig,
    ValidationError,
    ValidationOutput,
    ViolationDetails,
};
pub use validator::{
    UserOperationValidator, ValidationConfig,
    // EIP-7702 delegation helpers
    is_eip7702_delegation, get_eip7702_delegate, EIP7702_DELEGATION_PREFIX,
};

