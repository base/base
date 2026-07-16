use alloc::{string::String, vec, vec::Vec};

use alloy_primitives::{Address, B256, FixedBytes, U256, keccak256};
use alloy_sol_types::SolValue;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{IB20, TokenAccounting, Transferable};

/// ERC-5267 `eip712Domain()` return tuple: (fields, name, version, chainId, verifyingContract, salt, extensions).
pub type Eip712Domain = (FixedBytes<1>, String, String, U256, Address, B256, Vec<U256>);

/// Arguments for [`Permittable::permit`], grouping the EIP-2612 ABI fields.
#[derive(Clone, Debug)]
pub struct PermitArgs {
    /// Token owner whose allowance is being set.
    pub owner: Address,
    /// Account being granted the allowance.
    pub spender: Address,
    /// Allowance amount.
    pub value: U256,
    /// Unix timestamp after which the signature is no longer valid.
    pub deadline: U256,
    /// Signature recovery id: 27 or 28.
    pub v: u8,
    /// Signature `r` component.
    pub r: B256,
    /// Signature `s` component.
    pub s: B256,
}

impl PermitArgs {
    /// `keccak256("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)")`
    pub const TYPEHASH: B256 =
        alloy_primitives::b256!("6e71edae12b1b97f4d1f60370fef10105fa2faae0126114a169c64845d6126c9");

    /// EIP-191 prefix for structured data, followed by the EIP-712 version byte.
    pub const EIP712_SIGNING_PREFIX: [u8; 2] = [0x19, 0x01];

    /// Legacy `v` value for even-Y ECDSA recovery (`ecrecover`).
    pub const RECOVERY_ID_EVEN_Y: u8 = 27;
    /// Legacy `v` value for odd-Y ECDSA recovery (`ecrecover`).
    pub const RECOVERY_ID_ODD_Y: u8 = 28;

    /// Hashes the EIP-2612 `Permit` struct for `nonce`.
    pub fn struct_hash(&self, nonce: U256) -> B256 {
        keccak256(
            (Self::TYPEHASH, self.owner, self.spender, self.value, nonce, self.deadline)
                .abi_encode(),
        )
    }

    /// Builds the EIP-712 signing digest: `keccak256("\x19\x01" ‖ domainSeparator ‖ structHash)`.
    pub fn signing_hash(&self, domain_separator: B256, nonce: U256) -> B256 {
        let struct_hash = self.struct_hash(nonce);
        let mut buf = [0u8; 66];
        buf[..2].copy_from_slice(&Self::EIP712_SIGNING_PREFIX);
        buf[2..34].copy_from_slice(domain_separator.as_slice());
        buf[34..66].copy_from_slice(struct_hash.as_slice());
        keccak256(buf)
    }

    /// Validates a recovered ECDSA address against the declared `owner`.
    ///
    /// Returns `Err(InvalidSigner)` when `recovered` is `Address::ZERO` (matching Solidity's
    /// explicit zero-address guard) or when `recovered != owner`.
    pub fn validate_recovered_address(recovered: Address, owner: Address) -> Result<()> {
        if recovered.is_zero() || recovered != owner {
            return Err(BasePrecompileError::revert(IB20::InvalidSigner {
                signer: recovered,
                owner,
            }));
        }
        Ok(())
    }

    /// Maps Ethereum `v` (27/28) to secp256k1 recovery parity, then recovers the signer.
    pub fn recover_signer(&self, signing_hash: B256) -> Result<Address> {
        let odd_y_parity = match self.v {
            Self::RECOVERY_ID_EVEN_Y => false,
            Self::RECOVERY_ID_ODD_Y => true,
            _ => {
                return Err(BasePrecompileError::revert(IB20::InvalidSigner {
                    signer: Address::ZERO,
                    owner: self.owner,
                }));
            }
        };

        let sig =
            alloy_primitives::Signature::from_scalars_and_parity(self.r, self.s, odd_y_parity);
        sig.recover_address_from_prehash(&signing_hash).map_err(|_| {
            BasePrecompileError::revert(IB20::InvalidSigner {
                signer: Address::ZERO,
                owner: self.owner,
            })
        })
    }
}

// keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)")
const DOMAIN_TYPEHASH: B256 =
    alloy_primitives::b256!("8b73c3c69bb8fe3d512ecc4cf759cc79239f7b179b0ffacaa9a75d522b39400f");

/// EIP-712 domain version string pinned to `"1"`.
///
/// # Breaking change note
///
/// Adding `name` and `version` to the domain separator is an intentional, acknowledged breaking
/// change. Any permit signatures produced against the old domain (which only encoded `chainId` and
/// `verifyingContract`) will be invalid after this change. New tokens start with the canonical
/// four-field domain; existing token holders must re-sign outstanding permits.
const VERSION: &[u8] = b"1";

/// EIP-2612 permit and EIP-712 domain operations.
///
/// Requires [`Transferable`] since `permit` internally calls [`Transferable::approve`].
/// `token_address()` is inherited via `Permittable: Transferable: Token`.
pub trait Permittable: Transferable {
    /// Computes the EIP-712 domain separator for this token.
    ///
    /// Domain: `(name, version, chainId, verifyingContract)` — the canonical EIP-712 shape.
    /// `version` is pinned to `"1"`; `name` is read live from token storage so that
    /// a successful `updateName` invalidates outstanding permit signatures.
    fn domain_separator(&self, chain_id: u64) -> Result<B256> {
        let name = self.accounting().name()?;
        let name_hash = keccak256(name.as_bytes());
        let version_hash = keccak256(VERSION);
        let encoded =
            (DOMAIN_TYPEHASH, name_hash, version_hash, U256::from(chain_id), self.token_address())
                .abi_encode();
        Ok(keccak256(&encoded))
    }

    /// Returns the ERC-5267 `eip712Domain()` tuple for this token.
    fn eip712_domain(&self, chain_id: u64) -> Result<Eip712Domain> {
        let name = self.accounting().name()?;
        Ok((
            FixedBytes::<1>::from([0x0f]), // bits 0+1+2+3: name + version + chainId + verifyingContract
            name,
            String::from("1"),
            U256::from(chain_id),
            self.token_address(),
            B256::ZERO,
            vec![],
        ))
    }

    /// EIP-2612 permit. EOA signatures only (no ERC-1271).
    fn permit(&mut self, chain_id: u64, now: U256, args: PermitArgs) -> Result<()> {
        if now > args.deadline {
            return Err(BasePrecompileError::revert(IB20::ExpiredSignature {
                deadline: args.deadline,
            }));
        }

        let domain_sep = self.domain_separator(chain_id)?;
        let nonce = self.accounting().nonce(args.owner)?;
        let signing_hash = args.signing_hash(domain_sep, nonce);
        let recovered = args.recover_signer(signing_hash)?;
        PermitArgs::validate_recovered_address(recovered, args.owner)?;

        self.accounting_mut().increment_nonce(args.owner)?;
        self.approve(args.owner, args.spender, args.value)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256, keccak256};
    use alloy_sol_types::SolValue;
    use base_precompile_storage::BasePrecompileError;
    use k256::ecdsa::SigningKey;

    use crate::{
        FakePolicyAccounting, IB20, InMemoryTokenAccounting, PermitArgs, Permittable, TestToken,
        Token, TokenAccounting,
        common::ops::permittable::{DOMAIN_TYPEHASH, VERSION},
    };

    const CHAIN_ID: u64 = 1;
    const SPENDER: Address = Address::repeat_byte(0xbb);
    const TOKEN_ADDR: Address = Address::repeat_byte(1);
    const TOKEN_NAME: &str = "TestToken";

    // Anvil/Hardhat account 0 — well-known test key, never use in production.
    const PRIVATE_KEY: [u8; 32] =
        alloy_primitives::hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

    fn make_token() -> TestToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.name = TOKEN_NAME.to_string();
        TestToken::with_storage_and_policy(accounting, FakePolicyAccounting::new())
    }

    fn owner_address() -> Address {
        let key = SigningKey::from_slice(&PRIVATE_KEY).unwrap();
        let point = key.verifying_key().to_encoded_point(false);
        let hash = keccak256(&point.as_bytes()[1..]);
        Address::from_slice(&hash[12..])
    }

    fn sample_permit_args(owner: Address) -> PermitArgs {
        PermitArgs {
            owner,
            spender: SPENDER,
            value: U256::from(500u64),
            deadline: U256::MAX,
            v: PermitArgs::RECOVERY_ID_EVEN_Y,
            r: B256::ZERO,
            s: B256::ZERO,
        }
    }

    fn domain_separator_for_token(token: &TestToken, chain_id: u64) -> B256 {
        token.domain_separator(chain_id).unwrap()
    }

    fn signed_permit_args(
        token: &TestToken,
        owner: Address,
        spender: Address,
        value: U256,
        deadline: U256,
    ) -> PermitArgs {
        let domain_sep = domain_separator_for_token(token, CHAIN_ID);
        let nonce = token.accounting().nonce(owner).unwrap();
        let mut args =
            PermitArgs { owner, spender, value, deadline, v: 0, r: B256::ZERO, s: B256::ZERO };
        let signing_hash = args.signing_hash(domain_sep, nonce);

        let signing_key = SigningKey::from_slice(&PRIVATE_KEY).unwrap();
        let (sig, recid) = signing_key.sign_prehash_recoverable(signing_hash.as_slice()).unwrap();
        let sig_bytes = sig.to_bytes();
        args.r = B256::from_slice(&sig_bytes[..32]);
        args.s = B256::from_slice(&sig_bytes[32..]);
        args.v = if recid.is_y_odd() {
            PermitArgs::RECOVERY_ID_ODD_Y
        } else {
            PermitArgs::RECOVERY_ID_EVEN_Y
        };
        args
    }

    // ---- PermitArgs ----

    #[test]
    fn permit_args_struct_hash_matches_abi_encode() {
        let owner = owner_address();
        let args = sample_permit_args(owner);
        let nonce = U256::from(3u64);
        let expected = keccak256(
            (PermitArgs::TYPEHASH, owner, SPENDER, args.value, nonce, args.deadline).abi_encode(),
        );

        assert_eq!(args.struct_hash(nonce), expected);
    }

    #[test]
    fn permit_args_signing_hash_matches_eip712_digest() {
        let owner = owner_address();
        let args = sample_permit_args(owner);
        let nonce = U256::ZERO;
        let name_hash = keccak256(TOKEN_NAME.as_bytes());
        let version_hash = keccak256(VERSION);
        let domain_sep = keccak256(
            (DOMAIN_TYPEHASH, name_hash, version_hash, U256::from(CHAIN_ID), TOKEN_ADDR)
                .abi_encode(),
        );
        let struct_hash = args.struct_hash(nonce);
        let mut expected_preimage = [0u8; 66];
        expected_preimage[..2].copy_from_slice(&[0x19, 0x01]);
        expected_preimage[2..34].copy_from_slice(domain_sep.as_slice());
        expected_preimage[34..66].copy_from_slice(struct_hash.as_slice());

        assert_eq!(args.signing_hash(domain_sep, nonce), keccak256(expected_preimage));
    }

    #[test]
    fn permit_args_signing_hash_differs_by_nonce() {
        let owner = owner_address();
        let args = sample_permit_args(owner);
        let domain_sep = B256::repeat_byte(0x42);

        assert_ne!(
            args.signing_hash(domain_sep, U256::ZERO),
            args.signing_hash(domain_sep, U256::ONE)
        );
    }

    #[test]
    fn validate_recovered_address_rejects_zero_address() {
        let owner = Address::repeat_byte(0xaa);

        assert_eq!(
            PermitArgs::validate_recovered_address(Address::ZERO, owner).unwrap_err(),
            BasePrecompileError::revert(IB20::InvalidSigner { signer: Address::ZERO, owner })
        );
    }

    #[test]
    fn validate_recovered_address_rejects_wrong_signer() {
        let owner = Address::repeat_byte(0xaa);
        let wrong = Address::repeat_byte(0xbb);

        assert_eq!(
            PermitArgs::validate_recovered_address(wrong, owner).unwrap_err(),
            BasePrecompileError::revert(IB20::InvalidSigner { signer: wrong, owner })
        );
    }

    #[test]
    fn validate_recovered_address_accepts_matching_signer() {
        let owner = Address::repeat_byte(0xaa);
        PermitArgs::validate_recovered_address(owner, owner).unwrap();
    }

    #[test]
    fn permit_args_recover_signer_returns_owner() {
        let token = make_token();
        let owner = owner_address();
        let args = signed_permit_args(&token, owner, SPENDER, U256::from(100u64), U256::MAX);
        let domain_sep = domain_separator_for_token(&token, CHAIN_ID);
        let signing_hash = args.signing_hash(domain_sep, U256::ZERO);

        assert_eq!(args.recover_signer(signing_hash).unwrap(), owner);
    }

    #[test]
    fn permit_args_recover_signer_rejects_invalid_v() {
        let owner = owner_address();
        let mut args = sample_permit_args(owner);
        args.v = 26;

        assert_eq!(
            args.recover_signer(B256::ZERO).unwrap_err(),
            BasePrecompileError::revert(IB20::InvalidSigner { signer: Address::ZERO, owner })
        );
    }

    #[test]
    fn permit_args_recover_signer_rejects_invalid_signature() {
        let owner = owner_address();
        let args = sample_permit_args(owner);

        assert!(args.recover_signer(B256::repeat_byte(0x11)).is_err());
    }

    // ---- Permittable ----

    #[test]
    fn domain_typehash_matches_eip712_domain_type() {
        let domain_type =
            b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
        assert_eq!(DOMAIN_TYPEHASH, keccak256(domain_type));
    }

    #[test]
    fn permit_typehash_matches_permit_type_string() {
        let permit_type =
            b"Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)";
        assert_eq!(PermitArgs::TYPEHASH, keccak256(permit_type));
    }

    #[test]
    fn domain_separator_is_deterministic() {
        let token = make_token();
        let sep1 = token.domain_separator(CHAIN_ID).unwrap();
        let sep2 = token.domain_separator(CHAIN_ID).unwrap();
        assert_eq!(sep1, sep2);
    }

    #[test]
    fn domain_separator_differs_by_chain_id() {
        let token = make_token();
        assert_ne!(token.domain_separator(1).unwrap(), token.domain_separator(2).unwrap());
    }

    #[test]
    fn eip712_domain_returns_correct_fields() {
        let token = make_token();
        let (fields, name, version, chain_id, verifying, _salt, extensions) =
            token.eip712_domain(CHAIN_ID).unwrap();

        assert_eq!(fields.as_slice(), &[0x0f]);
        assert_eq!(name, TOKEN_NAME);
        assert_eq!(version, "1");
        assert_eq!(chain_id, U256::from(CHAIN_ID));
        assert_eq!(verifying, TOKEN_ADDR);
        assert!(extensions.is_empty());
    }

    #[test]
    fn domain_separator_differs_by_name() {
        let token = make_token();
        let sep_a = token.domain_separator(CHAIN_ID).unwrap();
        let mut token2 = make_token();
        token2.accounting_mut().set_name("OtherToken".to_string()).unwrap();
        let sep_b = token2.domain_separator(CHAIN_ID).unwrap();
        assert_ne!(sep_a, sep_b);
    }

    #[test]
    fn permit_expired_deadline_reverts() {
        let mut token = make_token();
        let owner = owner_address();
        let deadline = U256::from(999u64);
        let now = U256::from(1000u64);
        let args = signed_permit_args(&token, owner, SPENDER, U256::from(100u64), deadline);

        assert!(token.permit(CHAIN_ID, now, args).is_err());
    }

    #[test]
    fn permit_sets_allowance_and_increments_nonce() {
        let mut token = make_token();
        let owner = owner_address();
        let value = U256::from(500u64);
        let deadline = U256::MAX;
        let now = U256::ZERO;
        let args = signed_permit_args(&token, owner, SPENDER, value, deadline);

        token.permit(CHAIN_ID, now, args).unwrap();

        assert_eq!(token.accounting().allowance(owner, SPENDER).unwrap(), value);
        assert_eq!(token.accounting().nonce(owner).unwrap(), U256::from(1u64));
    }

    #[test]
    fn permit_wrong_signer_reverts() {
        let mut token = make_token();
        let owner = owner_address();
        let wrong_owner = Address::repeat_byte(0xde);
        let value = U256::from(100u64);
        let deadline = U256::MAX;
        let now = U256::ZERO;
        let mut args = signed_permit_args(&token, owner, SPENDER, value, deadline);
        args.owner = wrong_owner;

        assert!(token.permit(CHAIN_ID, now, args).is_err());
    }

    #[test]
    fn permit_zero_owner_returns_invalid_signer_not_invalid_approver() {
        // A unit test for the recovered == Address::ZERO branch is not feasible: alloy's ECDSA
        // recovery returns Err (not zero) for degenerate inputs, and producing a signature that
        // recovers to zero is cryptographically infeasible. This test instead confirms that when
        // owner is Address::ZERO the function returns InvalidSigner (from the signature guard),
        // not InvalidApprover (from approve()) — proving the guard fires before the approve() call.
        let mut token = make_token();
        let real_owner = owner_address();
        let base_args = signed_permit_args(&token, real_owner, SPENDER, U256::ONE, U256::MAX);
        let args = PermitArgs { owner: Address::ZERO, ..base_args };

        // Swapping owner to zero changes the struct hash, so the recovered signer is some
        // arbitrary address. Compute it from the same args permit() would use.
        let domain_sep = token.domain_separator(CHAIN_ID).unwrap();
        let nonce = token.accounting().nonce(Address::ZERO).unwrap();
        let signing_hash = args.signing_hash(domain_sep, nonce);
        let expected_signer = args.recover_signer(signing_hash).unwrap();

        assert_eq!(
            token.permit(CHAIN_ID, U256::ZERO, args).unwrap_err(),
            BasePrecompileError::revert(IB20::InvalidSigner {
                signer: expected_signer,
                owner: Address::ZERO,
            })
        );
    }

    #[test]
    fn permit_nonce_prevents_replay() {
        let mut token = make_token();
        let owner = owner_address();
        let value = U256::from(100u64);
        let deadline = U256::MAX;
        let now = U256::ZERO;
        let args = signed_permit_args(&token, owner, SPENDER, value, deadline);
        token.permit(CHAIN_ID, now, args.clone()).unwrap();

        // Replay the same (v, r, s) — nonce has advanced so the recovered address won't match.
        assert!(token.permit(CHAIN_ID, now, args).is_err());
    }
}
