use revmc_backend::{
    Attribute, Backend, Builder, CallConv, FunctionAttributeLocation, TypeMethods,
};

// Must be kept in sync with `remvc-build`.
const MANGLE_PREFIX: &str = "__revmc_builtin_";

/// Builtin cache.
#[derive(Debug)]
pub struct Builtins<B: Backend>([Option<B::Function>; Builtin::COUNT]);

unsafe impl<B: Backend> Send for Builtins<B> {}

impl<B: Backend> Default for Builtins<B> {
    fn default() -> Self {
        Self::new()
    }
}

impl<B: Backend> Builtins<B> {
    /// Create a new cache.
    pub fn new() -> Self {
        Self([None; Builtin::COUNT])
    }

    /// Clear the cache.
    pub fn clear(&mut self) {
        *self = Self::new();
    }

    /// Get the function for the given builtin.
    pub fn get(&mut self, builtin: Builtin, bcx: &mut B::Builder<'_>) -> B::Function {
        *self.0[builtin as usize].get_or_insert_with(|| Self::init(builtin, bcx))
    }

    #[cold]
    fn init(builtin: Builtin, bcx: &mut B::Builder<'_>) -> B::Function {
        let name = builtin.name();
        debug_assert!(name.starts_with(MANGLE_PREFIX), "{name:?}");
        if builtin.call_conv() == CallConv::Default
            && let Some(r) = bcx.get_function(name)
        {
            trace!(name, ?r, "pre-existing");
            return r;
        }

        let r = Self::build(name, builtin, bcx);
        trace!(name, ?r, "built");
        r
    }

    fn build(name: &str, builtin: Builtin, bcx: &mut B::Builder<'_>) -> B::Function {
        let ret = builtin.ret(bcx);
        let params = builtin.params(bcx);
        let address = builtin.addr();
        let linkage = revmc_backend::Linkage::Import;
        let f = bcx.add_function(name, &params, ret, Some(address), linkage, CallConv::Default);
        Self::add_attrs(builtin, f, bcx);
        match builtin.call_conv() {
            CallConv::Default => f,
            call_conv => {
                let f = bcx.add_function_stub(f, call_conv);
                Self::add_attrs(builtin, f, bcx);
                f
            }
        }
    }

    fn add_attrs(builtin: Builtin, f: B::Function, bcx: &mut B::Builder<'_>) {
        let param_attrs = builtin.param_attrs();
        let mut attrs = Vec::with_capacity(16);
        attrs.extend(builtin.attrs());
        attrs.extend([
            Attribute::NoFree,
            Attribute::NoRecurse,
            Attribute::NoSync,
            Attribute::NoUnwind,
        ]);
        // `argmem` is only valid if the function does not access memory reachable through pointers
        // *loaded* from its arguments (only memory directly derived from arguments via
        // GEP/bitcast). Builtins that take a writable `EvmContext` mutate state through
        // `ecx.host`, `ecx.gas`, etc., which are loaded from `ecx` and thus outside
        // `argmem`. Apply `ArgMemOnly` only when no parameter is a writable `EvmContext`.
        let evm_ctx_size = core::mem::size_of::<revmc_context::EvmContext<'static>>() as u64;
        let writes_ecx = param_attrs.iter().any(|p| {
            let writable = p.iter().any(|a| matches!(a, Attribute::Writable));
            let is_ecx =
                p.iter().any(|a| matches!(a, Attribute::Dereferenceable(s) if *s == evm_ctx_size));
            writable && is_ecx
        });
        if !writes_ecx {
            attrs.push(Attribute::ArgMemOnly);
        }
        for attr in attrs {
            bcx.add_function_attribute(Some(f), attr, FunctionAttributeLocation::Function);
        }
        for (i, param_attrs) in param_attrs.iter().enumerate() {
            for attr in param_attrs {
                bcx.add_function_attribute(
                    Some(f),
                    *attr,
                    FunctionAttributeLocation::Param(i as u32),
                );
            }
        }
    }
}

macro_rules! builtins {
    (@count) => { 0 };
    (@count $first:tt $(, $rest:tt)*) => { 1 + builtins!(@count $($rest),*) };

    (@param_attr $default:ident) => { $default() };
    (@param_attr $default:ident $name:expr) => { $name };

    (@types |$bcx:ident| { $($types_init:tt)* }
     @param_attrs |$op:ident| { $($attrs_init:tt)* }
     $($ident:ident = $(#[$attr:expr])* $name:ident($($(@[$param_attr:expr])? $params:expr),* $(,)?) $ret:expr),* $(,)?
    ) => { paste::paste! {
        /// Builtins that can be called by the compiled functions.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Builtin {
            $($ident,)*
        }

        #[allow(unused_variables)]
        impl Builtin {
            pub const COUNT: usize = builtins!(@count $($ident),*);
            pub const ALL: &[Self; Self::COUNT] = &[$(Self::$ident),*];

            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$ident => stringify!($name),)*
                }
            }

            pub fn addr(self) -> usize {
                match self {
                    $(Self::$ident => crate::$name as *const () as usize,)*
                }
            }

            pub fn parse(name: &str) -> Option<Self> {
                match name {
                    $(stringify!($name) => Some(Self::$ident),)*
                    _ => None,
                }
            }

            pub fn ret<B: TypeMethods>(self, $bcx: &mut B) -> Option<B::Type> {
                $($types_init)*
                match self {
                    $(Self::$ident => $ret,)*
                }
            }

            pub fn params<B: TypeMethods>(self, $bcx: &mut B) -> Vec<B::Type> {
                $($types_init)*
                match self {
                    $(Self::$ident => vec![$($params),*],)*
                }
            }

            pub fn attrs(self) -> &'static [Attribute] {
                #[allow(unused_imports)]
                use Attribute::*;
                match self {
                    $(Self::$ident => &[$($attr),*]),*
                }
            }

            #[allow(non_upper_case_globals)]
            pub fn param_attrs(self) -> Vec<Vec<Attribute>> {
                #[allow(unused_imports)]
                use Attribute::*;
                let $op = self;
                let default = || vec![Attribute::NoUndef];
                $($attrs_init)*
                match self {
                    $(Self::$ident => vec![$(builtins!(@param_attr default $($param_attr)?)),*]),*
                }
            }

            fn op(self) -> u8 {
                use revm_bytecode::opcode::*;

                // _in_out
                const _0_0: u8 = STOP;
                const _0_1: u8 = PUSH0;
                const _1_0: u8 = POP;

                const PANIC: u8 = _0_0;
                const ASSERTSPECID: u8 = _0_0;

                const EXPGAS: u8 = _1_0;
                const KECCAK256CC: u8 = _0_1;

                const CALLDATALOADC: u8 = _0_1;
                const SLOADC: u8 = _0_1;

                const MRESIZE: u8 = _0_0;

                const LOG: u8 = _0_0;
                const DORETURN: u8 = RETURN;
                const DORETURNCC: u8 = _0_0;

                match self {
                    $(Self::$ident => [<$ident:upper>]),*
                }
            }
        }
    }};
}

builtins! {
    @types |bcx| {
        let ptr = bcx.type_ptr();
        let usize = bcx.type_ptr_sized_int();
        let u8 = bcx.type_int(8);
    }

    @param_attrs |op| {
        fn size_and_align<T>() -> Vec<Attribute> {
            size_and_align_with(Some(core::mem::size_of::<T>()), core::mem::align_of::<T>())
        }

        fn size_and_align_with(size: Option<usize>, align: usize) -> Vec<Attribute> {
            let mut vec = Vec::with_capacity(8);
            vec.push(Attribute::NoAlias);
            vec.push(Attribute::NoCapture);
            vec.push(Attribute::NoUndef);
            vec.push(Attribute::Align(align as u64));
            if let Some(size) = size {
                vec.push(Attribute::Dereferenceable(size as u64));
            }
            vec
        }

        let op = op.op();
        let (inputs, outputs) = if let Some(info) = revm_bytecode::opcode::OPCODE_INFO[op as usize] {
            (info.inputs(), info.outputs())
        } else {
            (0, 0)
        };

        let ecx_base = size_and_align::<revmc_context::EvmContext<'static>>();
        let mut ecx = ecx_base.clone();
        ecx.push(Attribute::Writable);
        let mut ecx_ro = ecx_base;
        ecx_ro.push(Attribute::ReadOnly);

        let sp_base = size_and_align_with(None, core::mem::align_of::<revmc_context::EvmWord>());
        let mut sp_dyn = sp_base.clone();
        sp_dyn.push(Attribute::Writable);

        let mut sp = sp_base;
        // `sp` is at `top - inputs`, we have access to `max(inputs, outputs)` words.
        let n_stack_words = inputs.max(outputs);
        let size_of_word = core::mem::size_of::<revmc_context::EvmWord>();
        sp.push(Attribute::Dereferenceable(size_of_word as u64 * n_stack_words as u64));
        match (inputs, outputs) {
            (0, 0) => sp.push(Attribute::ReadNone),
            (0, 1..) => {
                sp.push(Attribute::Writable);
                sp.push(Attribute::WriteOnly);
                sp.push(Attribute::Initializes(size_of_word as u64 * outputs as u64));
            }
            (1.., 0) => sp.push(Attribute::ReadOnly),
            (1.., 1..) => sp.push(Attribute::Writable),
        }
    }

    Panic          = #[Cold] #[NoReturn] __revmc_builtin_panic(ptr, usize) None,
    AssertSpecId   = __revmc_builtin_assert_spec_id(@[ecx] ptr, u8) None,

    Div            = __revmc_builtin_div(@[sp] ptr) None,
    SDiv           = __revmc_builtin_sdiv(@[sp] ptr) None,
    Mod            = __revmc_builtin_mod(@[sp] ptr) None,
    SMod           = __revmc_builtin_smod(@[sp] ptr) None,
    AddMod         = __revmc_builtin_addmod(@[sp] ptr) None,
    MulMod         = __revmc_builtin_mulmod(@[sp] ptr) None,
    Exp            = __revmc_builtin_exp(@[ecx] ptr, @[sp] ptr) None,
    ExpGas         = __revmc_builtin_exp_gas(@[ecx] ptr, @[sp] ptr) None,
    Keccak256      = __revmc_builtin_keccak256(@[ecx] ptr, @[sp] ptr) None,
    Keccak256CC    = __revmc_builtin_keccak256_cc(@[ecx] ptr, @[sp] ptr, usize, usize) None,
    Balance        = __revmc_builtin_balance(@[ecx] ptr, @[sp] ptr) None,
    Origin         = __revmc_builtin_origin(@[ecx_ro] ptr, @[sp] ptr) None,
    CallDataLoad   = __revmc_builtin_calldataload(@[ecx_ro] ptr, @[sp] ptr) None,
    CallDataLoadC  = __revmc_builtin_calldataload_c(@[ecx_ro] ptr, @[sp] ptr, usize) None,
    CallDataCopy   = __revmc_builtin_calldatacopy(@[ecx] ptr, @[sp] ptr) None,
    CodeCopy       = __revmc_builtin_codecopy(@[ecx] ptr, @[sp] ptr) None,
    GasPrice       = __revmc_builtin_gas_price(@[ecx_ro] ptr, @[sp] ptr) None,
    ExtCodeSize    = __revmc_builtin_extcodesize(@[ecx] ptr, @[sp] ptr) None,
    ExtCodeCopy    = __revmc_builtin_extcodecopy(@[ecx] ptr, @[sp] ptr) None,
    ReturnDataCopy = __revmc_builtin_returndatacopy(@[ecx] ptr, @[sp] ptr) None,
    ExtCodeHash    = __revmc_builtin_extcodehash(@[ecx] ptr, @[sp] ptr) None,
    BlockHash      = __revmc_builtin_blockhash(@[ecx] ptr, @[sp] ptr) None,
    Coinbase       = __revmc_builtin_coinbase(@[ecx_ro] ptr, @[sp] ptr) None,
    Timestamp      = __revmc_builtin_timestamp(@[ecx_ro] ptr, @[sp] ptr) None,
    Number         = __revmc_builtin_number(@[ecx_ro] ptr, @[sp] ptr) None,
    Difficulty     = __revmc_builtin_difficulty(@[ecx_ro] ptr, @[sp] ptr) None,
    GasLimit       = __revmc_builtin_gaslimit(@[ecx_ro] ptr, @[sp] ptr) None,
    ChainId        = __revmc_builtin_chainid(@[ecx_ro] ptr, @[sp] ptr) None,
    SelfBalance    = __revmc_builtin_self_balance(@[ecx] ptr, @[sp] ptr) None,
    Basefee        = __revmc_builtin_basefee(@[ecx_ro] ptr, @[sp] ptr) None,
    BlobHash       = __revmc_builtin_blob_hash(@[ecx_ro] ptr, @[sp] ptr) None,
    BlobBaseFee    = __revmc_builtin_blob_base_fee(@[ecx_ro] ptr, @[sp] ptr) None,
    SlotNum        = __revmc_builtin_slot_num(@[ecx_ro] ptr, @[sp] ptr) None,
    Mresize        = __revmc_builtin_mresize(@[ecx] ptr, usize) None,
    Sload          = __revmc_builtin_sload(@[ecx] ptr, @[sp] ptr) None,
    SloadC         = __revmc_builtin_sload_c(@[ecx] ptr, @[sp] ptr, usize) None,
    Sstore         = __revmc_builtin_sstore(@[ecx] ptr, @[sp] ptr) None,
    Tload          = __revmc_builtin_tload(@[ecx] ptr, @[sp] ptr) None,
    Tstore         = __revmc_builtin_tstore(@[ecx] ptr, @[sp] ptr) None,
    Mcopy          = __revmc_builtin_mcopy(@[ecx] ptr, @[sp] ptr) None,
    Log            = __revmc_builtin_log(@[ecx] ptr, @[sp_dyn] ptr, u8) None,

    Create         = __revmc_builtin_create(@[ecx] ptr, @[sp_dyn] ptr, u8) None,
    Call           = __revmc_builtin_call(@[ecx] ptr, @[sp_dyn] ptr, u8) None,
    DoReturn       = #[NoReturn] __revmc_builtin_do_return(@[ecx] ptr, @[sp] ptr, u8) None,
    DoReturnCC     = #[NoReturn] __revmc_builtin_do_return_cc(@[ecx] ptr, usize, usize, u8) None,
    SelfDestruct   = #[NoReturn] __revmc_builtin_selfdestruct(@[ecx] ptr, @[sp] ptr) None,
}

impl Builtin {
    pub const fn call_conv(self) -> CallConv {
        match self {
            Self::Mresize => CallConv::Cold,
            _ => CallConv::Default,
        }
    }
}
