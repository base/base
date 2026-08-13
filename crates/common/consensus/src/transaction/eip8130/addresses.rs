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
    pub const ACCOUNT_CONFIG: Address = address!("0x81305d4f4976220D2af17E5Dc246848E235600AC");

    /// Per-contract mined CREATE2 salt for [`Self::ACCOUNT_CONFIG`], yielding its
    /// `0x8130…` vanity address.
    pub const ACCOUNT_CONFIG_SALT: B256 =
        b256!("0xf8341777f1a47fdc9c6bbb706dfa8e4f44580ec92fb58e3aae32d77c9d6039a8");

    /// keccak256 of the `ACCOUNT_CONFIG` deployment init code (for CREATE2
    /// derivation and bytecode-drift detection).
    pub const ACCOUNT_CONFIG_INIT_CODE_HASH: B256 =
        b256!("0xd5fec2364f479536d9ac6412580b918a6094bdab5f5a8c66e2f77b8ad5d33536");

    // ─────────────────────────────────────────────────────────────────────────
    // Account implementations (init code embeds `ACCOUNT_CONFIG`)
    // ─────────────────────────────────────────────────────────────────────────

    /// Default wallet implementation, used as the target of default EOA delegation.
    pub const DEFAULT_ACCOUNT: Address = address!("0x813078f98b3eb214046C8Dc93A771ac9de5AaDEf");

    /// Per-contract mined CREATE2 salt for [`Self::DEFAULT_ACCOUNT`].
    pub const DEFAULT_ACCOUNT_SALT: B256 =
        b256!("0x0000000000000000000000000000000000000000000000000000000139a99218");

    /// keccak256 of the `DEFAULT_ACCOUNT` deployment init code.
    pub const DEFAULT_ACCOUNT_INIT_CODE_HASH: B256 =
        b256!("0xc8d4dce2ca2004fc9e2ed3c1955a7cb27cacae31ea44c5e60608895f70edda2c");

    /// Canonical high-rate payer account implementation
    /// (`CanonicalHighRatePayerAccount`). Wallets that block ETH transfers when
    /// locked, granting higher EIP-8130 mempool access (rate limits).
    pub const CANONICAL_HIGH_RATE_PAYER_ACCOUNT: Address =
        address!("0x8130931874c894aC4963e128D6273AE520dAFa57");

    /// Per-contract mined CREATE2 salt for [`Self::CANONICAL_HIGH_RATE_PAYER_ACCOUNT`].
    pub const CANONICAL_HIGH_RATE_PAYER_ACCOUNT_SALT: B256 =
        b256!("0x00000000000000000000000000000000000000000000000000000000ac6f081b");

    /// keccak256 of the `CANONICAL_HIGH_RATE_PAYER_ACCOUNT` deployment init code.
    pub const CANONICAL_HIGH_RATE_PAYER_ACCOUNT_INIT_CODE_HASH: B256 =
        b256!("0x73c468cb90dce847524dfcec5acb6a0c152885c38e810311241142f13594ca4b");

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
        b256!("0xd48297a99e6d846a9b112783ed7c41c036d99709fbcaef3c205c3d16715768bf");

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
        address!("0x8130b7D430D041ED4050935814D493299980aDE1");

    /// Per-contract mined CREATE2 salt for [`Self::DELEGATE_AUTHENTICATOR`].
    pub const DELEGATE_AUTHENTICATOR_SALT: B256 =
        b256!("0x000000000000000000000000000000000000000000000000000000006f100b8d");

    /// keccak256 of the `DELEGATE_AUTHENTICATOR` deployment init code.
    pub const DELEGATE_AUTHENTICATOR_INIT_CODE_HASH: B256 =
        b256!("0x6060f8131b34060e3046ba3dda205f07c1c2f93ae956b3d3d2166a8b7ee09336");

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
