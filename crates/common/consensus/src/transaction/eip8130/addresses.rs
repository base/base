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
    pub const ACCOUNT_CONFIG: Address = address!("0xe7Bb8eF3728ea9f0A8be6D7e9585FeAb12dE086A");

    /// keccak256 of the `ACCOUNT_CONFIG` deployment init code (for CREATE2
    /// derivation and bytecode-drift detection).
    pub const ACCOUNT_CONFIG_INIT_CODE_HASH: B256 =
        b256!("0x7c04a9931efd384c64c7895cf0a254dfdaf3c1d650e23cab1480dac7840633bd");

    // ─────────────────────────────────────────────────────────────────────────
    // Account implementations (init code embeds `ACCOUNT_CONFIG`)
    // ─────────────────────────────────────────────────────────────────────────

    /// Default wallet implementation, used as the target of default EOA delegation.
    pub const DEFAULT_ACCOUNT: Address = address!("0xDd802113C9FF6964cD2A61A16e075D5271cC82c9");

    /// keccak256 of the `DEFAULT_ACCOUNT` deployment init code.
    pub const DEFAULT_ACCOUNT_INIT_CODE_HASH: B256 =
        b256!("0xa1b68747f3b48894ee02612a0f217b97ed76b1643ea791cb46637c10b4a21595");

    /// Wallet variant that blocks ETH transfers when locked, granting higher
    /// EIP-8130 mempool access (rate limits).
    pub const DEFAULT_HIGH_RATE_ACCOUNT: Address =
        address!("0xe5edfB7E7365893d685c2FbFBAC3e022f51d942F");

    /// keccak256 of the `DEFAULT_HIGH_RATE_ACCOUNT` deployment init code.
    pub const DEFAULT_HIGH_RATE_ACCOUNT_INIT_CODE_HASH: B256 =
        b256!("0xe77be0ab8bf7c2cb2743825182e7aa7f11f351c62cbf1b9fe74c9d43075c6ac6");

    // ─────────────────────────────────────────────────────────────────────────
    // Canonical authenticators (accepted on the EIP-8130 block-validation path)
    // ─────────────────────────────────────────────────────────────────────────

    // Note: secp256k1 has no contract entry. It is the protocol-reserved native
    // k1 sentinel
    // [`Eip8130Constants::K1_AUTHENTICATOR`](super::Eip8130Constants::K1_AUTHENTICATOR)
    // (`address(1)`), handled directly by the protocol, not a deployed contract.

    /// secp256r1 / P-256 (raw) authenticator contract.
    pub const P256_AUTHENTICATOR: Address = address!("0xf8847a74F8067CabaE5fe56B70b372A7D670f0f8");

    /// keccak256 of the `P256_AUTHENTICATOR` deployment init code.
    pub const P256_AUTHENTICATOR_INIT_CODE_HASH: B256 =
        b256!("0x64a6e7ca64d1043c5a9f6c4072ae3e06989b88f7a63df3cbbe4d717763c8b65a");

    /// secp256r1 / P-256 (`WebAuthn`) authenticator contract.
    pub const WEBAUTHN_AUTHENTICATOR: Address =
        address!("0x871c72d3950308A028E9c4917591bcfd3D6a1EF7");

    /// keccak256 of the `WEBAUTHN_AUTHENTICATOR` deployment init code.
    pub const WEBAUTHN_AUTHENTICATOR_INIT_CODE_HASH: B256 =
        b256!("0x92bc05424ceb5ef1f1ad17e1d462d45fff83f76daebeef2d5ff1cf0b80733a26");

    /// Delegated-validation (1-hop) authenticator contract (init code embeds
    /// `ACCOUNT_CONFIG`).
    pub const DELEGATE_AUTHENTICATOR: Address =
        address!("0x1B0195ba5E3FCdB387DD619816eeF8b510Ed0855");

    /// keccak256 of the `DELEGATE_AUTHENTICATOR` deployment init code.
    pub const DELEGATE_AUTHENTICATOR_INIT_CODE_HASH: B256 =
        b256!("0x483e360a8f1d8e2d891c1c260d164e433e2080080e111fa8bb78e0f4e8e3f876");

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
