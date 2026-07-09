//! Canonical [EIP-8130] system-contract addresses and the node authenticator allowlist.
//!
//! These are the deterministic CREATE2 addresses of the EIP-8130 contracts (the
//! Account Configuration system contract, the account implementations, and the
//! canonical authenticators). Every contract is deployed through Nick's
//! deterministic-deployment proxy ([`Eip8130Contracts::CREATE2_FACTORY`]) with a
//! zero salt, so the address is a pure function of the contract init code and is
//! identical on every chain that deploys the same bytecode.
//!
//! # ⚠️ These values are NOT final
//!
//! EIP-8130 is in Draft and the reference contracts (`base/eip-8130`) are still
//! churning. Because each address is derived from the contract init code, **any
//! change to the contract bytecode changes its address**, and the account-
//! implementation and delegate-authenticator addresses additionally cascade off
//! the Account Configuration address (it is passed as a constructor argument).
//!
//! The values below are the current Base Sepolia deployment. They are expected to
//! change as the contracts evolve and finalize. On each redeploy, re-pin both the
//! address and its `*_INIT_CODE_HASH` together (the [`tests`] module asserts they
//! stay consistent under CREATE2) and, once the bytecode is frozen for the Cobalt
//! upgrade, freeze these as the canonical mainnet values.
//!
//! [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130

use alloy_primitives::{Address, B256, address, b256};

/// Canonical [EIP-8130] contract addresses and the node authenticator allowlist.
///
/// See the [module docs](self) for the (important) caveat that these are
/// provisional Base Sepolia values that change with the contract bytecode.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Eip8130Contracts;

impl Eip8130Contracts {
    /// Nick's deterministic-deployment proxy (the "Arachnid" CREATE2 factory),
    /// deployed at the same address on every EVM chain. Every EIP-8130 contract is
    /// deployed by sending `SALT || init_code` to this factory.
    ///
    /// <https://github.com/Arachnid/deterministic-deployment-proxy>
    pub const CREATE2_FACTORY: Address = address!("0x4e59b44847b379578588920cA78FbF26c0B4956C");

    /// The CREATE2 salt used for every EIP-8130 deployment (zero).
    pub const SALT: B256 = B256::ZERO;

    // ─────────────────────────────────────────────────────────────────────────
    // System contract
    // ─────────────────────────────────────────────────────────────────────────

    /// Account Configuration system contract (`ACCOUNT_CONFIG_ADDRESS`). The
    /// protocol reads actor/account state directly from this contract's storage.
    pub const ACCOUNT_CONFIG: Address = address!("0x2403408177dB7F8512a9593343a7C80371D8f2dF");

    /// keccak256 of the `ACCOUNT_CONFIG` deployment init code (for CREATE2
    /// derivation and bytecode-drift detection).
    pub const ACCOUNT_CONFIG_INIT_CODE_HASH: B256 =
        b256!("0x742384b2e8ee556f4575e73524f6c3d84780c73b73140ea8770cb65abc143714");

    // ─────────────────────────────────────────────────────────────────────────
    // Account implementations (init code embeds `ACCOUNT_CONFIG`)
    // ─────────────────────────────────────────────────────────────────────────

    /// Default wallet implementation, used as the target of default EOA delegation.
    pub const DEFAULT_ACCOUNT: Address = address!("0xaF0973bbebe12BDaE6B61c96019dc0DcA554b67c");

    /// keccak256 of the `DEFAULT_ACCOUNT` deployment init code.
    pub const DEFAULT_ACCOUNT_INIT_CODE_HASH: B256 =
        b256!("0xf447cb971ef3087ff92c3238b4f90721d73c681661be5525d8937c476c7ac707");

    /// Wallet variant that blocks ETH transfers when locked, granting higher
    /// EIP-8130 mempool access (rate limits).
    pub const DEFAULT_HIGH_RATE_ACCOUNT: Address =
        address!("0x6c4230a4101849a3CB6438C40D3d47EdE9aca096");

    /// keccak256 of the `DEFAULT_HIGH_RATE_ACCOUNT` deployment init code.
    pub const DEFAULT_HIGH_RATE_ACCOUNT_INIT_CODE_HASH: B256 =
        b256!("0x2aa0cfe89cf370c6ece7cc751c00c3fa3c2044a750755ed6f4125119275bc251");

    // ─────────────────────────────────────────────────────────────────────────
    // Canonical authenticators (accepted on the EIP-8130 block-validation path)
    // ─────────────────────────────────────────────────────────────────────────

    // Note: secp256k1 has no contract entry. It is the protocol-reserved native
    // k1 sentinel
    // [`Eip8130Constants::K1_AUTHENTICATOR`](super::Eip8130Constants::K1_AUTHENTICATOR)
    // (`address(1)`), handled directly by the protocol, not a deployed contract.

    /// secp256r1 / P-256 (raw) authenticator contract.
    pub const P256_AUTHENTICATOR: Address = address!("0x28096E6f98996799A08fBbCFF0B7c0D512D1f503");

    /// keccak256 of the `P256_AUTHENTICATOR` deployment init code.
    pub const P256_AUTHENTICATOR_INIT_CODE_HASH: B256 =
        b256!("0xf2e69d7271b922c65cb55cc325efca4c7bd22e0c154fd6ff75575cd9f1a4db78");

    /// secp256r1 / P-256 (`WebAuthn`) authenticator contract.
    pub const WEBAUTHN_AUTHENTICATOR: Address =
        address!("0xD9B8d163a34FBaD781057F7B68889F0bbd70D7ed");

    /// keccak256 of the `WEBAUTHN_AUTHENTICATOR` deployment init code.
    pub const WEBAUTHN_AUTHENTICATOR_INIT_CODE_HASH: B256 =
        b256!("0x35cc9b095487cc4debe2956dcda11be5ae0586bb3c985d1b4d90d8e2a1f09460");

    /// Delegated-validation (1-hop) authenticator contract (init code embeds
    /// `ACCOUNT_CONFIG`).
    pub const DELEGATE_AUTHENTICATOR: Address =
        address!("0xb1f064A99919E4199b45F1b553b6ecb8d5d62a11");

    /// keccak256 of the `DELEGATE_AUTHENTICATOR` deployment init code.
    pub const DELEGATE_AUTHENTICATOR_INIT_CODE_HASH: B256 =
        b256!("0xb4c6b1f40cd6a3a256f20cc7aad6599390ba3540044e08134349b54f7b1439a8");

    /// The canonical authenticator allowlist: the deployed `IAuthenticator`
    /// contracts a compliant node accepts on the EIP-8130 block-validation path.
    ///
    /// secp256k1 is **not** a contract entry here: on EIP-8130 chains it is the
    /// protocol-reserved native ecrecover sentinel (`address(1)`), handled
    /// directly by the protocol rather than via a deployed authenticator contract.
    pub const CANONICAL_AUTHENTICATORS: [Address; 3] =
        [Self::P256_AUTHENTICATOR, Self::WEBAUTHN_AUTHENTICATOR, Self::DELEGATE_AUTHENTICATOR];

    /// Returns `true` if `authenticator` is in the canonical deployed-contract
    /// allowlist ([`Self::CANONICAL_AUTHENTICATORS`]).
    ///
    /// This intentionally does not account for the native ecrecover sentinel
    /// (`address(1)`), which is handled separately by the protocol.
    #[must_use]
    pub fn is_canonical_authenticator(authenticator: &Address) -> bool {
        Self::CANONICAL_AUTHENTICATORS.contains(authenticator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each `(address, init_code_hash)` pair must be self-consistent under
    /// CREATE2 with the canonical factory + zero salt. If the contract bytecode
    /// changes, both values must be re-pinned together or this fails — the drift
    /// guard for the provisional addresses.
    #[test]
    fn addresses_match_create2_derivation() {
        let cases = [
            (Eip8130Contracts::ACCOUNT_CONFIG, Eip8130Contracts::ACCOUNT_CONFIG_INIT_CODE_HASH),
            (Eip8130Contracts::DEFAULT_ACCOUNT, Eip8130Contracts::DEFAULT_ACCOUNT_INIT_CODE_HASH),
            (
                Eip8130Contracts::DEFAULT_HIGH_RATE_ACCOUNT,
                Eip8130Contracts::DEFAULT_HIGH_RATE_ACCOUNT_INIT_CODE_HASH,
            ),
            (
                Eip8130Contracts::P256_AUTHENTICATOR,
                Eip8130Contracts::P256_AUTHENTICATOR_INIT_CODE_HASH,
            ),
            (
                Eip8130Contracts::WEBAUTHN_AUTHENTICATOR,
                Eip8130Contracts::WEBAUTHN_AUTHENTICATOR_INIT_CODE_HASH,
            ),
            (
                Eip8130Contracts::DELEGATE_AUTHENTICATOR,
                Eip8130Contracts::DELEGATE_AUTHENTICATOR_INIT_CODE_HASH,
            ),
        ];
        for (expected, init_code_hash) in cases {
            let derived =
                Eip8130Contracts::CREATE2_FACTORY.create2(Eip8130Contracts::SALT, init_code_hash);
            assert_eq!(derived, expected, "CREATE2 derivation mismatch for {expected}");
        }
    }

    #[test]
    fn canonical_authenticator_membership() {
        assert_eq!(Eip8130Contracts::CANONICAL_AUTHENTICATORS.len(), 3);
        for auth in Eip8130Contracts::CANONICAL_AUTHENTICATORS {
            assert!(Eip8130Contracts::is_canonical_authenticator(&auth));
        }
        assert!(!Eip8130Contracts::is_canonical_authenticator(&Address::ZERO));
    }
}
