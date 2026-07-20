//! CLI argument structs for `state-populate`.

use alloy_primitives::{Address, B256, U256};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Top-level CLI for the `state-populate` utility.
#[derive(Debug, Parser)]
#[command(name = "state-populate", version)]
pub struct Args {
    /// Subcommand to run.
    #[command(subcommand)]
    pub command: SubCommand,
}

/// The subcommand selecting whether to populate or verify state.
#[derive(Debug, Subcommand)]
pub enum SubCommand {
    /// Write balance slots, accounts, and trie nodes into the database.
    Populate(PopulateArgs),
    /// Read back and validate previously written state.
    Verify(VerifyArgs),
}

/// Arguments for the `populate` subcommand.
#[derive(Debug, Parser)]
pub struct PopulateArgs {
    /// Path to the reth datadir (contains the `db/` sub-directory).
    #[arg(long)]
    pub datadir: PathBuf,

    /// Address of the contract whose balances mapping is populated.
    #[arg(long)]
    pub contract: Address,

    /// Base slot of the `mapping(address => uint256)` being populated
    /// (the first state variable of a standard ERC-20 is slot 0).
    #[arg(long, default_value = "0x0000000000000000000000000000000000000000000000000000000000000000")]
    pub balance_slot: B256,

    /// Number of synthetic balance slots to write (e.g. 700000000).
    #[arg(long, default_value = "700000000")]
    pub count: u64,

    /// Raw token balance credited to each account (in the token's base unit).
    #[arg(long, default_value = "1000000000000000000")]
    pub balance: U256,

    /// Number of balance slots written per MDBX transaction.
    #[arg(long, default_value = "1000000")]
    pub chunk_size: u64,

    /// Seed for deriving load-test sender addresses to pre-seed with balances.
    /// Must match the `seed` field in the load-test config being benchmarked.
    #[arg(long)]
    pub seed: Option<u64>,

    /// Number of sender addresses to derive from `--seed` and pre-seed with balances.
    /// Must match `sender_count` in the load-test config.
    #[arg(long)]
    pub sender_count: Option<u64>,

    /// Also write synthetic EOA accounts to `PlainAccountState` + `HashedAccounts` + `AccountsTrie`.
    /// The same `address_for_index` addresses are used for token balances, making every
    /// account holder also a token holder (worst-case equal-depth trie test).
    #[arg(long)]
    pub populate_accounts: bool,

    /// Number of synthetic EOA accounts to write (default: same as `--count`).
    #[arg(long)]
    pub account_count: Option<u64>,

    /// ETH balance (in wei) credited to each synthetic EOA account.
    #[arg(long, default_value = "1000000000000000000")]
    pub account_balance: U256,

    /// Rebuild only the storage and account tries from already-written flat state.
    /// Skips all balance-slot writes; use this to repair a dataset whose tries are
    /// stale without re-running the multi-hour slot write.
    #[arg(long)]
    pub trie_only: bool,
}

/// Arguments for the `verify` subcommand.
#[derive(Debug, Parser)]
pub struct VerifyArgs {
    /// Path to the reth datadir (contains the `db/` sub-directory).
    #[arg(long)]
    pub datadir: PathBuf,

    /// Address of the populated contract.
    #[arg(long)]
    pub contract: Address,

    /// Base slot of the populated balances mapping.
    #[arg(long, default_value = "0x0000000000000000000000000000000000000000000000000000000000000000")]
    pub balance_slot: B256,

    /// Expected number of balance slots (0 = skip the slot check).
    #[arg(long, default_value = "700000000")]
    pub count: u64,

    /// Seed used when populating sender balances; required to verify them.
    #[arg(long)]
    pub seed: Option<u64>,

    /// Number of sender addresses that were pre-seeded; required to verify them.
    #[arg(long)]
    pub sender_count: Option<u64>,

    /// Only verify the pre-seeded sender balance slots, skipping the slow full
    /// `DupSort` count; use this to quickly confirm sender balances are present.
    #[arg(long)]
    pub senders_only: bool,
}
