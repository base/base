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
    pub const ACCOUNT_CONFIG: Address = address!("0xb0198a714872EE5bfDF829e7986DB5C5899a6b50");

    /// keccak256 of the `ACCOUNT_CONFIG` deployment init code (for CREATE2
    /// derivation and bytecode-drift detection).
    pub const ACCOUNT_CONFIG_INIT_CODE_HASH: B256 =
        b256!("0x6c3a49f4636ff758e77f9213fd57c8e5a55677545e31d99441ec173f44a6f518");

    // ─────────────────────────────────────────────────────────────────────────
    // Account implementations (init code embeds `ACCOUNT_CONFIG`)
    // ─────────────────────────────────────────────────────────────────────────

    /// Default wallet implementation, used as the target of default EOA delegation.
    pub const DEFAULT_ACCOUNT: Address = address!("0x124b52d5D57a76ed064c414975beA11Beffe0251");

    /// keccak256 of the `DEFAULT_ACCOUNT` deployment init code.
    pub const DEFAULT_ACCOUNT_INIT_CODE_HASH: B256 =
        b256!("0xcabcfd4783f18c6b043f02dbea18dc611fb9fec737477884620a83e2de25c898");

    /// Wallet variant that blocks ETH transfers when locked, granting higher
    /// EIP-8130 mempool access (rate limits).
    pub const DEFAULT_HIGH_RATE_ACCOUNT: Address =
        address!("0x13dD0F222cCF60B7C08a95C2d1FcC85A38DD675D");

    /// keccak256 of the `DEFAULT_HIGH_RATE_ACCOUNT` deployment init code.
    pub const DEFAULT_HIGH_RATE_ACCOUNT_INIT_CODE_HASH: B256 =
        b256!("0x9496825d45d5185c429df2ee447c2a47b0b3240ef91bfbfe82a2e55a71815bd3");

    // ─────────────────────────────────────────────────────────────────────────
    // Canonical authenticators (accepted on the EIP-8130 block-validation path)
    // ─────────────────────────────────────────────────────────────────────────

    /// secp256k1 (ECDSA) authenticator contract.
    ///
    /// Note: native EOA ecrecover is the separate protocol-reserved sentinel
    /// [`Eip8130Constants::ECRECOVER_AUTHENTICATOR`](super::Eip8130Constants::ECRECOVER_AUTHENTICATOR)
    /// (`address(1)`); this is the deployed `IAuthenticator` contract form.
    pub const K1_AUTHENTICATOR: Address = address!("0x39221FB37Df105B22316328e88632C9684861466");

    /// keccak256 of the `K1_AUTHENTICATOR` deployment init code.
    pub const K1_AUTHENTICATOR_INIT_CODE_HASH: B256 =
        b256!("0x07a31dfd4ba2e2a529d9642b98430ea299bac428fe83312c54d16f519568d7d5");

    /// secp256r1 / P-256 (raw) authenticator contract.
    pub const P256_AUTHENTICATOR: Address = address!("0x3AE129D846CD1CAf0369b4Caa56c188E18E11B15");

    /// keccak256 of the `P256_AUTHENTICATOR` deployment init code.
    pub const P256_AUTHENTICATOR_INIT_CODE_HASH: B256 =
        b256!("0x87b9a0dbffce118797e1d133f563f99697396c9b447becb900157f60197adea0");

    /// secp256r1 / P-256 (`WebAuthn`) authenticator contract.
    pub const WEBAUTHN_AUTHENTICATOR: Address =
        address!("0x1CB75BE39Fb950202BF4239010534B86EdA66c31");

    /// keccak256 of the `WEBAUTHN_AUTHENTICATOR` deployment init code.
    pub const WEBAUTHN_AUTHENTICATOR_INIT_CODE_HASH: B256 =
        b256!("0xbaeb605566ca94a5d7af1bb61996e409d457ac3d21a2cda682f08ee46a356ef5");

    /// Delegated-validation (1-hop) authenticator contract (init code embeds
    /// `ACCOUNT_CONFIG`).
    pub const DELEGATE_AUTHENTICATOR: Address =
        address!("0xE67D299Ff3F0a185398B6C5a28998696969265d7");

    /// keccak256 of the `DELEGATE_AUTHENTICATOR` deployment init code.
    pub const DELEGATE_AUTHENTICATOR_INIT_CODE_HASH: B256 =
        b256!("0xa1980fff3bbb86a0d4ea4593ae921c8292f4b33254851db8406b098eb5db49ec");

    /// The canonical authenticator allowlist: the deployed `IAuthenticator`
    /// contracts a compliant node accepts on the EIP-8130 block-validation path.
    ///
    /// Native EOA ecrecover (`address(1)`) is a separate protocol-reserved path
    /// and is not a deployed contract, so it is not listed here.
    ///
    /// The exact membership (including whether the native ecrecover sentinel and
    /// the `K1_AUTHENTICATOR` contract are both accepted, and how enshrinement is
    /// metered) is TBD pending the spec's companion ERC and contract finalization.
    pub const CANONICAL_AUTHENTICATORS: [Address; 4] = [
        Self::K1_AUTHENTICATOR,
        Self::P256_AUTHENTICATOR,
        Self::WEBAUTHN_AUTHENTICATOR,
        Self::DELEGATE_AUTHENTICATOR,
    ];

    /// Returns `true` if `authenticator` is in the canonical deployed-contract
    /// allowlist ([`Self::CANONICAL_AUTHENTICATORS`]).
    ///
    /// This intentionally does not account for the native ecrecover sentinel
    /// (`address(1)`), which is handled separately by the protocol; see the
    /// allowlist docs for the TBD around final membership.
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
            (Eip8130Contracts::K1_AUTHENTICATOR, Eip8130Contracts::K1_AUTHENTICATOR_INIT_CODE_HASH),
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
        assert_eq!(Eip8130Contracts::CANONICAL_AUTHENTICATORS.len(), 4);
        for auth in Eip8130Contracts::CANONICAL_AUTHENTICATORS {
            assert!(Eip8130Contracts::is_canonical_authenticator(&auth));
        }
        assert!(!Eip8130Contracts::is_canonical_authenticator(&Address::ZERO));
    }
}
