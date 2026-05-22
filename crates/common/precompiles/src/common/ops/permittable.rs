use alloc::{string::String, vec, vec::Vec};

use alloy_primitives::{Address, B256, FixedBytes, U256, keccak256};
use alloy_sol_types::SolValue;
use base_precompile_storage::{BasePrecompileError, Result};

use super::Transferable;
use crate::{IB20, TokenAccounting};

/// ERC-5267 `eip712Domain()` return tuple: (fields, name, version, chainId, verifyingContract, salt, extensions).
pub(super) type Eip712Domain = (FixedBytes<1>, String, String, U256, Address, B256, Vec<U256>);

// keccak256("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)")
const PERMIT_TYPEHASH: B256 =
    alloy_primitives::b256!("6e71edae12b1b97f4d1f60370fef10105fa2faae0126114a169c64845d6126c9");

/// EIP-2612 permit and EIP-712 domain operations.
///
/// Requires [`Transferable`] since `permit` internally calls [`Transferable::approve`].
/// `token_address()` is inherited via `Permittable: Transferable: Token`.
pub trait Permittable: Transferable {
    /// Computes the EIP-712 domain separator for this token.
    ///
    /// Domain: `(chainId, verifyingContract)` only — `name` and `version`
    /// are intentionally empty per the `IB20` spec.
    fn domain_separator(&self, chain_id: u64) -> Result<B256> {
        let domain_type = b"EIP712Domain(uint256 chainId,address verifyingContract)";
        let type_hash: B256 = keccak256(domain_type);
        let encoded = (type_hash, U256::from(chain_id), self.token_address()).abi_encode();
        Ok(keccak256(&encoded))
    }

    /// Returns the ERC-5267 `eip712Domain()` tuple for this token.
    fn eip712_domain(&self, chain_id: u64) -> Result<Eip712Domain> {
        Ok((
            FixedBytes::<1>::from([0x0c]), // bits 2+3: chainId + verifyingContract
            String::new(),
            String::new(),
            U256::from(chain_id),
            self.token_address(),
            B256::ZERO,
            vec![],
        ))
    }

    /// EIP-2612 permit. EOA signatures only (no ERC-1271).
    /// Domain: `(chainId, verifyingContract)`; `name` and `version` are empty.
    #[allow(clippy::too_many_arguments)]
    fn permit(
        &mut self,
        chain_id: u64,
        now: U256,
        owner: Address,
        spender: Address,
        value: U256,
        deadline: U256,
        v: u8,
        r: B256,
        s: B256,
    ) -> Result<()> {
        if now > deadline {
            return Err(BasePrecompileError::revert(IB20::ExpiredSignature { deadline }));
        }

        let domain_sep = self.domain_separator(chain_id)?;
        let nonce = self.accounting().nonce(owner)?;

        let struct_hash =
            keccak256((PERMIT_TYPEHASH, owner, spender, value, nonce, deadline).abi_encode());

        let mut buf = [0u8; 66];
        buf[0] = 0x19;
        buf[1] = 0x01;
        buf[2..34].copy_from_slice(domain_sep.as_slice());
        buf[34..66].copy_from_slice(struct_hash.as_slice());
        let hash = keccak256(buf);

        let odd_y_parity = v == 28;
        let sig = alloy_primitives::Signature::from_scalars_and_parity(r, s, odd_y_parity);
        let recovered = sig.recover_address_from_prehash(&hash).map_err(|_| {
            BasePrecompileError::revert(IB20::InvalidSigner { signer: Address::ZERO, owner })
        })?;

        if recovered != owner {
            return Err(BasePrecompileError::revert(IB20::InvalidSigner {
                signer: recovered,
                owner,
            }));
        }

        self.accounting_mut().increment_nonce(owner)?;
        self.approve(owner, spender, value)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256, keccak256};
    use alloy_sol_types::SolValue;
    use k256::ecdsa::SigningKey;

    use super::{PERMIT_TYPEHASH, Permittable};
    use crate::common::{
        Token, TokenAccounting,
        test_utils::{InMemoryPolicy, InMemoryTokenAccounting, TestToken},
    };

    const CHAIN_ID: u64 = 1;
    const SPENDER: Address = Address::repeat_byte(0xbb);
    const TOKEN_ADDR: Address = Address::repeat_byte(1);

    // Anvil/Hardhat account 0 — well-known test key, never use in production.
    const PRIVATE_KEY: [u8; 32] =
        alloy_primitives::hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

    fn make_token() -> TestToken {
        TestToken::with_storage_and_policy(
            InMemoryTokenAccounting::new(TOKEN_ADDR),
            InMemoryPolicy::new(),
        )
    }

    fn owner_address() -> Address {
        let key = SigningKey::from_slice(&PRIVATE_KEY).unwrap();
        let point = key.verifying_key().to_encoded_point(false);
        let hash = keccak256(&point.as_bytes()[1..]);
        Address::from_slice(&hash[12..])
    }

    fn sign_permit(
        token: &TestToken,
        owner: Address,
        spender: Address,
        value: U256,
        deadline: U256,
    ) -> (u8, B256, B256) {
        let domain_sep = token.domain_separator(CHAIN_ID).unwrap();
        let nonce = token.accounting().nonce(owner).unwrap();
        let struct_hash =
            keccak256((PERMIT_TYPEHASH, owner, spender, value, nonce, deadline).abi_encode());
        let mut buf = [0u8; 66];
        buf[0] = 0x19;
        buf[1] = 0x01;
        buf[2..34].copy_from_slice(domain_sep.as_slice());
        buf[34..66].copy_from_slice(struct_hash.as_slice());
        let hash = keccak256(buf);

        let signing_key = SigningKey::from_slice(&PRIVATE_KEY).unwrap();
        let (sig, recid) = signing_key.sign_prehash_recoverable(hash.as_slice()).unwrap();
        let sig_bytes = sig.to_bytes();
        let r = B256::from_slice(&sig_bytes[..32]);
        let s = B256::from_slice(&sig_bytes[32..]);
        let v = if recid.is_y_odd() { 28u8 } else { 27u8 };
        (v, r, s)
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

        assert_eq!(fields.as_slice(), &[0x0c]);
        assert!(name.is_empty());
        assert!(version.is_empty());
        assert_eq!(chain_id, U256::from(CHAIN_ID));
        assert_eq!(verifying, TOKEN_ADDR);
        assert!(extensions.is_empty());
    }

    #[test]
    fn permit_expired_deadline_reverts() {
        let mut token = make_token();
        let owner = owner_address();
        let deadline = U256::from(999u64);
        let now = U256::from(1000u64);
        let (v, r, s) = sign_permit(&token, owner, SPENDER, U256::from(100u64), deadline);

        assert!(
            token
                .permit(CHAIN_ID, now, owner, SPENDER, U256::from(100u64), deadline, v, r, s)
                .is_err()
        );
    }

    #[test]
    fn permit_sets_allowance_and_increments_nonce() {
        let mut token = make_token();
        let owner = owner_address();
        let value = U256::from(500u64);
        let deadline = U256::MAX;
        let now = U256::ZERO;
        let (v, r, s) = sign_permit(&token, owner, SPENDER, value, deadline);

        token.permit(CHAIN_ID, now, owner, SPENDER, value, deadline, v, r, s).unwrap();

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
        // Sign as `owner` but claim `wrong_owner` — recovered address won't match.
        let (v, r, s) = sign_permit(&token, owner, SPENDER, value, deadline);

        assert!(
            token.permit(CHAIN_ID, now, wrong_owner, SPENDER, value, deadline, v, r, s).is_err()
        );
    }

    #[test]
    fn permit_nonce_prevents_replay() {
        let mut token = make_token();
        let owner = owner_address();
        let value = U256::from(100u64);
        let deadline = U256::MAX;
        let now = U256::ZERO;

        let (v, r, s) = sign_permit(&token, owner, SPENDER, value, deadline);
        token.permit(CHAIN_ID, now, owner, SPENDER, value, deadline, v, r, s).unwrap();

        // Replay the same (v, r, s) — nonce has advanced so the recovered address won't match.
        assert!(token.permit(CHAIN_ID, now, owner, SPENDER, value, deadline, v, r, s).is_err());
    }
}
