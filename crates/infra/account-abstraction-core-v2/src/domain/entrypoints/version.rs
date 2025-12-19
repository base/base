use alloy_primitives::{Address, address};

#[derive(Debug, Clone)]
pub enum EntryPointVersion {
    V06,
    V07,
}

impl EntryPointVersion {
    pub const V06_ADDRESS: Address = address!("0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789");
    pub const V07_ADDRESS: Address = address!("0x0000000071727De22E5E9d8BAf0edAc6f37da032");
}

#[derive(Debug)]
pub struct UnknownEntryPointAddress {
    pub address: Address,
}

impl TryFrom<Address> for EntryPointVersion {
    type Error = UnknownEntryPointAddress;

    fn try_from(addr: Address) -> Result<Self, Self::Error> {
        if addr == Self::V06_ADDRESS {
            Ok(EntryPointVersion::V06)
        } else if addr == Self::V07_ADDRESS {
            Ok(EntryPointVersion::V07)
        } else {
            Err(UnknownEntryPointAddress { address: addr })
        }
    }
}
