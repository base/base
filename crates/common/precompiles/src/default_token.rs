use alloc::{string::String, vec::Vec};
use core::result;

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::{Address, B256, Bytes, U256, address};
use alloy_sol_types::{SolCall, SolError, sol};
use base_precompile_macros::contract;
use base_precompile_storage::{EvmPrecompileStorageProvider, Handler, Mapping, Result, StorageCtx};
use revm::precompile::{PrecompileId, PrecompileOutput, PrecompileResult};

/// Address of the Base default ERC-20 token precompile.
pub const DEFAULT_TOKEN_ADDRESS: Address = address!("0x8453000000000000000000000000000000000000");

/// Default admin role used by the Base default ERC-20 token precompile.
pub const DEFAULT_ADMIN_ROLE: B256 = B256::ZERO;

/// Issuer role used by the Base default ERC-20 token precompile.
pub const ISSUER_ROLE: B256 = B256::new([
    0x11, 0x4e, 0x74, 0xf6, 0xea, 0x3b, 0xd8, 0x19, 0x99, 0x8f, 0x78, 0x68, 0x7b, 0xfc, 0xb1, 0x1b,
    0x14, 0x0d, 0xa0, 0x8e, 0x9b, 0x7d, 0x22, 0x2f, 0xa9, 0xc1, 0xf1, 0xba, 0x1f, 0x2a, 0xa1, 0x22,
]);

sol! {
    error Unauthorized(address account, bytes32 role);
    error InvalidRole(bytes32 role);
    error InvalidRecipient(address recipient);
    error InsufficientBalance(uint256 available, uint256 required, address account);
    error InsufficientAllowance(uint256 available, uint256 required, address owner, address spender);
    error StaticCallNotAllowed();
    error DelegateCallNotAllowed();
    error NonPayable();

    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);
    event RoleMembershipUpdated(bytes32 indexed role, address indexed account, address indexed sender, bool hasRole);
    event Mint(address indexed to, uint256 amount);

    interface IBaseDefaultToken {
        function name() external view returns (string);
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
        function totalSupply() external view returns (uint256);
        function balanceOf(address account) external view returns (uint256);
        function transfer(address to, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
        function transferFrom(address from, address to, uint256 amount) external returns (bool);

        function ISSUER_ROLE() external view returns (bytes32);
        function hasRole(address account, bytes32 role) external view returns (bool);
        function getRoleAdmin(bytes32 role) external view returns (bytes32);
        function grantRole(bytes32 role, address account) external;
        function revokeRole(bytes32 role, address account) external;
        function renounceRole(bytes32 role) external;

        function mint(address to, uint256 amount) external;
    }
}

/// Storage and dispatch implementation for the Base default ERC-20 token precompile.
#[contract(addr = DEFAULT_TOKEN_ADDRESS)]
pub struct DefaultToken {
    pub total_supply: U256,
    pub balances: Mapping<Address, U256>,
    pub allowances: Mapping<Address, Mapping<Address, U256>>,
    pub roles: Mapping<B256, Mapping<Address, bool>>,
}

impl DefaultToken {
    /// Canonical precompile address.
    pub const ADDRESS: Address = DEFAULT_TOKEN_ADDRESS;
    /// Token display name.
    pub const NAME: &'static str = "Base Default Token";
    /// Token display symbol.
    pub const SYMBOL: &'static str = "BASE";
    /// Token decimals.
    pub const DECIMALS: u8 = 18;
    /// Flat gas charged for read-only token calls.
    pub const VIEW_GAS: u64 = 2_100;
    /// Flat gas charged for state-mutating token calls.
    pub const WRITE_GAS: u64 = 50_000;

    /// Returns this precompile's dynamic registration entry.
    pub fn registration() -> (Address, DynPrecompile) {
        (
            DEFAULT_TOKEN_ADDRESS,
            DynPrecompile::new_stateful(PrecompileId::custom("BASE_DEFAULT_TOKEN"), Self::run),
        )
    }

    /// Executes the precompile against live EVM state.
    pub fn run(input: PrecompileInput<'_>) -> PrecompileResult {
        let data = input.data;
        let direct_call = input.is_direct_call();
        let value = input.value;

        if !direct_call {
            return Self::revert_without_context(DelegateCallNotAllowed {}.abi_encode());
        }
        if value != U256::ZERO {
            return Self::revert_without_context(NonPayable {}.abi_encode());
        }

        let mut storage = EvmPrecompileStorageProvider::new(input);
        StorageCtx::enter(&mut storage, || Self::dispatch(data))
    }

    /// Dispatches ABI-encoded calldata inside an active [`StorageCtx`].
    pub fn dispatch(data: &[u8]) -> PrecompileResult {
        let mut ctx = StorageCtx;
        match Self::dispatch_inner(&mut ctx, data) {
            Ok(output) => Ok(ctx.success_output(output)),
            Err(result) => result,
        }
    }

    /// Returns whether `role` is a role recognized by this precompile.
    pub const fn is_valid_role(role: B256) -> bool {
        role.const_eq(&DEFAULT_ADMIN_ROLE) || role.const_eq(&ISSUER_ROLE)
    }

    /// Returns the admin role for `role`.
    pub const fn role_admin(role: B256) -> Option<B256> {
        if Self::is_valid_role(role) { Some(DEFAULT_ADMIN_ROLE) } else { None }
    }

    /// Decodes the selector and executes the matching token call.
    pub fn dispatch_inner(
        ctx: &mut StorageCtx,
        data: &[u8],
    ) -> result::Result<Bytes, PrecompileResult> {
        let selector = Self::selector(data)?;
        match selector {
            IBaseDefaultToken::nameCall::SELECTOR => {
                Self::decode::<IBaseDefaultToken::nameCall>(ctx, data)?;
                ctx.deduct_gas(Self::VIEW_GAS).map_err(|e| ctx.error_result(e))?;
                let value = String::from(Self::NAME);
                Ok(Self::encode_returns::<IBaseDefaultToken::nameCall>(&value))
            }
            IBaseDefaultToken::symbolCall::SELECTOR => {
                Self::decode::<IBaseDefaultToken::symbolCall>(ctx, data)?;
                ctx.deduct_gas(Self::VIEW_GAS).map_err(|e| ctx.error_result(e))?;
                let value = String::from(Self::SYMBOL);
                Ok(Self::encode_returns::<IBaseDefaultToken::symbolCall>(&value))
            }
            IBaseDefaultToken::decimalsCall::SELECTOR => {
                Self::decode::<IBaseDefaultToken::decimalsCall>(ctx, data)?;
                ctx.deduct_gas(Self::VIEW_GAS).map_err(|e| ctx.error_result(e))?;
                Ok(Self::encode_returns::<IBaseDefaultToken::decimalsCall>(&Self::DECIMALS))
            }
            IBaseDefaultToken::totalSupplyCall::SELECTOR => {
                Self::decode::<IBaseDefaultToken::totalSupplyCall>(ctx, data)?;
                ctx.deduct_gas(Self::VIEW_GAS).map_err(|e| ctx.error_result(e))?;
                let token = Self::new();
                let supply = Self::storage_result(ctx, token.total_supply.read())?;
                Ok(Self::encode_returns::<IBaseDefaultToken::totalSupplyCall>(&supply))
            }
            IBaseDefaultToken::balanceOfCall::SELECTOR => {
                let call = Self::decode::<IBaseDefaultToken::balanceOfCall>(ctx, data)?;
                ctx.deduct_gas(Self::VIEW_GAS).map_err(|e| ctx.error_result(e))?;
                let token = Self::new();
                let balance = Self::storage_result(ctx, token.balances.at(&call.account).read())?;
                Ok(Self::encode_returns::<IBaseDefaultToken::balanceOfCall>(&balance))
            }
            IBaseDefaultToken::allowanceCall::SELECTOR => {
                let call = Self::decode::<IBaseDefaultToken::allowanceCall>(ctx, data)?;
                ctx.deduct_gas(Self::VIEW_GAS).map_err(|e| ctx.error_result(e))?;
                let token = Self::new();
                let allowance = Self::storage_result(
                    ctx,
                    token.allowances.at(&call.owner).at(&call.spender).read(),
                )?;
                Ok(Self::encode_returns::<IBaseDefaultToken::allowanceCall>(&allowance))
            }
            IBaseDefaultToken::ISSUER_ROLECall::SELECTOR => {
                Self::decode::<IBaseDefaultToken::ISSUER_ROLECall>(ctx, data)?;
                ctx.deduct_gas(Self::VIEW_GAS).map_err(|e| ctx.error_result(e))?;
                Ok(Self::encode_returns::<IBaseDefaultToken::ISSUER_ROLECall>(&ISSUER_ROLE))
            }
            IBaseDefaultToken::hasRoleCall::SELECTOR => {
                let call = Self::decode::<IBaseDefaultToken::hasRoleCall>(ctx, data)?;
                ctx.deduct_gas(Self::VIEW_GAS).map_err(|e| ctx.error_result(e))?;
                let token = Self::new();
                let has_role =
                    Self::storage_result(ctx, token.roles.at(&call.role).at(&call.account).read())?;
                Ok(Self::encode_returns::<IBaseDefaultToken::hasRoleCall>(&has_role))
            }
            IBaseDefaultToken::getRoleAdminCall::SELECTOR => {
                let call = Self::decode::<IBaseDefaultToken::getRoleAdminCall>(ctx, data)?;
                ctx.deduct_gas(Self::VIEW_GAS).map_err(|e| ctx.error_result(e))?;
                let Some(admin_role) = Self::role_admin(call.role) else {
                    return Err(Self::revert(ctx, InvalidRole { role: call.role }.abi_encode()));
                };
                Ok(Self::encode_returns::<IBaseDefaultToken::getRoleAdminCall>(&admin_role))
            }
            IBaseDefaultToken::transferCall::SELECTOR => {
                let call = Self::decode::<IBaseDefaultToken::transferCall>(ctx, data)?;
                Self::reject_static(ctx)?;
                ctx.deduct_gas(Self::WRITE_GAS).map_err(|e| ctx.error_result(e))?;
                let from = ctx.caller();
                let mut token = Self::new();
                Self::transfer_tokens(ctx, &mut token, from, call.to, call.amount)?;
                Ok(Self::encode_returns::<IBaseDefaultToken::transferCall>(&true))
            }
            IBaseDefaultToken::approveCall::SELECTOR => {
                let call = Self::decode::<IBaseDefaultToken::approveCall>(ctx, data)?;
                Self::reject_static(ctx)?;
                ctx.deduct_gas(Self::WRITE_GAS).map_err(|e| ctx.error_result(e))?;
                let owner = ctx.caller();
                let mut token = Self::new();
                Self::storage_result(
                    ctx,
                    token.allowances.at_mut(&owner).at_mut(&call.spender).write(call.amount),
                )?;
                Self::storage_result(
                    ctx,
                    token.emit_event(Approval { owner, spender: call.spender, value: call.amount }),
                )?;
                Ok(Self::encode_returns::<IBaseDefaultToken::approveCall>(&true))
            }
            IBaseDefaultToken::transferFromCall::SELECTOR => {
                let call = Self::decode::<IBaseDefaultToken::transferFromCall>(ctx, data)?;
                Self::reject_static(ctx)?;
                ctx.deduct_gas(Self::WRITE_GAS).map_err(|e| ctx.error_result(e))?;
                let spender = ctx.caller();
                let mut token = Self::new();
                let guard = ctx.checkpoint();
                if spender != call.from {
                    Self::spend_allowance(ctx, &mut token, call.from, spender, call.amount)?;
                }
                Self::transfer_tokens(ctx, &mut token, call.from, call.to, call.amount)?;
                guard.commit();
                Ok(Self::encode_returns::<IBaseDefaultToken::transferFromCall>(&true))
            }
            IBaseDefaultToken::grantRoleCall::SELECTOR => {
                let call = Self::decode::<IBaseDefaultToken::grantRoleCall>(ctx, data)?;
                Self::reject_static(ctx)?;
                ctx.deduct_gas(Self::WRITE_GAS).map_err(|e| ctx.error_result(e))?;
                let mut token = Self::new();
                Self::ensure_role_admin(ctx, &token, call.role)?;
                Self::set_role(ctx, &mut token, call.role, call.account, true)?;
                Ok(Bytes::new())
            }
            IBaseDefaultToken::revokeRoleCall::SELECTOR => {
                let call = Self::decode::<IBaseDefaultToken::revokeRoleCall>(ctx, data)?;
                Self::reject_static(ctx)?;
                ctx.deduct_gas(Self::WRITE_GAS).map_err(|e| ctx.error_result(e))?;
                let mut token = Self::new();
                Self::ensure_role_admin(ctx, &token, call.role)?;
                Self::set_role(ctx, &mut token, call.role, call.account, false)?;
                Ok(Bytes::new())
            }
            IBaseDefaultToken::renounceRoleCall::SELECTOR => {
                let call = Self::decode::<IBaseDefaultToken::renounceRoleCall>(ctx, data)?;
                Self::reject_static(ctx)?;
                ctx.deduct_gas(Self::WRITE_GAS).map_err(|e| ctx.error_result(e))?;
                if !Self::is_valid_role(call.role) {
                    return Err(Self::revert(ctx, InvalidRole { role: call.role }.abi_encode()));
                }
                let mut token = Self::new();
                Self::set_role(ctx, &mut token, call.role, ctx.caller(), false)?;
                Ok(Bytes::new())
            }
            IBaseDefaultToken::mintCall::SELECTOR => {
                let call = Self::decode::<IBaseDefaultToken::mintCall>(ctx, data)?;
                Self::reject_static(ctx)?;
                ctx.deduct_gas(Self::WRITE_GAS).map_err(|e| ctx.error_result(e))?;
                let mut token = Self::new();
                Self::ensure_role(ctx, &token, ISSUER_ROLE)?;
                Self::mint_tokens(ctx, &mut token, call.to, call.amount)?;
                Ok(Bytes::new())
            }
            _ => Err(ctx.error_result(
                base_precompile_storage::BasePrecompileError::UnknownFunctionSelector(selector),
            )),
        }
    }

    /// Moves token balance between two accounts and emits `Transfer`.
    pub fn transfer_tokens(
        ctx: &mut StorageCtx,
        token: &mut Self,
        from: Address,
        to: Address,
        amount: U256,
    ) -> result::Result<(), PrecompileResult> {
        if to == Address::ZERO {
            return Err(Self::revert(ctx, InvalidRecipient { recipient: to }.abi_encode()));
        }

        let from_balance = Self::storage_result(ctx, token.balances.at(&from).read())?;
        let Some(new_from_balance) = from_balance.checked_sub(amount) else {
            return Err(Self::revert(
                ctx,
                InsufficientBalance { available: from_balance, required: amount, account: from }
                    .abi_encode(),
            ));
        };

        let to_balance = Self::storage_result(ctx, token.balances.at(&to).read())?;
        let new_to_balance = Self::checked_add(ctx, to_balance, amount)?;

        let guard = ctx.checkpoint();
        Self::storage_result(ctx, token.balances.at_mut(&from).write(new_from_balance))?;
        Self::storage_result(ctx, token.balances.at_mut(&to).write(new_to_balance))?;
        Self::storage_result(ctx, token.emit_event(Transfer { from, to, value: amount }))?;
        guard.commit();
        Ok(())
    }

    /// Mints new token balance to an account and emits mint/transfer events.
    pub fn mint_tokens(
        ctx: &mut StorageCtx,
        token: &mut Self,
        to: Address,
        amount: U256,
    ) -> result::Result<(), PrecompileResult> {
        if to == Address::ZERO {
            return Err(Self::revert(ctx, InvalidRecipient { recipient: to }.abi_encode()));
        }

        let total_supply = Self::storage_result(ctx, token.total_supply.read())?;
        let new_total_supply = Self::checked_add(ctx, total_supply, amount)?;
        let balance = Self::storage_result(ctx, token.balances.at(&to).read())?;
        let new_balance = Self::checked_add(ctx, balance, amount)?;
        let guard = ctx.checkpoint();
        Self::storage_result(ctx, token.total_supply.write(new_total_supply))?;
        Self::storage_result(ctx, token.balances.at_mut(&to).write(new_balance))?;
        Self::storage_result(
            ctx,
            token.emit_event(Transfer { from: Address::ZERO, to, value: amount }),
        )?;
        Self::storage_result(ctx, token.emit_event(Mint { to, amount }))?;
        guard.commit();
        Ok(())
    }

    /// Decreases a spender allowance and emits `Approval`.
    pub fn spend_allowance(
        ctx: &StorageCtx,
        token: &mut Self,
        owner: Address,
        spender: Address,
        amount: U256,
    ) -> result::Result<(), PrecompileResult> {
        let allowance = Self::storage_result(ctx, token.allowances.at(&owner).at(&spender).read())?;
        let Some(new_allowance) = allowance.checked_sub(amount) else {
            return Err(Self::revert(
                ctx,
                InsufficientAllowance { available: allowance, required: amount, owner, spender }
                    .abi_encode(),
            ));
        };

        Self::storage_result(
            ctx,
            token.allowances.at_mut(&owner).at_mut(&spender).write(new_allowance),
        )?;
        Self::storage_result(
            ctx,
            token.emit_event(Approval { owner, spender, value: new_allowance }),
        )?;
        Ok(())
    }

    /// Returns success when the caller has `role`.
    pub fn ensure_role(
        ctx: &StorageCtx,
        token: &Self,
        role: B256,
    ) -> result::Result<(), PrecompileResult> {
        if !Self::is_valid_role(role) {
            return Err(Self::revert(ctx, InvalidRole { role }.abi_encode()));
        }
        let account = ctx.caller();
        let has_role = Self::storage_result(ctx, token.roles.at(&role).at(&account).read())?;
        if !has_role {
            return Err(Self::revert(ctx, Unauthorized { account, role }.abi_encode()));
        }
        Ok(())
    }

    /// Returns success when the caller has the admin role for `role`.
    pub fn ensure_role_admin(
        ctx: &StorageCtx,
        token: &Self,
        role: B256,
    ) -> result::Result<(), PrecompileResult> {
        let Some(admin_role) = Self::role_admin(role) else {
            return Err(Self::revert(ctx, InvalidRole { role }.abi_encode()));
        };
        Self::ensure_role(ctx, token, admin_role)
    }

    /// Sets a role membership flag and emits `RoleMembershipUpdated`.
    pub fn set_role(
        ctx: &StorageCtx,
        token: &mut Self,
        role: B256,
        account: Address,
        enabled: bool,
    ) -> result::Result<(), PrecompileResult> {
        if !Self::is_valid_role(role) {
            return Err(Self::revert(ctx, InvalidRole { role }.abi_encode()));
        }
        Self::storage_result(ctx, token.roles.at_mut(&role).at_mut(&account).write(enabled))?;
        Self::storage_result(
            ctx,
            token.emit_event(RoleMembershipUpdated {
                role,
                account,
                sender: ctx.caller(),
                hasRole: enabled,
            }),
        )?;
        Ok(())
    }

    /// Rejects a mutating operation from a static call context.
    pub fn reject_static(ctx: &StorageCtx) -> result::Result<(), PrecompileResult> {
        if ctx.is_static() {
            return Err(Self::revert(ctx, StaticCallNotAllowed {}.abi_encode()));
        }
        Ok(())
    }

    /// Adds two U256 values or returns a Solidity panic-style precompile result.
    pub fn checked_add(
        ctx: &StorageCtx,
        lhs: U256,
        rhs: U256,
    ) -> result::Result<U256, PrecompileResult> {
        lhs.checked_add(rhs).ok_or_else(|| {
            ctx.error_result(base_precompile_storage::BasePrecompileError::under_overflow())
        })
    }

    /// Converts a storage-layer result into this precompile's result shape.
    pub fn storage_result<T>(
        ctx: &StorageCtx,
        result: Result<T>,
    ) -> result::Result<T, PrecompileResult> {
        result.map_err(|e| ctx.error_result(e))
    }

    /// Decodes calldata for one generated Solidity call type.
    pub fn decode<C: SolCall>(
        ctx: &StorageCtx,
        data: &[u8],
    ) -> result::Result<C, PrecompileResult> {
        C::abi_decode(data).map_err(|_| {
            let selector = Self::selector(data).unwrap_or([0u8; 4]);
            ctx.error_result(base_precompile_storage::BasePrecompileError::UnknownFunctionSelector(
                selector,
            ))
        })
    }

    /// ABI-encodes return data for one generated Solidity call type.
    pub fn encode_returns<C: SolCall>(returns: &C::Return) -> Bytes {
        Bytes::from(C::abi_encode_returns(returns))
    }

    /// Extracts the 4-byte function selector from calldata.
    pub fn selector(data: &[u8]) -> result::Result<[u8; 4], PrecompileResult> {
        data.get(..4).and_then(|selector| selector.try_into().ok()).ok_or_else(|| {
            base_precompile_storage::BasePrecompileError::UnknownFunctionSelector([0u8; 4])
                .into_precompile_result(0)
        })
    }

    /// Builds a reverted precompile output with the current gas used.
    pub fn revert(ctx: &StorageCtx, encoded_error: Vec<u8>) -> PrecompileResult {
        Ok(ctx.revert_output(Bytes::from(encoded_error)))
    }

    /// Builds a reverted precompile output before a storage context exists.
    pub fn revert_without_context(encoded_error: Vec<u8>) -> PrecompileResult {
        Ok(PrecompileOutput::new_reverted(0, Bytes::from(encoded_error)))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::IntoLogData;
    use alloy_sol_types::SolCall;
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use super::*;

    const ADMIN: Address = address!("0x1111111111111111111111111111111111111111");
    const ALICE: Address = address!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    const BOB: Address = address!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

    fn run_as<T>(caller: Address, f: impl FnOnce() -> T) -> T {
        run_with_context(caller, false, f)
    }

    fn run_with_context<T>(caller: Address, is_static: bool, f: impl FnOnce() -> T) -> T {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_caller(caller);
        storage.set_static(is_static);
        StorageCtx::enter(&mut storage, f)
    }

    fn enter<T>(storage: &mut HashMapStorageProvider, f: impl FnOnce() -> T) -> T {
        StorageCtx::enter(storage, f)
    }

    fn seed_role(token: &mut DefaultToken, role: B256, account: Address) {
        token.roles.at_mut(&role).at_mut(&account).write(true).unwrap();
    }

    #[test]
    fn metadata_and_role_constants_are_exposed() {
        run_as(ADMIN, || {
            let output = DefaultToken::dispatch(&IBaseDefaultToken::nameCall {}.abi_encode())
                .expect("name should execute");
            assert!(!output.reverted);
            assert_eq!(
                IBaseDefaultToken::nameCall::abi_decode_returns(&output.bytes).unwrap(),
                String::from(DefaultToken::NAME)
            );

            let output =
                DefaultToken::dispatch(&IBaseDefaultToken::ISSUER_ROLECall {}.abi_encode())
                    .expect("ISSUER_ROLE should execute");
            assert!(!output.reverted);
            assert_eq!(
                IBaseDefaultToken::ISSUER_ROLECall::abi_decode_returns(&output.bytes).unwrap(),
                ISSUER_ROLE
            );
        });
    }

    #[test]
    fn issuer_can_mint_and_holder_can_transfer() {
        run_as(ADMIN, || {
            let mut token = DefaultToken::new();
            seed_role(&mut token, ISSUER_ROLE, ADMIN);

            let mint = IBaseDefaultToken::mintCall { to: ALICE, amount: U256::from(100u64) };
            let output = DefaultToken::dispatch(&mint.abi_encode()).expect("mint should execute");
            assert!(!output.reverted);

            assert_eq!(token.total_supply.read().unwrap(), U256::from(100u64));
            assert_eq!(token.balances.at(&ALICE).read().unwrap(), U256::from(100u64));
        });

        run_as(ALICE, || {
            let mut token = DefaultToken::new();
            token.balances.at_mut(&ALICE).write(U256::from(100u64)).unwrap();

            let transfer = IBaseDefaultToken::transferCall { to: BOB, amount: U256::from(40u64) };
            let output =
                DefaultToken::dispatch(&transfer.abi_encode()).expect("transfer should execute");
            assert!(!output.reverted);
            assert!(IBaseDefaultToken::transferCall::abi_decode_returns(&output.bytes).unwrap());

            assert_eq!(token.balances.at(&ALICE).read().unwrap(), U256::from(60u64));
            assert_eq!(token.balances.at(&BOB).read().unwrap(), U256::from(40u64));
        });
    }

    #[test]
    fn mint_requires_issuer_role() {
        run_as(ADMIN, || {
            let mint = IBaseDefaultToken::mintCall { to: ALICE, amount: U256::from(100u64) };
            let output = DefaultToken::dispatch(&mint.abi_encode()).expect("mint should revert");
            assert!(output.reverted);
            assert_eq!(output.bytes[..4], Unauthorized::SELECTOR);
        });
    }

    #[test]
    fn admin_can_grant_and_revoke_issuer_role() {
        run_as(ADMIN, || {
            let mut token = DefaultToken::new();
            seed_role(&mut token, DEFAULT_ADMIN_ROLE, ADMIN);

            let grant = IBaseDefaultToken::grantRoleCall { role: ISSUER_ROLE, account: ALICE };
            let output = DefaultToken::dispatch(&grant.abi_encode()).expect("grant should execute");
            assert!(!output.reverted);
            assert!(token.roles.at(&ISSUER_ROLE).at(&ALICE).read().unwrap());

            let revoke = IBaseDefaultToken::revokeRoleCall { role: ISSUER_ROLE, account: ALICE };
            let output =
                DefaultToken::dispatch(&revoke.abi_encode()).expect("revoke should execute");
            assert!(!output.reverted);
            assert!(!token.roles.at(&ISSUER_ROLE).at(&ALICE).read().unwrap());

            let events = token.emitted_events();
            assert_eq!(events.len(), 2);
            assert_eq!(
                events[0],
                RoleMembershipUpdated {
                    role: ISSUER_ROLE,
                    account: ALICE,
                    sender: ADMIN,
                    hasRole: true,
                }
                .into_log_data()
            );
            assert_eq!(
                events[1],
                RoleMembershipUpdated {
                    role: ISSUER_ROLE,
                    account: ALICE,
                    sender: ADMIN,
                    hasRole: false,
                }
                .into_log_data()
            );
        });
    }

    #[test]
    fn allowance_flow_allows_transfer_from_and_decrements_allowance() {
        let mut storage = HashMapStorageProvider::new(1);

        storage.set_caller(ALICE);
        enter(&mut storage, || {
            let mut token = DefaultToken::new();
            token.balances.at_mut(&ALICE).write(U256::from(100u64)).unwrap();

            let approve =
                IBaseDefaultToken::approveCall { spender: BOB, amount: U256::from(70u64) };
            let output =
                DefaultToken::dispatch(&approve.abi_encode()).expect("approve should execute");
            assert!(!output.reverted);
            assert!(IBaseDefaultToken::approveCall::abi_decode_returns(&output.bytes).unwrap());
            assert_eq!(token.allowances.at(&ALICE).at(&BOB).read().unwrap(), U256::from(70u64));
        });

        storage.set_caller(BOB);
        enter(&mut storage, || {
            let token = DefaultToken::new();
            let transfer = IBaseDefaultToken::transferFromCall {
                from: ALICE,
                to: BOB,
                amount: U256::from(40u64),
            };
            let output = DefaultToken::dispatch(&transfer.abi_encode())
                .expect("transferFrom should execute");
            assert!(!output.reverted);
            assert!(
                IBaseDefaultToken::transferFromCall::abi_decode_returns(&output.bytes).unwrap()
            );

            assert_eq!(token.balances.at(&ALICE).read().unwrap(), U256::from(60u64));
            assert_eq!(token.balances.at(&BOB).read().unwrap(), U256::from(40u64));
            assert_eq!(token.allowances.at(&ALICE).at(&BOB).read().unwrap(), U256::from(30u64));
        });
    }

    #[test]
    fn failed_transfer_from_reverts_allowance_spend() {
        run_as(BOB, || {
            let mut token = DefaultToken::new();
            token.balances.at_mut(&ALICE).write(U256::from(10u64)).unwrap();
            token.allowances.at_mut(&ALICE).at_mut(&BOB).write(U256::from(50u64)).unwrap();

            let transfer = IBaseDefaultToken::transferFromCall {
                from: ALICE,
                to: BOB,
                amount: U256::from(20u64),
            };
            let output =
                DefaultToken::dispatch(&transfer.abi_encode()).expect("transferFrom should revert");
            assert!(output.reverted);
            assert_eq!(output.bytes[..4], InsufficientBalance::SELECTOR);

            assert_eq!(token.balances.at(&ALICE).read().unwrap(), U256::from(10u64));
            assert_eq!(token.balances.at(&BOB).read().unwrap(), U256::ZERO);
            assert_eq!(token.allowances.at(&ALICE).at(&BOB).read().unwrap(), U256::from(50u64));
            assert!(token.emitted_events().is_empty());
        });
    }

    #[test]
    fn static_call_rejects_mutating_selector() {
        run_with_context(ALICE, true, || {
            let mut token = DefaultToken::new();
            token.balances.at_mut(&ALICE).write(U256::from(100u64)).unwrap();

            let transfer = IBaseDefaultToken::transferCall { to: BOB, amount: U256::from(1u64) };
            let output =
                DefaultToken::dispatch(&transfer.abi_encode()).expect("transfer should revert");
            assert!(output.reverted);
            assert_eq!(output.bytes[..4], StaticCallNotAllowed::SELECTOR);

            assert_eq!(token.balances.at(&ALICE).read().unwrap(), U256::from(100u64));
            assert_eq!(token.balances.at(&BOB).read().unwrap(), U256::ZERO);
        });
    }
}
