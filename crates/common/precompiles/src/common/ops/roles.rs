//! Built-in role identifiers for B-20 tokens.

use alloy_primitives::{B256, b256};

const MINT_ROLE: B256 = b256!("154c00819833dac601ee5ddded6fda79d9d8b506b911b3dbd54cdb95fe6c3686");
const BURN_ROLE: B256 = b256!("e97b137254058bd94f28d2f3eb79e2d34074ffb488d042e3bc958e0a57d2fa22");
const BURN_BLOCKED_ROLE: B256 =
    b256!("7408fdc0d31c7bcb349eab611f5d1168acd4303574993f8cdc98b1cd18c41cae");
const TRANSFER_FROM_SEIZABLE_ROLE: B256 =
    b256!("466d0fa97b99b3315998c229880e8a3a6aa5f5586b46f762a35e210facb4ea63");
const PAUSE_ROLE: B256 = b256!("139c2898040ef16910dc9f44dc697df79363da767d8bc92f2e310312b816e46d");
const UNPAUSE_ROLE: B256 =
    b256!("265b220c5a8891efdd9e1b1b7fa72f257bd5169f8d87e319cf3dad6ff52b94ae");
const METADATA_ROLE: B256 =
    b256!("6bd6b5318a46e5fff572d5e4258a20774aab40cc35ac7680654b9081fcc82f80");

/// Built-in B-20 roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum B20TokenRole {
    /// The default top-level admin role.
    DefaultAdmin,
    /// Role required for `mint` and `mintWithMemo`.
    Mint,
    /// Role required for `burn` and `burnWithMemo`.
    Burn,
    /// Role required for `burnBlocked`; permits burning from blocked accounts without `BURN_ROLE`.
    BurnBlocked,
    /// Role required for `transferFromSeizableWithMemo`; permits reassigning a blocked account's
    /// balance without `TRANSFER_SENDER_POLICY` authorization.
    TransferFromSeizable,
    /// Role required for `pause`.
    Pause,
    /// Role required for `unpause`.
    Unpause,
    /// Role required for `updateName` and `updateSymbol`.
    Metadata,
}

impl B20TokenRole {
    /// Returns the `AccessControl` role identifier.
    pub const fn id(self) -> B256 {
        match self {
            Self::DefaultAdmin => B256::ZERO,
            Self::Mint => MINT_ROLE,
            Self::Burn => BURN_ROLE,
            Self::BurnBlocked => BURN_BLOCKED_ROLE,
            Self::TransferFromSeizable => TRANSFER_FROM_SEIZABLE_ROLE,
            Self::Pause => PAUSE_ROLE,
            Self::Unpause => UNPAUSE_ROLE,
            Self::Metadata => METADATA_ROLE,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, keccak256};

    use crate::B20TokenRole;

    #[test]
    fn default_admin_role_matches_access_control_zero() {
        assert_eq!(B20TokenRole::DefaultAdmin.id(), B256::ZERO);
    }

    /// Cross-impl parity: keccak-derived role ids must equal `keccak256` of the exact strings
    /// base-std uses, so the two implementations cannot silently diverge. `DefaultAdmin` is the
    /// `AccessControl` zero sentinel, not a keccak, so it is excluded.
    #[test]
    fn role_ids_match_base_std_keccak() {
        assert_eq!(B20TokenRole::Mint.id(), keccak256("MINT_ROLE"));
        assert_eq!(B20TokenRole::Burn.id(), keccak256("BURN_ROLE"));
        assert_eq!(B20TokenRole::BurnBlocked.id(), keccak256("BURN_BLOCKED_ROLE"));
        assert_eq!(
            B20TokenRole::TransferFromSeizable.id(),
            keccak256("TRANSFER_FROM_SEIZABLE_ROLE")
        );
        assert_eq!(B20TokenRole::Pause.id(), keccak256("PAUSE_ROLE"));
        assert_eq!(B20TokenRole::Unpause.id(), keccak256("UNPAUSE_ROLE"));
        assert_eq!(B20TokenRole::Metadata.id(), keccak256("METADATA_ROLE"));
    }
}
