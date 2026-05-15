//! `TokenBase<S>` — generic domain layer for all B-20 token variants.
//!
//! Wraps any storage adapter `S: ITokenCoreAccounting` and exposes the
//! full set of composable token operations. Variants use this by wrapping
//! their concrete storage struct: `TokenBase<MyStorage>`.

use alloy_primitives::{Address, B256, FixedBytes, U256, keccak256};
use alloy_sol_types::{SolEvent, SolValue};
use base_precompile_storage::{BasePrecompileError, Result, StorageCtx};

use crate::token::IDefaultToken;

use super::{CAPABILITY_CAP_MUTABLE, CAPABILITY_PAUSABLE, ITokenCoreAccounting};

// keccak256("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)")
const PERMIT_TYPEHASH: B256 =
    alloy_primitives::b256!("6e71edae12b1b97f4d1f60370fef10105fa2faae0126114a169c64845d6126c9");

/// Generic domain layer shared across all B-20 token variants.
///
/// `S` is the storage adapter (the `#[contract]` struct) that implements
/// [`ITokenCoreAccounting`]. All business logic lives here; variants provide
/// only storage and ABI dispatch.
#[derive(Clone)]
pub struct TokenBase<S: ITokenCoreAccounting> {
    /// Direct access to the storage adapter for pure reads in dispatch.
    pub accounting: S,
    address: Address,
}

impl<S: ITokenCoreAccounting> std::fmt::Debug for TokenBase<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenBase").field("address", &self.address).finish_non_exhaustive()
    }
}

impl<S: ITokenCoreAccounting> TokenBase<S> {
    /// Creates a new `TokenBase` wrapping the given storage adapter.
    pub fn new(accounting: S, address: Address) -> Self {
        Self { accounting, address }
    }

    // -------------------------------------------------------------------------
    // View helpers
    // -------------------------------------------------------------------------

    /// Returns whether the given pause `vector` bit is currently set.
    pub fn is_paused(&self, vector: U256) -> Result<bool> {
        Ok((self.accounting.paused()? & vector) != U256::ZERO)
    }

    /// Returns whether the `PAUSABLE` capability bit is set on this token.
    pub fn is_pausable(&self) -> Result<bool> {
        Ok((self.accounting.capabilities()? & CAPABILITY_PAUSABLE) != U256::ZERO)
    }

    /// Returns whether the `CAP_MUTABLE` capability bit is set on this token.
    pub fn is_cap_mutable(&self) -> Result<bool> {
        Ok((self.accounting.capabilities()? & CAPABILITY_CAP_MUTABLE) != U256::ZERO)
    }

    /// Computes the EIP-712 domain separator for this token.
    ///
    /// Domain: `(chainId, verifyingContract)` only — `name` and `version`
    /// are intentionally empty per the `IDefaultToken` spec.
    pub fn domain_separator(&self) -> Result<B256> {
        let domain_type = b"EIP712Domain(uint256 chainId,address verifyingContract)";
        let type_hash: B256 = keccak256(domain_type);
        let chain_id = U256::from(StorageCtx.chain_id());
        let encoded = (type_hash, chain_id, self.address).abi_encode();
        Ok(keccak256(&encoded))
    }

    /// Returns the ERC-5267 `eip712Domain()` tuple for this token.
    pub fn eip712_domain(
        &self,
    ) -> Result<(FixedBytes<1>, String, String, U256, Address, B256, Vec<U256>)> {
        Ok((
            FixedBytes::<1>::from([0x0c]), // bits 2+3: chainId + verifyingContract
            String::new(),
            String::new(),
            U256::from(StorageCtx.chain_id()),
            self.address,
            B256::ZERO,
            vec![],
        ))
    }

    // -------------------------------------------------------------------------
    // ERC-20 primitives
    // -------------------------------------------------------------------------

    /// Moves `amount` tokens from `from` to `to`. Emits `Transfer`.
    pub fn transfer(&mut self, from: Address, to: Address, amount: U256) -> Result<()> {
        if from == Address::ZERO {
            return Err(BasePrecompileError::revert(IDefaultToken::InvalidSender {
                sender: from,
            }));
        }
        if to == Address::ZERO {
            return Err(BasePrecompileError::revert(IDefaultToken::InvalidReceiver {
                receiver: to,
            }));
        }
        let balance = self.accounting.balance_of(from)?;
        if balance < amount {
            return Err(BasePrecompileError::revert(IDefaultToken::InsufficientBalance {
                sender: from,
                balance,
                needed: amount,
            }));
        }
        self.accounting.set_balance(from, balance - amount)?;
        self.accounting.set_balance(to, self.accounting.balance_of(to)? + amount)?;
        self.emit_transfer(from, to, amount)
    }

    /// Moves `amount` tokens from `from` to `to` using `spender`'s allowance.
    /// Emits `Transfer`. Skips allowance decrement when allowance is `U256::MAX`.
    pub fn transfer_from(
        &mut self,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
    ) -> Result<()> {
        if from == Address::ZERO {
            return Err(BasePrecompileError::revert(IDefaultToken::InvalidSender {
                sender: from,
            }));
        }
        let allowance = self.accounting.allowance(from, spender)?;
        if allowance != U256::MAX {
            if allowance < amount {
                return Err(BasePrecompileError::revert(
                    IDefaultToken::InsufficientAllowance { spender, allowance, needed: amount },
                ));
            }
            self.accounting.set_allowance(from, spender, allowance - amount)?;
        }
        self.transfer(from, to, amount)
    }

    /// Sets `spender`'s allowance from `owner` to `amount`. Emits `Approval`.
    pub fn approve(&mut self, owner: Address, spender: Address, amount: U256) -> Result<()> {
        if owner == Address::ZERO {
            return Err(BasePrecompileError::revert(IDefaultToken::InvalidApprover {
                approver: owner,
            }));
        }
        if spender == Address::ZERO {
            return Err(BasePrecompileError::revert(IDefaultToken::InvalidSpender { spender }));
        }
        self.accounting.set_allowance(owner, spender, amount)?;
        let mut ctx = StorageCtx;
        let log = IDefaultToken::Approval { owner, spender, amount }.encode_log_data();
        ctx.emit_event(self.address, log)
    }

    // -------------------------------------------------------------------------
    // Memo variants (compositions: primitive + emit_memo)
    // -------------------------------------------------------------------------

    /// `transfer` + emits `Memo(memo)` immediately after `Transfer`.
    pub fn transfer_with_memo(
        &mut self,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
    ) -> Result<()> {
        self.transfer(from, to, amount)?;
        self.emit_memo(memo)
    }

    /// `transfer_from` + emits `Memo(memo)` immediately after `Transfer`.
    pub fn transfer_from_with_memo(
        &mut self,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
    ) -> Result<()> {
        self.transfer_from(spender, from, to, amount)?;
        self.emit_memo(memo)
    }

    /// `mint` + emits `Memo(memo)` immediately after `Transfer`.
    pub fn mint_with_memo(&mut self, to: Address, amount: U256, memo: B256) -> Result<()> {
        self.mint(to, amount)?;
        self.emit_memo(memo)
    }

    /// `burn` + emits `Memo(memo)` immediately after `Transfer`.
    pub fn burn_with_memo(&mut self, from: Address, amount: U256, memo: B256) -> Result<()> {
        self.burn(from, amount)?;
        self.emit_memo(memo)
    }

    // -------------------------------------------------------------------------
    // Mint / burn
    // -------------------------------------------------------------------------

    /// Creates `amount` tokens at `to`. Enforces supply cap. Emits `Transfer(0x0, to, amount)`.
    pub fn mint(&mut self, to: Address, amount: U256) -> Result<()> {
        if to == Address::ZERO {
            return Err(BasePrecompileError::revert(IDefaultToken::InvalidReceiver {
                receiver: to,
            }));
        }
        let supply = self.accounting.total_supply()?;
        let cap = self.accounting.supply_cap()?;
        let new_supply =
            supply.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        if new_supply > cap {
            return Err(BasePrecompileError::revert(IDefaultToken::SupplyCapExceeded {
                cap,
                attempted: new_supply,
            }));
        }
        self.accounting.set_total_supply(new_supply)?;
        self.accounting.set_balance(to, self.accounting.balance_of(to)? + amount)?;
        self.emit_transfer(Address::ZERO, to, amount)
    }

    /// Destroys `amount` tokens from `from`. Emits `Transfer(from, 0x0, amount)`.
    pub fn burn(&mut self, from: Address, amount: U256) -> Result<()> {
        let balance = self.accounting.balance_of(from)?;
        if balance < amount {
            return Err(BasePrecompileError::revert(IDefaultToken::InsufficientBalance {
                sender: from,
                balance,
                needed: amount,
            }));
        }
        self.accounting.set_balance(from, balance - amount)?;
        self.accounting.set_total_supply(self.accounting.total_supply()? - amount)?;
        self.emit_transfer(from, Address::ZERO, amount)
    }

    // -------------------------------------------------------------------------
    // Redeem
    // -------------------------------------------------------------------------

    /// User-initiated burn with off-chain settlement implication.
    /// Emits `Transfer(caller, 0x0, amount)` then `Redeemed(caller, amount)`.
    pub fn redeem(&mut self, caller: Address, amount: U256) -> Result<()> {
        let minimum = self.accounting.minimum_redeemable()?;
        if amount < minimum {
            return Err(BasePrecompileError::revert(IDefaultToken::MinimumRedeemableNotMet {
                amount,
                minimum,
            }));
        }
        self.burn(caller, amount)?;
        let mut ctx = StorageCtx;
        let log = IDefaultToken::Redeemed { holder: caller, amount }.encode_log_data();
        ctx.emit_event(self.address, log)
    }

    /// `redeem` + emits `Memo(memo)` immediately after `Redeemed`.
    pub fn redeem_with_memo(&mut self, caller: Address, amount: U256, memo: B256) -> Result<()> {
        self.redeem(caller, amount)?;
        self.emit_memo(memo)
    }

    /// Sets the minimum redeemable amount. Emits `MinimumRedeemableUpdated`.
    pub fn set_minimum_redeemable(&mut self, caller: Address, minimum: U256) -> Result<()> {
        let old = self.accounting.minimum_redeemable()?;
        self.accounting.set_minimum_redeemable(minimum)?;
        let mut ctx = StorageCtx;
        let log = IDefaultToken::MinimumRedeemableUpdated {
            updater: caller,
            oldMinimum: old,
            newMinimum: minimum,
        }
        .encode_log_data();
        ctx.emit_event(self.address, log)
    }

    // -------------------------------------------------------------------------
    // Pause
    // -------------------------------------------------------------------------

    /// ORs `vectors` into the current paused bitmask. Requires `PAUSABLE` capability.
    /// Emits `Paused(caller, vectors)`.
    pub fn pause(&mut self, caller: Address, vectors: U256) -> Result<()> {
        if vectors == U256::ZERO {
            return Err(BasePrecompileError::revert(IDefaultToken::InvalidAmount {}));
        }
        if !self.is_pausable()? {
            return Err(BasePrecompileError::revert(IDefaultToken::FeatureDisabled {
                capability: CAPABILITY_PAUSABLE,
            }));
        }
        let current = self.accounting.paused()?;
        self.accounting.set_paused(current | vectors)?;
        let mut ctx = StorageCtx;
        let log = IDefaultToken::Paused { updater: caller, vectors }.encode_log_data();
        ctx.emit_event(self.address, log)
    }

    /// Clears all paused vectors. Requires `PAUSABLE` capability.
    /// Emits `Unpaused(caller)`.
    pub fn unpause(&mut self, caller: Address) -> Result<()> {
        if !self.is_pausable()? {
            return Err(BasePrecompileError::revert(IDefaultToken::FeatureDisabled {
                capability: CAPABILITY_PAUSABLE,
            }));
        }
        self.accounting.set_paused(U256::ZERO)?;
        let mut ctx = StorageCtx;
        let log = IDefaultToken::Unpaused { updater: caller }.encode_log_data();
        ctx.emit_event(self.address, log)
    }

    // -------------------------------------------------------------------------
    // Admin
    // -------------------------------------------------------------------------

    /// Updates the supply cap. Requires `CAP_MUTABLE` capability.
    /// Reverts if new cap < current supply. Emits `SupplyCapUpdated`.
    pub fn set_supply_cap(&mut self, caller: Address, new_cap: U256) -> Result<()> {
        if !self.is_cap_mutable()? {
            return Err(BasePrecompileError::revert(IDefaultToken::FeatureDisabled {
                capability: CAPABILITY_CAP_MUTABLE,
            }));
        }
        let supply = self.accounting.total_supply()?;
        if new_cap < supply {
            return Err(BasePrecompileError::revert(IDefaultToken::InvalidSupplyCap {
                currentSupply: supply,
                proposedCap: new_cap,
            }));
        }
        let old = self.accounting.supply_cap()?;
        self.accounting.set_supply_cap(new_cap)?;
        let mut ctx = StorageCtx;
        let log = IDefaultToken::SupplyCapUpdated {
            updater: caller,
            oldSupplyCap: old,
            newSupplyCap: new_cap,
        }
        .encode_log_data();
        ctx.emit_event(self.address, log)
    }

    /// Updates the token name. Emits `NameUpdated`.
    pub fn set_name(&mut self, caller: Address, name: String) -> Result<()> {
        self.accounting.set_name(name.clone())?;
        let mut ctx = StorageCtx;
        let log = IDefaultToken::NameUpdated { updater: caller, newName: name }.encode_log_data();
        ctx.emit_event(self.address, log)
    }

    /// Updates the token symbol. Emits `SymbolUpdated`.
    pub fn set_symbol(&mut self, caller: Address, symbol: String) -> Result<()> {
        self.accounting.set_symbol(symbol.clone())?;
        let mut ctx = StorageCtx;
        let log =
            IDefaultToken::SymbolUpdated { updater: caller, newSymbol: symbol }.encode_log_data();
        ctx.emit_event(self.address, log)
    }

    /// Updates the contract URI. Emits `ContractURIUpdated`.
    pub fn set_contract_uri(&mut self, _caller: Address, uri: String) -> Result<()> {
        self.accounting.set_contract_uri(uri)?;
        let mut ctx = StorageCtx;
        let log = IDefaultToken::ContractURIUpdated {}.encode_log_data();
        ctx.emit_event(self.address, log)
    }

    // -------------------------------------------------------------------------
    // Permit (EIP-2612)
    // -------------------------------------------------------------------------

    /// EIP-2612 permit. EOA signatures only (no ERC-1271).
    /// Domain: `(chainId, verifyingContract)`; `name` and `version` are empty.
    pub fn permit(
        &mut self,
        owner: Address,
        spender: Address,
        value: U256,
        deadline: U256,
        v: u8,
        r: B256,
        s: B256,
    ) -> Result<()> {
        let now = StorageCtx.timestamp();
        if now > deadline {
            return Err(BasePrecompileError::revert(IDefaultToken::ExpiredSignature { deadline }));
        }

        let domain_sep = self.domain_separator()?;
        let nonce = self.accounting.nonce(owner)?;

        let struct_hash = keccak256(
            (PERMIT_TYPEHASH, owner, spender, value, nonce, deadline).abi_encode(),
        );

        let mut buf = [0u8; 66];
        buf[0] = 0x19;
        buf[1] = 0x01;
        buf[2..34].copy_from_slice(domain_sep.as_slice());
        buf[34..66].copy_from_slice(struct_hash.as_slice());
        let hash = keccak256(&buf);

        let odd_y_parity = v == 28;
        let sig = alloy_primitives::Signature::from_scalars_and_parity(r, s, odd_y_parity);
        let recovered =
            sig.recover_address_from_prehash(&hash).map_err(|_| {
                BasePrecompileError::revert(IDefaultToken::InvalidSigner {
                    signer: Address::ZERO,
                    owner,
                })
            })?;

        if recovered != owner {
            return Err(BasePrecompileError::revert(IDefaultToken::InvalidSigner {
                signer: recovered,
                owner,
            }));
        }

        self.accounting.increment_nonce(owner)?;
        self.approve(owner, spender, value)
    }

    // -------------------------------------------------------------------------
    // Event emission helpers
    // -------------------------------------------------------------------------

    /// Emits a `Transfer(from, to, amount)` event from this token's address.
    pub fn emit_transfer(&mut self, from: Address, to: Address, amount: U256) -> Result<()> {
        let mut ctx = StorageCtx;
        let log = IDefaultToken::Transfer { from, to, amount }.encode_log_data();
        ctx.emit_event(self.address, log)
    }

    /// Emits a `Memo(memo)` event from this token's address.
    pub fn emit_memo(&mut self, memo: B256) -> Result<()> {
        let mut ctx = StorageCtx;
        let log = IDefaultToken::Memo { memo }.encode_log_data();
        ctx.emit_event(self.address, log)
    }
}
