//! L1 transaction format selection for the derivation-pipeline reader.

/// The transaction format of the L1 chain the derivation pipeline reads from.
///
/// A `Base` L1 carries deposit (`0x7E`) and EIP-8130 (`0x7D`) transactions (and the receipts
/// mirroring them) the default Ethereum envelopes cannot deserialize. Selected at startup via
/// `--l1.tx-format`.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, derive_more::Display, derive_more::FromStr,
)]
pub enum L1TxFormat {
    /// An Ethereum-format L1 chain (alloy's standard envelopes, blob DA).
    #[display("ethereum")]
    #[default]
    Ethereum,
    /// A Base/OP-format L1 chain (deposit/EIP-8130 transactions, calldata DA).
    #[display("base")]
    Base,
}
