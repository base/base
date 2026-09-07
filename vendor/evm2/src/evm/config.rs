//! EVM configuration.

use core::fmt::Debug;

use derive_where::derive_where;

use crate::{
    Evm, OpcodeConfig, SpecId,
    ethereum::TxEnvelope,
    interpreter::{
        Host,
        dispatch::{ConfigInstrTables, InstrTable, SelectorInstrTables},
    },
    version::Version,
};

/// Runtime EVM type family.
///
/// Defines the concrete host, transaction, runtime spec-id, and config selector types used by an
/// EVM instance. The runtime spec-id can be a custom type, but each value must map to the base
/// `SpecId` whose rules it inherits.
// TODO: Custom hosts aren't that well supported yet.
#[doc(hidden)]
pub trait EvmTypesHost: Sized + 'static {
    /// Configuration selector used by this EVM.
    type ConfigSelector: EvmConfigSelector<Self>;

    /// Runtime specification ID accepted by this EVM.
    ///
    /// Custom EVMs may use their own ID type, but every value must map to a base `SpecId`.
    type SpecId: Copy + Into<SpecId>;

    /// Transaction type handled by this EVM.
    type Tx;

    /// EVM instance-specific extension state.
    type EvmExt;

    /// Extra data stored in frame messages.
    type MessageExt: Clone + Debug + Default;

    /// Extra data stored in message execution results.
    type MessageResultExt: Clone + Debug + Default;

    /// Extra data stored in transaction environments.
    type TxEnvExt: Clone + Debug + Default;

    /// Extra data stored in transaction execution results.
    type TxResultExt: Clone + Debug + Default;

    /// Extra data stored in block environments.
    type BlockEnvExt: Copy + Debug + Default;

    /// Host type used by this EVM.
    type Host<'a>: Host<Self> + ?Sized;
}

/// Runtime EVM type family whose host is [`Evm`] for every stored-object lifetime.
pub trait EvmTypes: for<'a> EvmTypesHost<Host<'a> = Evm<'a, Self>> {}

impl<T> EvmTypes for T where for<'a> T: EvmTypesHost<Host<'a> = Evm<'a, T>> {}

/// Compile-time EVM table configuration.
///
/// Names the inherited base `SpecId` and the type-specific `OpcodeConfig` needed to build a
/// dispatch table. Custom configs are modeled as overlays on a base spec, not as new base specs.
pub trait EvmConfig<T: EvmTypesHost> {
    /// Inherited base specification ID.
    const BASE_SPEC_ID: SpecId;

    /// Active type-specific opcode config.
    const OPCODE_CONFIG: &'static OpcodeConfig<T>;
}

/// Runtime EVM config selector.
///
/// Maps a runtime spec-id value accepted by `EvmTypesHost` to the `ExecutionConfig` that the EVM
/// and interpreter use. Custom selectors can choose different configs for runtime IDs that share
/// the same inherited base `SpecId`, while base selectors use `u32::MAX` as the custom-spec
/// sentinel.
pub trait EvmConfigSelector<T: EvmTypesHost>: Sized {
    /// Concrete EVM configuration for a base specification ID and custom specification ID.
    ///
    /// `BASE_SPEC_ID` is always a `crate::SpecId` discriminant, even when `T::SpecId` is a custom
    /// runtime ID type. `CUSTOM_SPEC_ID` is a selector-specific `u32` discriminant; `u32::MAX`
    /// represents the base table inherited from `BASE_SPEC_ID`.
    type Config<const BASE_SPEC_ID: u32, const CUSTOM_SPEC_ID: u32>: EvmConfig<T>;

    /// Returns the selected execution config for `spec_id`.
    fn execution_config(spec_id: T::SpecId) -> ExecutionConfig<T>;
}

pub(crate) struct SelectorOpcodeConfig<T, F, const CUSTOM_SPEC_ID: u32>(
    core::marker::PhantomData<fn() -> (T, F)>,
);

impl<T, F, const CUSTOM_SPEC_ID: u32> SelectorOpcodeConfig<T, F, CUSTOM_SPEC_ID>
where
    T: EvmTypesHost,
    F: EvmConfigSelector<T>,
{
    pub(crate) const OPCODE_CONFIG: &'static [&'static OpcodeConfig<T>; SpecId::COUNT] =
        &selector_opcode_config::<T, F, CUSTOM_SPEC_ID>();
}

const fn selector_opcode_config<T, F, const CUSTOM_SPEC_ID: u32>()
-> [&'static OpcodeConfig<T>; SpecId::COUNT]
where
    T: EvmTypesHost,
    F: EvmConfigSelector<T>,
{
    macro_rules! opcode_config {
        ([$evm_types:ty, $selector:ty] $($spec:ident $name:ident,)*) => {
            [
                $(
                    <<$selector as EvmConfigSelector<$evm_types>>::Config<
                        { SpecId::$spec as u32 },
                        CUSTOM_SPEC_ID,
                    > as EvmConfig<$evm_types>>::OPCODE_CONFIG,
                )*
            ]
        };
    }

    crate::for_each_spec!([T, F] opcode_config)
}

/// Selected execution configuration.
///
/// Bundles the active runtime `Version` with the finalized instruction dispatch table selected for
/// an EVM instance. This is the data passed to the interpreter when it runs.
#[derive_where(Debug)]
pub struct ExecutionConfig<T: EvmTypesHost> {
    base_spec_id: SpecId,
    pub(crate) version: Version,
    #[derive_where(skip)]
    pub(crate) instructions: &'static InstrTable<T>,
    #[derive_where(skip)]
    pub(crate) inspect_instructions: &'static InstrTable<T>,
}

impl<T: EvmTypesHost> Clone for ExecutionConfig<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: EvmTypesHost> Copy for ExecutionConfig<T> {}

impl<T: EvmTypesHost> ExecutionConfig<T> {
    /// Creates an execution config for a base `SpecId` through selector `F`.
    ///
    /// This uses the selector's base inherited tables by passing `u32::MAX` as the custom-spec
    /// sentinel.
    #[inline]
    pub(crate) const fn for_base_spec<F: EvmConfigSelector<T>>(base_spec_id: SpecId) -> Self {
        Self::for_custom_spec::<F, { u32::MAX }>(base_spec_id)
    }

    /// Creates an execution config for selector custom spec `CUSTOM_SPEC_ID` and base `SpecId`.
    #[inline]
    pub(crate) const fn for_custom_spec<F: EvmConfigSelector<T>, const CUSTOM_SPEC_ID: u32>(
        base_spec_id: SpecId,
    ) -> Self {
        let i = base_spec_id as usize;
        Self {
            base_spec_id,
            version: Version::new(base_spec_id),
            instructions: &SelectorInstrTables::<T, F, CUSTOM_SPEC_ID>::INSTRUCTIONS[i],
            inspect_instructions:
                &SelectorInstrTables::<T, F, CUSTOM_SPEC_ID>::INSPECT_INSTRUCTIONS[i],
        }
    }

    /// Creates an execution config for concrete EVM configuration `C`.
    #[inline]
    pub const fn for_config<C: EvmConfig<T>>() -> Self {
        let base_spec_id = C::BASE_SPEC_ID;
        Self {
            base_spec_id,
            version: Version::new(base_spec_id),
            instructions: ConfigInstrTables::<T, C>::INSTRUCTIONS,
            inspect_instructions: ConfigInstrTables::<T, C>::INSPECT_INSTRUCTIONS,
        }
    }

    /// Creates an execution config for `spec_id` with dynamic runtime version data.
    #[inline]
    pub fn for_spec_and_version(spec_id: T::SpecId, version: Version) -> Self {
        let config = <T::ConfigSelector as EvmConfigSelector<T>>::execution_config(spec_id);
        assert_eq!(spec_id.into(), config.base_spec_id, "execution config spec mismatch");
        config.with_version(version)
    }

    /// Replaces the runtime version data while keeping the same dispatch table.
    #[inline]
    pub const fn with_version(mut self, version: Version) -> Self {
        self.version = version;
        self
    }

    /// Returns the active base specification ID.
    #[inline]
    pub const fn base_spec_id(&self) -> SpecId {
        self.base_spec_id
    }

    /// Returns the active EVM version.
    #[inline]
    pub const fn version(&self) -> &Version {
        &self.version
    }
}

/// Base EVM types.
#[allow(missing_copy_implementations, missing_debug_implementations)]
pub struct BaseEvmTypes(());

impl EvmTypesHost for BaseEvmTypes {
    type ConfigSelector = BaseEvmConfigSelector;
    type SpecId = SpecId;
    type Tx = TxEnvelope;
    type EvmExt = ();
    type MessageExt = ();
    type MessageResultExt = ();
    type TxEnvExt = ();
    type TxResultExt = ();
    type BlockEnvExt = ();
    type Host<'a> = Evm<'a, Self>;
}

/// Base EVM configuration for an inherited base specification ID.
///
/// `BASE_SPEC_ID` is the raw discriminant of `crate::SpecId`, not a custom runtime spec-id value.
#[allow(missing_copy_implementations, missing_debug_implementations)]
pub struct BaseEvmConfig<const BASE_SPEC_ID: u32>(());

impl<T: EvmTypesHost, const BASE_SPEC_ID: u32> EvmConfig<T> for BaseEvmConfig<BASE_SPEC_ID> {
    const BASE_SPEC_ID: SpecId = SpecId::try_from_u32(BASE_SPEC_ID).expect("invalid spec id");
    const OPCODE_CONFIG: &'static OpcodeConfig<T> = &OpcodeConfig::<T>::base::<Self>();
}

/// Base EVM config selector.
#[allow(missing_copy_implementations, missing_debug_implementations)]
pub struct BaseEvmConfigSelector(());

impl<T: EvmTypesHost> EvmConfigSelector<T> for BaseEvmConfigSelector {
    type Config<const BASE_SPEC_ID: u32, const CUSTOM_SPEC_ID: u32> = BaseEvmConfig<BASE_SPEC_ID>;

    fn execution_config(spec_id: T::SpecId) -> ExecutionConfig<T> {
        ExecutionConfig::for_base_spec::<Self>(spec_id.into())
    }
}
