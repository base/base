//! `EntryPoint` version detection and canonical addresses.

use alloy_primitives::{Address, address};

/// The version of the ERC-4337 `EntryPoint` contract.
#[derive(Debug, Clone)]
pub enum EntryPointVersion {
    /// ERC-4337 v0.6 `EntryPoint`.
    V06,
    /// ERC-4337 v0.7 `EntryPoint`.
    V07,
}

impl EntryPointVersion {
    /// The canonical address of the v0.6 `EntryPoint` contract.
    pub const V06_ADDRESS: Address = address!("0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789");
    /// The canonical address of the v0.7 `EntryPoint` contract.
    pub const V07_ADDRESS: Address = address!("0x0000000071727De22E5E9d8BAf0edAc6f37da032");
}

/// Error returned when an address does not match any known `EntryPoint` version.
#[derive(Debug)]
pub struct UnknownEntryPointAddress {
    /// The unknown address that was provided.
    pub address: Address,
}

impl TryFrom<Address> for EntryPointVersion {
    type Error = UnknownEntryPointAddress;

    fn try_from(addr: Address) -> Result<Self, Self::Error> {
        if addr == Self::V06_ADDRESS {
            Ok(Self::V06)
        } else if addr == Self::V07_ADDRESS {
            Ok(Self::V07)
        } else {
            Err(UnknownEntryPointAddress { address: addr })
        }
    }
}
