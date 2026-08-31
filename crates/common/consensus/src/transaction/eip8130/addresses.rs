//! Canonical [EIP-8130] system-contract addresses and the node authenticator allowlist.
//!
//! These are the deterministic CREATE2 addresses of the EIP-8130 contracts (the
//! Account Configuration system contract, the account implementations, and the
//! canonical authenticators). Every contract is deployed through Nick's
//! deterministic-deployment proxy ([`Eip8130Contracts::CREATE2_FACTORY`]) with a
//! **per-contract mined salt**, so each address is a pure function of the
//! contract init code (under its own salt) and is identical on every chain that
//! deploys the same bytecode. Each salt is mined individually so that every
//! contract shares the `0x8130…` vanity prefix.
//!
//! # ⚠️ These values track the contract bytecode
//!
//! Because each address is derived from the contract init code, **any change to
//! the contract bytecode changes its address**, and the account-implementation
//! and delegate-authenticator addresses additionally cascade off the Account
//! Configuration address (it is passed as a constructor argument). On each
//! redeploy, re-pin the address, its `*_SALT`, and its `*_INIT_CODE_HASH`
//! together — the [`tests`] module asserts each triple stays consistent under
//! CREATE2 (`address == create2(factory, salt, init_code_hash)`).
//!
//! [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130

use alloy_primitives::{Address, B256, address, b256, keccak256};
use alloy_sol_types::sol;

/// Canonical [EIP-8130] contract addresses and the node authenticator allowlist.
///
/// See the [module docs](self) for the caveat that each address is derived from
/// its per-contract salt and init code, and changes with the contract bytecode.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Eip8130Contracts;

impl Eip8130Contracts {
    /// Nick's deterministic-deployment proxy (the "Arachnid" CREATE2 factory),
    /// deployed at the same address on every EVM chain. Every EIP-8130 contract is
    /// deployed by sending `salt || init_code` to this factory.
    ///
    /// <https://github.com/Arachnid/deterministic-deployment-proxy>
    pub const CREATE2_FACTORY: Address = address!("0x4e59b44847b379578588920cA78FbF26c0B4956C");

    // ─────────────────────────────────────────────────────────────────────────
    // System contract
    // ─────────────────────────────────────────────────────────────────────────

    /// Account Configuration system contract (`ACCOUNT_CONFIG_ADDRESS`). The
    /// protocol reads actor/account state directly from this contract's storage.
    pub const ACCOUNT_CONFIG: Address = address!("0x813079205F4cFCC0bE8166be1C7F863Db8A700AC");

    /// Per-contract mined CREATE2 salt for [`Self::ACCOUNT_CONFIG`], yielding its
    /// `0x8130…` vanity address.
    pub const ACCOUNT_CONFIG_SALT: B256 =
        b256!("0x544ebe415c98e81edc2101578e9389396d270c2af154129832b643549b08a819");

    /// keccak256 of the `ACCOUNT_CONFIG` deployment init code (for CREATE2
    /// derivation and bytecode-drift detection).
    pub const ACCOUNT_CONFIG_INIT_CODE_HASH: B256 =
        b256!("0x70eb4641a426e42492d0730a934d831dbe0963321fb55860d6246c35145349da");

    // ─────────────────────────────────────────────────────────────────────────
    // Account implementations (init code embeds `ACCOUNT_CONFIG`)
    // ─────────────────────────────────────────────────────────────────────────

    /// Default wallet implementation, used as the target of default EOA delegation.
    pub const DEFAULT_ACCOUNT: Address = address!("0x8130f53536097991a3AD949D9a1D76E1b666aDEf");

    /// Per-contract mined CREATE2 salt for [`Self::DEFAULT_ACCOUNT`].
    pub const DEFAULT_ACCOUNT_SALT: B256 =
        b256!("0x000000000000000000000000000000000000000000000000000000030924c784");

    /// keccak256 of the `DEFAULT_ACCOUNT` deployment init code.
    pub const DEFAULT_ACCOUNT_INIT_CODE_HASH: B256 =
        b256!("0xf99c7ecb38528e8f0b209f476b3cbbba8f4f8f83161e9e54945e9a041ee8ee2b");

    /// Canonical high-rate payer account implementation
    /// (`CanonicalHighRatePayerAccount`). Wallets that block ETH transfers when
    /// locked, granting higher EIP-8130 mempool access (rate limits).
    pub const CANONICAL_HIGH_RATE_PAYER_ACCOUNT: Address =
        address!("0x8130A75698ace565afeae1fa36a327248D93fA57");

    /// Per-contract mined CREATE2 salt for [`Self::CANONICAL_HIGH_RATE_PAYER_ACCOUNT`].
    pub const CANONICAL_HIGH_RATE_PAYER_ACCOUNT_SALT: B256 =
        b256!("0x00000000000000000000000000000000000000000000000000000001f53faf75");

    /// keccak256 of the `CANONICAL_HIGH_RATE_PAYER_ACCOUNT` deployment init code.
    pub const CANONICAL_HIGH_RATE_PAYER_ACCOUNT_INIT_CODE_HASH: B256 =
        b256!("0x92cddd461fb1c1ab9297cef0ff86ed2d59d9c14a6399c5e71d262e5e2dade5c4");

    /// keccak256 of the ERC-1167 minimal-proxy *runtime* bytecode whose
    /// implementation is [`Self::CANONICAL_HIGH_RATE_PAYER_ACCOUNT`]:
    ///
    /// ```text
    /// 0x363d3d373d3d3d363d73 <implementation> 5af43d82803e903d91602b57fd5bf3
    /// ```
    ///
    /// Used to recognize high-rate payer accounts by codehash (e.g. mempool
    /// admission) without resolving an EIP-7702 delegation target.
    pub const CANONICAL_HIGH_RATE_PAYER_PROXY_CODE_HASH: B256 =
        b256!("0x029d2e31f6309cdefe6a1514785cd001d123ecf88fd79e4ba2e3801fd8b81cf9");

    // ─────────────────────────────────────────────────────────────────────────
    // Canonical authenticators (accepted on the EIP-8130 block-validation path)
    // ─────────────────────────────────────────────────────────────────────────

    // Note: secp256k1 has no contract entry. It is the protocol-reserved native
    // k1 sentinel
    // [`Eip8130Constants::K1_AUTHENTICATOR`](super::Eip8130Constants::K1_AUTHENTICATOR)
    // (`address(1)`), handled directly by the protocol, not a deployed contract.

    /// secp256r1 / P-256 (raw) authenticator contract.
    pub const P256_AUTHENTICATOR: Address = address!("0x8130C89F65750431b564A4730397552a11CeA256");

    /// Per-contract mined CREATE2 salt for [`Self::P256_AUTHENTICATOR`].
    pub const P256_AUTHENTICATOR_SALT: B256 =
        b256!("0x000000000000000000000000000000000000000000000000000000014139e07b");

    /// keccak256 of the `P256_AUTHENTICATOR` deployment init code.
    pub const P256_AUTHENTICATOR_INIT_CODE_HASH: B256 =
        b256!("0x64a6e7ca64d1043c5a9f6c4072ae3e06989b88f7a63df3cbbe4d717763c8b65a");

    /// secp256r1 / P-256 (`WebAuthn`) authenticator contract.
    pub const WEBAUTHN_AUTHENTICATOR: Address =
        address!("0x813007b6b1b48E75D91dEc5927ab515d12a0F1d0");

    /// Per-contract mined CREATE2 salt for [`Self::WEBAUTHN_AUTHENTICATOR`].
    pub const WEBAUTHN_AUTHENTICATOR_SALT: B256 =
        b256!("0x000000000000000000000000000000000000000000000000000000015ec496a4");

    /// keccak256 of the `WEBAUTHN_AUTHENTICATOR` deployment init code.
    pub const WEBAUTHN_AUTHENTICATOR_INIT_CODE_HASH: B256 =
        b256!("0x92bc05424ceb5ef1f1ad17e1d462d45fff83f76daebeef2d5ff1cf0b80733a26");

    /// Delegated-validation (1-hop) authenticator contract (init code embeds
    /// `ACCOUNT_CONFIG`).
    pub const DELEGATE_AUTHENTICATOR: Address =
        address!("0x8130fB676B7e718AE6a097b62eA271915B9bade1");

    /// Per-contract mined CREATE2 salt for [`Self::DELEGATE_AUTHENTICATOR`].
    pub const DELEGATE_AUTHENTICATOR_SALT: B256 =
        b256!("0x00000000000000000000000000000000000000000000000000000000115250c1");

    /// keccak256 of the `DELEGATE_AUTHENTICATOR` deployment init code.
    pub const DELEGATE_AUTHENTICATOR_INIT_CODE_HASH: B256 =
        b256!("0x027ef6d7ad2f6f84b9aece32ca0db7aba30e1e83890a6a82fc6a357a4e2c8679");

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

    /// The 10-byte ERC-1167 minimal-proxy runtime prefix (before the 20-byte
    /// implementation address).
    pub const ERC1167_PREFIX: [u8; 10] =
        [0x36, 0x3d, 0x3d, 0x37, 0x3d, 0x3d, 0x3d, 0x36, 0x3d, 0x73];

    /// The 15-byte ERC-1167 minimal-proxy runtime suffix (after the 20-byte
    /// implementation address).
    pub const ERC1167_SUFFIX: [u8; 15] =
        [0x5a, 0xf4, 0x3d, 0x82, 0x80, 0x3e, 0x90, 0x3d, 0x91, 0x60, 0x2b, 0x57, 0xfd, 0x5b, 0xf3];

    /// Builds the ERC-1167 minimal-proxy runtime bytecode for `implementation`.
    #[must_use]
    pub fn erc1167_proxy_runtime(implementation: Address) -> [u8; 45] {
        let mut code = [0u8; 45];
        code[..10].copy_from_slice(&Self::ERC1167_PREFIX);
        code[10..30].copy_from_slice(implementation.as_slice());
        code[30..].copy_from_slice(&Self::ERC1167_SUFFIX);
        code
    }

    /// The account code hash of the canonical ERC-1167 minimal-proxy *runtime*
    /// for `implementation` — i.e. `keccak256(erc1167_proxy_runtime(impl))`.
    ///
    /// This is the immutable, deployment-independent fingerprint used to
    /// recognize high-rate payer accounts (and other trusted delegations) by
    /// their on-chain code hash, without fetching or parsing the bytecode. For
    /// [`Self::CANONICAL_HIGH_RATE_PAYER_ACCOUNT`] it equals
    /// [`Self::CANONICAL_HIGH_RATE_PAYER_PROXY_CODE_HASH`].
    ///
    /// An EIP-7702 delegation (`0xef0100 ‖ impl`) has a different code hash and
    /// therefore can never match — which is intentional: only immutable proxy
    /// deployments carry the enshrined "block ETH transfers while locked"
    /// guarantee the balance-bounded mempool admission relies on.
    #[must_use]
    pub fn erc1167_proxy_code_hash(implementation: Address) -> B256 {
        keccak256(Self::erc1167_proxy_runtime(implementation))
    }
}

sol! {
    /// ABI of [`Eip8130Contracts::DEFAULT_ACCOUNT`] (`execute` / `executeBatch`).
    interface IDefaultAccount {
        /// Inner call of `executeBatch`.
        struct Call {
            address target;
            uint256 value;
            bytes data;
        }

        /// Single call from the account; equivalent to a one-element batch.
        function execute(address target, uint256 value, bytes data) external;

        /// Ordered batch of calls from the account.
        function executeBatch(Call[] calls) external;
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;

    use super::*;

    /// Each `(address, salt, init_code_hash)` triple must be self-consistent
    /// under CREATE2 with the canonical factory. If the contract bytecode
    /// changes, all three values must be re-pinned together or this fails — the
    /// drift guard for the deployed addresses.
    #[test]
    fn addresses_match_create2_derivation() {
        let cases = [
            (
                Eip8130Contracts::ACCOUNT_CONFIG,
                Eip8130Contracts::ACCOUNT_CONFIG_SALT,
                Eip8130Contracts::ACCOUNT_CONFIG_INIT_CODE_HASH,
            ),
            (
                Eip8130Contracts::DEFAULT_ACCOUNT,
                Eip8130Contracts::DEFAULT_ACCOUNT_SALT,
                Eip8130Contracts::DEFAULT_ACCOUNT_INIT_CODE_HASH,
            ),
            (
                Eip8130Contracts::CANONICAL_HIGH_RATE_PAYER_ACCOUNT,
                Eip8130Contracts::CANONICAL_HIGH_RATE_PAYER_ACCOUNT_SALT,
                Eip8130Contracts::CANONICAL_HIGH_RATE_PAYER_ACCOUNT_INIT_CODE_HASH,
            ),
            (
                Eip8130Contracts::P256_AUTHENTICATOR,
                Eip8130Contracts::P256_AUTHENTICATOR_SALT,
                Eip8130Contracts::P256_AUTHENTICATOR_INIT_CODE_HASH,
            ),
            (
                Eip8130Contracts::WEBAUTHN_AUTHENTICATOR,
                Eip8130Contracts::WEBAUTHN_AUTHENTICATOR_SALT,
                Eip8130Contracts::WEBAUTHN_AUTHENTICATOR_INIT_CODE_HASH,
            ),
            (
                Eip8130Contracts::DELEGATE_AUTHENTICATOR,
                Eip8130Contracts::DELEGATE_AUTHENTICATOR_SALT,
                Eip8130Contracts::DELEGATE_AUTHENTICATOR_INIT_CODE_HASH,
            ),
        ];
        for (expected, salt, init_code_hash) in cases {
            let derived = Eip8130Contracts::CREATE2_FACTORY.create2(salt, init_code_hash);
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

    #[test]
    fn high_rate_payer_proxy_code_hash_matches_erc1167_runtime() {
        let runtime = Eip8130Contracts::erc1167_proxy_runtime(
            Eip8130Contracts::CANONICAL_HIGH_RATE_PAYER_ACCOUNT,
        );
        assert_eq!(runtime.len(), 45);
        assert_eq!(keccak256(runtime), Eip8130Contracts::CANONICAL_HIGH_RATE_PAYER_PROXY_CODE_HASH);
    }

    #[test]
    fn erc1167_proxy_code_hash_matches_canonical_and_distinguishes_impls() {
        // The canonical high-rate payer proxy code hash matches the pinned const.
        let impl_addr = Eip8130Contracts::CANONICAL_HIGH_RATE_PAYER_ACCOUNT;
        assert_eq!(
            Eip8130Contracts::erc1167_proxy_code_hash(impl_addr),
            Eip8130Contracts::CANONICAL_HIGH_RATE_PAYER_PROXY_CODE_HASH,
        );
        // It equals the code hash of the concrete runtime bytecode.
        let runtime = Eip8130Contracts::erc1167_proxy_runtime(impl_addr);
        assert_eq!(Eip8130Contracts::erc1167_proxy_code_hash(impl_addr), keccak256(runtime));

        // A different implementation yields a different code hash (the embedded
        // address changes the runtime, so distinct impls never collide).
        let other = Eip8130Contracts::DEFAULT_ACCOUNT;
        assert_ne!(
            Eip8130Contracts::erc1167_proxy_code_hash(other),
            Eip8130Contracts::erc1167_proxy_code_hash(impl_addr),
        );

        // An EIP-7702 designator (`0xef0100 || impl`) hashes to something else,
        // so a delegated EOA can never match a trusted proxy code hash.
        let mut eip7702 = vec![0xef, 0x01, 0x00];
        eip7702.extend_from_slice(impl_addr.as_slice());
        assert_ne!(keccak256(&eip7702), Eip8130Contracts::erc1167_proxy_code_hash(impl_addr));
    }
}
