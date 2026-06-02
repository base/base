//! Derives for Base B-20 storage accounting ports.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields};

pub(crate) fn derive_token(input: DeriveInput) -> proc_macro::TokenStream {
    expand_token(input).unwrap_or_else(syn::Error::into_compile_error).into()
}

pub(crate) fn derive_stablecoin(input: DeriveInput) -> proc_macro::TokenStream {
    expand_stablecoin(input).unwrap_or_else(syn::Error::into_compile_error).into()
}

pub(crate) fn derive_security(input: DeriveInput) -> proc_macro::TokenStream {
    expand_security(input).unwrap_or_else(syn::Error::into_compile_error).into()
}

fn expand_token(input: DeriveInput) -> syn::Result<TokenStream> {
    require_field(&input, "b20")?;
    let has_redeem = has_field(&input, "redeem");
    let name = input.ident;
    let redeem = has_redeem.then_some(quote! {
        if policy_scope == Self::REDEEM_SENDER_POLICY {
            return self.redeem.redeem_sender_policy_id();
        }
    });
    let set_redeem = has_redeem.then_some(quote! {
        if policy_scope == Self::REDEEM_SENDER_POLICY {
            return self.redeem.set_redeem_sender_policy_id(policy_id);
        }
    });

    Ok(quote! {
        impl #name<'_> {
            fn __require_policy_type(
                policy_scope: ::alloy_primitives::B256,
            ) -> ::base_precompile_storage::Result<crate::B20PolicyType> {
                crate::B20PolicyType::from_id(policy_scope).ok_or_else(|| {
                    ::base_precompile_storage::BasePrecompileError::revert(
                        crate::IB20::UnsupportedPolicyType { policyScope: policy_scope },
                    )
                })
            }
        }

        impl crate::TokenAccounting for #name<'_> {
            fn token_address(&self) -> ::alloy_primitives::Address {
                ::base_precompile_storage::ContractStorage::address(self)
            }

            fn is_initialized(&self) -> ::base_precompile_storage::Result<bool> {
                ::base_precompile_storage::ContractStorage::is_initialized(self)
            }

            fn balance_of(
                &self,
                account: ::alloy_primitives::Address,
            ) -> ::base_precompile_storage::Result<::alloy_primitives::U256> {
                self.b20.balance_of(account)
            }

            fn set_balance(
                &mut self,
                account: ::alloy_primitives::Address,
                balance: ::alloy_primitives::U256,
            ) -> ::base_precompile_storage::Result<()> {
                self.b20.set_balance(account, balance)
            }

            fn allowance(
                &self,
                owner: ::alloy_primitives::Address,
                spender: ::alloy_primitives::Address,
            ) -> ::base_precompile_storage::Result<::alloy_primitives::U256> {
                self.b20.allowance(owner, spender)
            }

            fn set_allowance(
                &mut self,
                owner: ::alloy_primitives::Address,
                spender: ::alloy_primitives::Address,
                amount: ::alloy_primitives::U256,
            ) -> ::base_precompile_storage::Result<()> {
                self.b20.set_allowance(owner, spender, amount)
            }

            fn total_supply(
                &self,
            ) -> ::base_precompile_storage::Result<::alloy_primitives::U256> {
                self.b20.total_supply()
            }

            fn set_total_supply(
                &mut self,
                supply: ::alloy_primitives::U256,
            ) -> ::base_precompile_storage::Result<()> {
                self.b20.set_total_supply(supply)
            }

            fn supply_cap(&self) -> ::base_precompile_storage::Result<::alloy_primitives::U256> {
                self.b20.supply_cap()
            }

            fn set_supply_cap(
                &mut self,
                cap: ::alloy_primitives::U256,
            ) -> ::base_precompile_storage::Result<()> {
                self.b20.set_supply_cap(cap)
            }

            fn name(&self) -> ::base_precompile_storage::Result<::alloc::string::String> {
                self.b20.name()
            }

            fn set_name(
                &mut self,
                name: ::alloc::string::String,
            ) -> ::base_precompile_storage::Result<()> {
                self.b20.set_name(name)
            }

            fn symbol(&self) -> ::base_precompile_storage::Result<::alloc::string::String> {
                self.b20.symbol()
            }

            fn set_symbol(
                &mut self,
                symbol: ::alloc::string::String,
            ) -> ::base_precompile_storage::Result<()> {
                self.b20.set_symbol(symbol)
            }

            fn decimals(&self) -> ::base_precompile_storage::Result<u8> {
                Ok(crate::B20Variant::from_address(
                    ::base_precompile_storage::ContractStorage::address(self),
                )
                .map_or(0, crate::B20Variant::decimals))
            }

            fn paused(&self) -> ::base_precompile_storage::Result<::alloy_primitives::U256> {
                self.b20.paused()
            }

            fn set_paused(
                &mut self,
                vectors: ::alloy_primitives::U256,
            ) -> ::base_precompile_storage::Result<()> {
                self.b20.set_paused(vectors)
            }

            fn nonce(
                &self,
                owner: ::alloy_primitives::Address,
            ) -> ::base_precompile_storage::Result<::alloy_primitives::U256> {
                self.b20.nonce(owner)
            }

            fn increment_nonce(
                &mut self,
                owner: ::alloy_primitives::Address,
            ) -> ::base_precompile_storage::Result<()> {
                let current = self.b20.nonce(owner)?;
                let next = current
                    .checked_add(::alloy_primitives::U256::ONE)
                    .ok_or_else(::base_precompile_storage::BasePrecompileError::under_overflow)?;
                self.b20.set_nonce(owner, next)
            }

            fn contract_uri(&self) -> ::base_precompile_storage::Result<::alloc::string::String> {
                self.b20.contract_uri()
            }

            fn set_contract_uri(
                &mut self,
                uri: ::alloc::string::String,
            ) -> ::base_precompile_storage::Result<()> {
                self.b20.set_contract_uri(uri)
            }

            fn has_role(
                &self,
                role: ::alloy_primitives::B256,
                account: ::alloy_primitives::Address,
            ) -> ::base_precompile_storage::Result<bool> {
                self.b20.has_role(role, account)
            }

            fn set_role(
                &mut self,
                role: ::alloy_primitives::B256,
                account: ::alloy_primitives::Address,
                enabled: bool,
            ) -> ::base_precompile_storage::Result<()> {
                self.b20.set_role(role, account, enabled)
            }

            fn role_member_count(
                &self,
                role: ::alloy_primitives::B256,
            ) -> ::base_precompile_storage::Result<::alloy_primitives::U256> {
                if role == crate::B20TokenRole::DefaultAdmin.id() {
                    self.b20.admin_count()
                } else {
                    Ok(::alloy_primitives::U256::ZERO)
                }
            }

            fn set_role_member_count(
                &mut self,
                role: ::alloy_primitives::B256,
                count: ::alloy_primitives::U256,
            ) -> ::base_precompile_storage::Result<()> {
                if role == crate::B20TokenRole::DefaultAdmin.id() {
                    self.b20.set_admin_count(count)
                } else {
                    Ok(())
                }
            }

            fn role_admin(
                &self,
                role: ::alloy_primitives::B256,
            ) -> ::base_precompile_storage::Result<::alloy_primitives::B256> {
                let admin_role = self.b20.role_admin(role)?;
                if admin_role.is_zero() && role != crate::B20TokenRole::DefaultAdmin.id() {
                    Ok(crate::B20TokenRole::DefaultAdmin.id())
                } else {
                    Ok(admin_role)
                }
            }

            fn set_role_admin(
                &mut self,
                role: ::alloy_primitives::B256,
                admin_role: ::alloy_primitives::B256,
            ) -> ::base_precompile_storage::Result<()> {
                self.b20.set_role_admin(role, admin_role)
            }

            fn policy_id(
                &self,
                policy_scope: ::alloy_primitives::B256,
            ) -> ::base_precompile_storage::Result<u64> {
                #redeem
                match Self::__require_policy_type(policy_scope)? {
                    crate::B20PolicyType::TransferSender => self.b20.transfer_sender_policy_id(),
                    crate::B20PolicyType::TransferReceiver => {
                        self.b20.transfer_receiver_policy_id()
                    }
                    crate::B20PolicyType::TransferExecutor => {
                        self.b20.transfer_executor_policy_id()
                    }
                    crate::B20PolicyType::MintReceiver => self.b20.mint_receiver_policy_id(),
                }
            }

            fn set_policy_id(
                &mut self,
                policy_scope: ::alloy_primitives::B256,
                policy_id: u64,
            ) -> ::base_precompile_storage::Result<()> {
                #set_redeem
                match Self::__require_policy_type(policy_scope)? {
                    crate::B20PolicyType::TransferSender => {
                        self.b20.set_transfer_sender_policy_id(policy_id)
                    }
                    crate::B20PolicyType::TransferReceiver => {
                        self.b20.set_transfer_receiver_policy_id(policy_id)
                    }
                    crate::B20PolicyType::TransferExecutor => {
                        self.b20.set_transfer_executor_policy_id(policy_id)
                    }
                    crate::B20PolicyType::MintReceiver => {
                        self.b20.set_mint_receiver_policy_id(policy_id)
                    }
                }
            }

            fn emit_event(
                &mut self,
                log: ::alloy_primitives::LogData,
            ) -> ::base_precompile_storage::Result<()> {
                self.emit_event(log)
            }
        }
    })
}

fn expand_stablecoin(input: DeriveInput) -> syn::Result<TokenStream> {
    require_field(&input, "stablecoin")?;
    let name = input.ident;
    Ok(quote! {
        impl crate::StablecoinAccounting for #name<'_> {
            fn currency(&self) -> ::base_precompile_storage::Result<::alloc::string::String> {
                self.stablecoin.currency()
            }

            fn set_currency(
                &mut self,
                currency: ::alloc::string::String,
            ) -> ::base_precompile_storage::Result<()> {
                self.stablecoin.set_currency(currency)
            }
        }
    })
}

fn expand_security(input: DeriveInput) -> syn::Result<TokenStream> {
    require_field(&input, "security")?;
    require_field(&input, "redeem")?;
    let name = input.ident;
    Ok(quote! {
        impl crate::SecurityAccounting for #name<'_> {
            fn multiplier(
                &self,
            ) -> ::base_precompile_storage::Result<::alloy_primitives::U256> {
                let multiplier = self.security.multiplier()?;
                Ok(if multiplier.is_zero() { Self::WAD } else { multiplier })
            }

            fn set_multiplier(
                &mut self,
                multiplier: ::alloy_primitives::U256,
            ) -> ::base_precompile_storage::Result<()> {
                self.security.set_multiplier(multiplier)
            }

            fn extra_metadata(
                &self,
                identifier_type: &str,
            ) -> ::base_precompile_storage::Result<::alloc::string::String> {
                ::base_precompile_storage::Handler::read(
                    self.security
                        .identifiers
                        .at(&::alloc::string::String::from(identifier_type)),
                )
            }

            fn set_extra_metadata_value(
                &mut self,
                identifier_type: &str,
                value: ::alloc::string::String,
            ) -> ::base_precompile_storage::Result<()> {
                let key = ::alloc::string::String::from(identifier_type);
                if value.is_empty() {
                    ::base_precompile_storage::Handler::delete(self.security.identifiers.at_mut(&key))
                } else {
                    ::base_precompile_storage::Handler::write(
                        self.security.identifiers.at_mut(&key),
                        value,
                    )
                }
            }

            fn minimum_redeemable(
                &self,
            ) -> ::base_precompile_storage::Result<::alloy_primitives::U256> {
                self.redeem.minimum_redeemable()
            }

            fn set_minimum_redeemable(
                &mut self,
                minimum: ::alloy_primitives::U256,
            ) -> ::base_precompile_storage::Result<()> {
                self.redeem.set_minimum_redeemable(minimum)
            }

            fn is_announcement_id_used(
                &self,
                id: &str,
            ) -> ::base_precompile_storage::Result<bool> {
                ::base_precompile_storage::Handler::read(
                    self.security
                        .used_announcement_ids
                        .at(&::alloc::string::String::from(id)),
                )
            }

            fn mark_announcement_id_used(
                &mut self,
                id: &str,
            ) -> ::base_precompile_storage::Result<()> {
                ::base_precompile_storage::Handler::write(
                    self.security
                        .used_announcement_ids
                        .at_mut(&::alloc::string::String::from(id)),
                    true,
                )
            }
        }
    })
}

fn require_field(input: &DeriveInput, name: &str) -> syn::Result<()> {
    if has_field(input, name) {
        Ok(())
    } else {
        Err(syn::Error::new_spanned(&input.ident, format!("missing `{name}` field")))
    }
}

fn has_field(input: &DeriveInput, name: &str) -> bool {
    let Data::Struct(data) = &input.data else {
        return false;
    };
    let Fields::Named(fields) = &data.fields else {
        return false;
    };
    fields.named.iter().any(|field| field.ident.as_ref().is_some_and(|ident| ident == name))
}
