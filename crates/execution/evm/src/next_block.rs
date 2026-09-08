use alloy_eips::eip4895::Withdrawals;
use alloy_primitives::{Address, B256, Bytes};

/// Represents additional attributes required to configure the next block.
///
/// This struct contains all the information needed to build a new block that cannot be
/// derived from the parent block header alone. These attributes are typically provided
/// by the consensus layer (CL) through the Engine API during payload building.
///
/// # Relationship with [`BaseEvmConfig`] and [`execute::BlockAssembler`]
///
/// The flow for building a new block involves:
///
/// 1. **Receive attributes** from the consensus layer containing:
///    - Timestamp for the new block
///    - Fee recipient (coinbase/beneficiary)
///    - Randomness value (prevRandao)
///    - Withdrawals to process
///    - Parent beacon block root for EIP-4788
///
/// 2. **Configure EVM environment** using these attributes: ```rust,ignore let evm_env =
///    evm_config.next_evm_env(&parent, &attributes)?; ```
///
/// 3. **Build the block** with transactions: ```rust,ignore let mut builder =
///    evm_config.builder_for_next_block( &mut state, &parent, attributes )?; ```
///
/// 4. **Assemble the final block** using [`execute::BlockAssembler`] which takes:
///    - Execution results from all transactions
///    - The attributes used during execution
///    - Final state root after all changes
///
/// This design cleanly separates:
/// - **Configuration** (what parameters to use) - handled by `NextBlockEnvAttributes`
/// - **Execution** (running transactions) - handled by `BlockExecutor`
/// - **Assembly** (creating the final block) - handled by `BlockAssembler`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextBlockEnvAttributes {
    /// The timestamp of the next block.
    pub timestamp: u64,
    /// The suggested fee recipient for the next block.
    pub suggested_fee_recipient: Address,
    /// The randomness value for the next block.
    pub prev_randao: B256,
    /// Block gas limit.
    pub gas_limit: u64,
    /// The parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,
    /// Withdrawals
    pub withdrawals: Option<Withdrawals>,
    /// Optional extra data.
    pub extra_data: Bytes,
    /// Optional slot number for post-Amsterdam payloads.
    pub slot_number: Option<u64>,
}
