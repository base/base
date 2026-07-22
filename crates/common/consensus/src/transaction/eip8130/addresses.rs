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

use alloy_primitives::{Address, B256, address, b256, keccak256};

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
    pub const ACCOUNT_CONFIG: Address = address!("0x53648Cf00356fbAA1F2B531715c6B64AaBDE1555");

    /// keccak256 of the `ACCOUNT_CONFIG` deployment init code (for CREATE2
    /// derivation and bytecode-drift detection).
    pub const ACCOUNT_CONFIG_INIT_CODE_HASH: B256 =
        b256!("0x7c9289d84f391aa48b89cffe374fac16926d7763b5647a832f0810fb9d98aef5");

    // ─────────────────────────────────────────────────────────────────────────
    // Account implementations (init code embeds `ACCOUNT_CONFIG`)
    // ─────────────────────────────────────────────────────────────────────────

    /// Default wallet implementation, used as the target of default EOA delegation.
    pub const DEFAULT_ACCOUNT: Address = address!("0x58da469ef71Dd4B092B010CdA37DE124C926EebD");

    /// keccak256 of the `DEFAULT_ACCOUNT` deployment init code.
    pub const DEFAULT_ACCOUNT_INIT_CODE_HASH: B256 =
        b256!("0x9ec5fba8d1093ed7edd44bf786513788d5f7b0fc8a29d2b43f8b509f022706b3");

    /// Canonical high-rate payer account implementation
    /// (`CanonicalHighRatePayerAccount`). Wallets that block ETH transfers when
    /// locked, granting higher EIP-8130 mempool access (rate limits).
    pub const CANONICAL_HIGH_RATE_PAYER_ACCOUNT: Address =
        address!("0x23Fe6949d6370330Ae32e7c17E1265D65955C92a");

    /// keccak256 of the `CANONICAL_HIGH_RATE_PAYER_ACCOUNT` deployment init code.
    pub const CANONICAL_HIGH_RATE_PAYER_ACCOUNT_INIT_CODE_HASH: B256 =
        b256!("0xb694f9cf053eaf4d5a778c60d67d50fb08bf41d88c817904e1295333040980ac");

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
        b256!("0x3e37c64c39476e47c52408ea45eb3ae0f07e3ca0fd1d713acbcf17bf3b51312c");

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
        address!("0xbb73E3871FBaC8aef1a7Ee8A24E21139916f14C2");

    /// keccak256 of the `DELEGATE_AUTHENTICATOR` deployment init code.
    pub const DELEGATE_AUTHENTICATOR_INIT_CODE_HASH: B256 =
        b256!("0x27e4ead86d2bf8cebc501d9e0c795a139c4616d972152c4f79fa503b84fb4170");

    /// Always-valid authenticator (keyless relay / test). Deployed alongside the
    /// canonical set but **not** on the node block-validation allowlist
    /// ([`Self::CANONICAL_AUTHENTICATORS`]).
    pub const ALWAYS_VALID_AUTHENTICATOR: Address =
        address!("0xA550545Da91720c23483c5B3493412A02D1cF9F9");

    /// keccak256 of the `ALWAYS_VALID_AUTHENTICATOR` deployment init code.
    pub const ALWAYS_VALID_AUTHENTICATOR_INIT_CODE_HASH: B256 =
        b256!("0xc45c91538660545608577c119aeffc5a5550becf4cd2a8710d8368fce6a6b27a");

    /// The canonical authenticator allowlist: the deployed `IAuthenticator`
    /// contracts a compliant node accepts on the EIP-8130 block-validation path.
    ///
    /// secp256k1 is **not** a contract entry here: on EIP-8130 chains it is the
    /// protocol-reserved native ecrecover sentinel (`address(1)`), handled
    /// directly by the protocol rather than via a deployed authenticator contract.
    /// [`Self::ALWAYS_VALID_AUTHENTICATOR`] is also excluded (test / keyless relay).
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
                Eip8130Contracts::CANONICAL_HIGH_RATE_PAYER_ACCOUNT,
                Eip8130Contracts::CANONICAL_HIGH_RATE_PAYER_ACCOUNT_INIT_CODE_HASH,
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
            (
                Eip8130Contracts::ALWAYS_VALID_AUTHENTICATOR,
                Eip8130Contracts::ALWAYS_VALID_AUTHENTICATOR_INIT_CODE_HASH,
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
        // Deployed, but not on the node allowlist.
        assert!(!Eip8130Contracts::is_canonical_authenticator(
            &Eip8130Contracts::ALWAYS_VALID_AUTHENTICATOR
        ));
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
