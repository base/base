//! Runtime helpers for wrapping native precompile dispatch.

macro_rules! base_precompile {
    ($id:expr, |$ctx:ident, $calldata:ident| $impl:expr $(,)?) => {{
        ::alloy_evm::precompiles::DynPrecompile::new_stateful(
            ::revm::precompile::PrecompileId::Custom($id.into()),
            move |input| {
                if !input.is_direct_call() {
                    return Ok(::revm::precompile::PrecompileOutput::new_reverted(
                        0,
                        ::alloy_primitives::Bytes::new(),
                    ));
                }

                let $calldata: ::alloy_primitives::Bytes = input.data.to_vec().into();
                let mut provider =
                    ::base_precompile_storage::EvmPrecompileStorageProvider::new(input);

                ::base_precompile_storage::StorageCtx::enter(&mut provider, |$ctx| $impl)
            },
        )
    }};
    ($id:expr, |$input:ident, $ctx:ident, $calldata:ident| $impl:expr $(,)?) => {{
        ::alloy_evm::precompiles::DynPrecompile::new_stateful(
            ::revm::precompile::PrecompileId::Custom($id.into()),
            move |$input| {
                if !$input.is_direct_call() {
                    return Ok(::revm::precompile::PrecompileOutput::new_reverted(
                        0,
                        ::alloy_primitives::Bytes::new(),
                    ));
                }

                let $calldata: ::alloy_primitives::Bytes = $input.data.to_vec().into();
                let mut provider =
                    ::base_precompile_storage::EvmPrecompileStorageProvider::new($input);

                ::base_precompile_storage::StorageCtx::enter(&mut provider, |$ctx| $impl)
            },
        )
    }};
}

pub(crate) use base_precompile;
