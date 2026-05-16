use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolInterface, SolValue};
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::token::abi::IDefaultToken;
use crate::token::abi::IDefaultToken::IDefaultTokenCalls as C;
use crate::token::common::ops::{
    Burnable, Mintable, Pausable, Permittable, Redeemable, TokenAdmin, Transferable,
};

use crate::token::common::ITokenCoreAccounting;

use super::DefaultToken;

impl<S: ITokenCoreAccounting> DefaultToken<S> {
    /// ABI-dispatches `calldata` to the appropriate `IDefaultToken` handler.
    pub fn dispatch(&mut self, calldata: &[u8]) -> PrecompileResult {
        let ctx = StorageCtx;
        self.inner(calldata).into_precompile_result(ctx.gas_used(), |b| b)
    }

    fn inner(&mut self, calldata: &[u8]) -> base_precompile_storage::Result<Bytes> {
        if calldata.len() < 4 {
            return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
        }
        let selector: [u8; 4] = calldata[..4].try_into().unwrap();
        let call = IDefaultToken::IDefaultTokenCalls::abi_decode(calldata)
            .map_err(|_| BasePrecompileError::UnknownFunctionSelector(selector))?;

        let encoded: Bytes = match call {
            // --- Pure reads: direct to accounting ---
            C::name(_) => self.accounting.name()?.abi_encode().into(),
            C::symbol(_) => self.accounting.symbol()?.abi_encode().into(),
            C::decimals(_) => U256::from(self.accounting.decimals()?).abi_encode().into(),
            C::totalSupply(_) => self.accounting.total_supply()?.abi_encode().into(),
            C::balanceOf(c) => self.accounting.balance_of(c.account)?.abi_encode().into(),
            C::allowance(c) => {
                self.accounting.allowance(c.owner, c.spender)?.abi_encode().into()
            }
            C::supplyCap(_) => self.accounting.supply_cap()?.abi_encode().into(),
            C::paused(_) => self.accounting.paused()?.abi_encode().into(),
            C::nonces(c) => self.accounting.nonce(c.owner)?.abi_encode().into(),
            C::minimumRedeemable(_) => self.accounting.minimum_redeemable()?.abi_encode().into(),
            C::contractURI(_) => self.accounting.contract_uri()?.abi_encode().into(),
            C::capabilities(_) => self.accounting.capabilities()?.abi_encode().into(),

            // --- Domain reads (light logic) ---
            C::isPaused(c) => self.is_paused(c.vector)?.abi_encode().into(),
            C::isPausable(_) => self.is_pausable()?.abi_encode().into(),
            C::isCapMutable(_) => self.is_cap_mutable()?.abi_encode().into(),
            C::DOMAIN_SEPARATOR(_) => {
                self.domain_separator(StorageCtx.chain_id())?.abi_encode().into()
            }
            C::eip712Domain(_) => {
                self.eip712_domain(StorageCtx.chain_id())?.abi_encode().into()
            }

            // --- ERC-20 mutating ---
            C::transfer(c) => {
                let caller = StorageCtx.caller();
                self.transfer(caller, c.to, c.amount)?;
                true.abi_encode().into()
            }
            C::transferFrom(c) => {
                let caller = StorageCtx.caller();
                self.transfer_from(caller, c.from, c.to, c.amount)?;
                true.abi_encode().into()
            }
            C::approve(c) => {
                let caller = StorageCtx.caller();
                self.approve(caller, c.spender, c.amount)?;
                true.abi_encode().into()
            }
            C::transferWithMemo(c) => {
                let caller = StorageCtx.caller();
                self.transfer_with_memo(caller, c.to, c.amount, c.memo)?;
                true.abi_encode().into()
            }
            C::transferFromWithMemo(c) => {
                let caller = StorageCtx.caller();
                self.transfer_from_with_memo(caller, c.from, c.to, c.amount, c.memo)?;
                true.abi_encode().into()
            }

            // --- Mint ---
            C::mint(c) => {
                self.mint(c.to, c.amount)?;
                Bytes::new()
            }
            C::mintWithMemo(c) => {
                self.mint_with_memo(c.to, c.amount, c.memo)?;
                Bytes::new()
            }

            // --- Burn ---
            C::burn(c) => {
                let caller = StorageCtx.caller();
                self.burn(caller, c.amount)?;
                Bytes::new()
            }
            C::burnWithMemo(c) => {
                let caller = StorageCtx.caller();
                self.burn_with_memo(caller, c.amount, c.memo)?;
                Bytes::new()
            }

            // --- Redeem ---
            C::redeem(c) => {
                let caller = StorageCtx.caller();
                self.redeem(caller, c.amount)?;
                Bytes::new()
            }
            C::redeemWithMemo(c) => {
                let caller = StorageCtx.caller();
                self.redeem_with_memo(caller, c.amount, c.memo)?;
                Bytes::new()
            }
            C::setMinimumRedeemable(c) => {
                let caller = StorageCtx.caller();
                Redeemable::set_minimum_redeemable(self, caller, c.newMinimum)?;
                Bytes::new()
            }

            // --- Pause ---
            C::pause(c) => {
                let caller = StorageCtx.caller();
                self.pause(caller, c.vectors)?;
                Bytes::new()
            }
            C::unpause(_) => {
                let caller = StorageCtx.caller();
                self.unpause(caller)?;
                Bytes::new()
            }

            // --- Admin ---
            C::setSupplyCap(c) => {
                let caller = StorageCtx.caller();
                TokenAdmin::set_supply_cap(self, caller, c.newSupplyCap)?;
                Bytes::new()
            }
            C::setName(c) => {
                let caller = StorageCtx.caller();
                TokenAdmin::set_name(self, caller, c.newName)?;
                Bytes::new()
            }
            C::setSymbol(c) => {
                let caller = StorageCtx.caller();
                TokenAdmin::set_symbol(self, caller, c.newSymbol)?;
                Bytes::new()
            }
            C::setContractURI(c) => {
                let caller = StorageCtx.caller();
                TokenAdmin::set_contract_uri(self, caller, c.newURI)?;
                Bytes::new()
            }

            // --- Permit ---
            C::permit(c) => {
                self.permit(
                    StorageCtx.chain_id(),
                    StorageCtx.timestamp(),
                    c.owner,
                    c.spender,
                    c.value,
                    c.deadline,
                    c.v,
                    c.r,
                    c.s,
                )?;
                Bytes::new()
            }
        };
        Ok(encoded)
    }
}
