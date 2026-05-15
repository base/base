use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolInterface, SolValue};
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::token::abi::IDefaultToken;
use crate::token::abi::IDefaultToken::IDefaultTokenCalls as C;
use crate::token::common::ITokenCoreAccounting;

use super::DefaultToken;

impl DefaultToken {
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
            C::name(_) => self.base.accounting.name()?.abi_encode().into(),
            C::symbol(_) => self.base.accounting.symbol()?.abi_encode().into(),
            C::decimals(_) => U256::from(self.base.accounting.decimals()?).abi_encode().into(),
            C::totalSupply(_) => self.base.accounting.total_supply()?.abi_encode().into(),
            C::balanceOf(c) => self.base.accounting.balance_of(c.account)?.abi_encode().into(),
            C::allowance(c) => {
                self.base.accounting.allowance(c.owner, c.spender)?.abi_encode().into()
            }
            C::supplyCap(_) => self.base.accounting.supply_cap()?.abi_encode().into(),
            C::paused(_) => self.base.accounting.paused()?.abi_encode().into(),
            C::nonces(c) => self.base.accounting.nonce(c.owner)?.abi_encode().into(),
            C::minimumRedeemable(_) => {
                self.base.accounting.minimum_redeemable()?.abi_encode().into()
            }
            C::contractURI(_) => self.base.accounting.contract_uri()?.abi_encode().into(),
            C::capabilities(_) => self.base.accounting.capabilities()?.abi_encode().into(),

            // --- Domain reads (light logic) ---
            C::isPaused(c) => self.base.is_paused(c.vector)?.abi_encode().into(),
            C::isPausable(_) => self.base.is_pausable()?.abi_encode().into(),
            C::isCapMutable(_) => self.base.is_cap_mutable()?.abi_encode().into(),
            C::DOMAIN_SEPARATOR(_) => {
                self.base.domain_separator(StorageCtx.chain_id())?.abi_encode().into()
            }
            C::eip712Domain(_) => {
                self.base.eip712_domain(StorageCtx.chain_id())?.abi_encode().into()
            }

            // --- ERC-20 mutating ---
            C::transfer(c) => {
                let caller = StorageCtx.caller();
                self.base.transfer(caller, c.to, c.amount)?;
                true.abi_encode().into()
            }
            C::transferFrom(c) => {
                let caller = StorageCtx.caller();
                self.base.transfer_from(caller, c.from, c.to, c.amount)?;
                true.abi_encode().into()
            }
            C::approve(c) => {
                let caller = StorageCtx.caller();
                self.base.approve(caller, c.spender, c.amount)?;
                true.abi_encode().into()
            }
            C::transferWithMemo(c) => {
                let caller = StorageCtx.caller();
                self.base.transfer_with_memo(caller, c.to, c.amount, c.memo)?;
                true.abi_encode().into()
            }
            C::transferFromWithMemo(c) => {
                let caller = StorageCtx.caller();
                self.base.transfer_from_with_memo(caller, c.from, c.to, c.amount, c.memo)?;
                true.abi_encode().into()
            }

            // --- Mint ---
            C::mint(c) => {
                self.base.mint(c.to, c.amount)?;
                Bytes::new()
            }
            C::mintWithMemo(c) => {
                self.base.mint_with_memo(c.to, c.amount, c.memo)?;
                Bytes::new()
            }

            // --- Burn ---
            C::burn(c) => {
                let caller = StorageCtx.caller();
                self.base.burn(caller, c.amount)?;
                Bytes::new()
            }
            C::burnWithMemo(c) => {
                let caller = StorageCtx.caller();
                self.base.burn_with_memo(caller, c.amount, c.memo)?;
                Bytes::new()
            }

            // --- Redeem ---
            C::redeem(c) => {
                let caller = StorageCtx.caller();
                self.base.redeem(caller, c.amount)?;
                Bytes::new()
            }
            C::redeemWithMemo(c) => {
                let caller = StorageCtx.caller();
                self.base.redeem_with_memo(caller, c.amount, c.memo)?;
                Bytes::new()
            }
            C::setMinimumRedeemable(c) => {
                let caller = StorageCtx.caller();
                self.base.set_minimum_redeemable(caller, c.newMinimum)?;
                Bytes::new()
            }

            // --- Pause ---
            C::pause(c) => {
                let caller = StorageCtx.caller();
                self.base.pause(caller, c.vectors)?;
                Bytes::new()
            }
            C::unpause(_) => {
                let caller = StorageCtx.caller();
                self.base.unpause(caller)?;
                Bytes::new()
            }

            // --- Admin ---
            C::setSupplyCap(c) => {
                let caller = StorageCtx.caller();
                self.base.set_supply_cap(caller, c.newSupplyCap)?;
                Bytes::new()
            }
            C::setName(c) => {
                let caller = StorageCtx.caller();
                self.base.set_name(caller, c.newName)?;
                Bytes::new()
            }
            C::setSymbol(c) => {
                let caller = StorageCtx.caller();
                self.base.set_symbol(caller, c.newSymbol)?;
                Bytes::new()
            }
            C::setContractURI(c) => {
                let caller = StorageCtx.caller();
                self.base.set_contract_uri(caller, c.newURI)?;
                Bytes::new()
            }

            // --- Permit ---
            C::permit(c) => {
                self.base.permit(
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
