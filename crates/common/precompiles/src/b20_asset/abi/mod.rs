//! Wire (ABI) surfaces for the asset B-20 precompile, one per hardfork that moved them.
//!
//! The latest surface is always named `IB20Asset` in its `vN` module, then re-exported here as
//! both [`IB20Asset`] (canonical) and `IB20AssetVN`. Older forks keep the same Rust name inside
//! their module so truncated-calldata revert bytes stay stable, and are re-exported as
//! [`IB20AssetV1`], [`IB20AssetV2`], etc.
//!
//! Only the asset-specific surface is versioned here. The inherited common surface lives under
//! [`crate::B20Abi`] and is joined with this extension by [`crate::AssetVersion::abi`].
//!
//! This module is pure glue: surface definitions, the `as_label` mapping, the ERC-165 ids, and all
//! tests live in the `vN` modules; the canonical (newest) surface owns anything keyed to it.

mod v1;
pub use v1::IB20Asset as IB20AssetV1;

mod v2;
pub use v2::{ERC165_INTERFACE_ID, ERC8056_INTERFACE_IDS, IB20Asset, IB20Asset as IB20AssetV2};

/// Lifts a Beryl-frozen asset call into the canonical (Cobalt) enum without re-parsing calldata.
///
/// Every V1 selector exists on V2 with identical parameter layouts, so this is a move of owned
/// fields — the second owned ABI materialization that Cantina #16 measured is avoided.
impl From<IB20AssetV1::IB20AssetCalls> for IB20Asset::IB20AssetCalls {
    fn from(call: IB20AssetV1::IB20AssetCalls) -> Self {
        match call {
            IB20AssetV1::IB20AssetCalls::OPERATOR_ROLE(_) => {
                Self::OPERATOR_ROLE(IB20Asset::OPERATOR_ROLECall {})
            }
            IB20AssetV1::IB20AssetCalls::WAD_PRECISION(_) => {
                Self::WAD_PRECISION(IB20Asset::WAD_PRECISIONCall {})
            }
            IB20AssetV1::IB20AssetCalls::announce(c) => Self::announce(IB20Asset::announceCall {
                internalCalls: c.internalCalls,
                id: c.id,
                description: c.description,
                uri: c.uri,
            }),
            IB20AssetV1::IB20AssetCalls::isAnnouncementIdUsed(c) => {
                Self::isAnnouncementIdUsed(IB20Asset::isAnnouncementIdUsedCall { id: c.id })
            }
            IB20AssetV1::IB20AssetCalls::multiplier(_) => {
                Self::multiplier(IB20Asset::multiplierCall {})
            }
            IB20AssetV1::IB20AssetCalls::toScaledBalance(c) => {
                Self::toScaledBalance(IB20Asset::toScaledBalanceCall { rawBalance: c.rawBalance })
            }
            IB20AssetV1::IB20AssetCalls::toRawBalance(c) => {
                Self::toRawBalance(IB20Asset::toRawBalanceCall { scaledBalance: c.scaledBalance })
            }
            IB20AssetV1::IB20AssetCalls::scaledBalanceOf(c) => {
                Self::scaledBalanceOf(IB20Asset::scaledBalanceOfCall { account: c.account })
            }
            IB20AssetV1::IB20AssetCalls::updateMultiplier(c) => {
                Self::updateMultiplier(IB20Asset::updateMultiplierCall {
                    newMultiplier: c.newMultiplier,
                })
            }
            IB20AssetV1::IB20AssetCalls::batchMint(c) => Self::batchMint(IB20Asset::batchMintCall {
                recipients: c.recipients,
                amounts: c.amounts,
            }),
            IB20AssetV1::IB20AssetCalls::extraMetadata(c) => {
                Self::extraMetadata(IB20Asset::extraMetadataCall { key: c.key })
            }
            IB20AssetV1::IB20AssetCalls::updateExtraMetadata(c) => {
                Self::updateExtraMetadata(IB20Asset::updateExtraMetadataCall {
                    key: c.key,
                    value: c.value,
                })
            }
        }
    }
}
