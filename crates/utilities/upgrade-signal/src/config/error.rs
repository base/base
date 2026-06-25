/// Error returned when CLI arguments cannot form an upgrade signal configuration.
#[derive(Debug, thiserror::Error)]
pub enum UpgradeSignalConfigError {
    /// Hardfork IDs were set without a contract address.
    #[error("upgrade signal hardfork ID requires --upgrade-signal.contract")]
    MissingContractAddress,
    /// The hardfork ID is empty.
    #[error("upgrade signal hardfork ID cannot be empty")]
    EmptyHardforkId,
    /// The hardfork ID is not recognized.
    #[error("unknown upgrade signal hardfork ID `{0}`")]
    UnknownHardforkId(String),
    /// An apply hardfork ID is not present in the set of read hardfork IDs.
    #[error(
        "upgrade signal apply hardfork ID `{0}` is not read; add it to --upgrade-signal.hardfork-id"
    )]
    ApplyHardforkIdNotRead(String),
}
