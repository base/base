use alloc::{string::String, vec, vec::Vec};

use alloy_primitives::{Address, B256, FixedBytes, U256, keccak256};
use alloy_sol_types::SolValue;
use base_precompile_storage::{BasePrecompileError, Result};

use super::Transferable;
use crate::token::{IB20, common::TokenAccounting};

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
