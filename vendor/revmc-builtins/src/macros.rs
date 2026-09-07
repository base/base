#[collapse_debuginfo(yes)]
macro_rules! gas {
    ($ecx:expr, $gas:expr) => {
        if !$ecx.gas.record_regular_cost($gas) {
            core::hint::cold_path();
            return Err(InstructionResult::OutOfGas.into());
        }
    };
}

#[collapse_debuginfo(yes)]
macro_rules! state_gas {
    ($ecx:expr, $gas:expr) => {
        if !$ecx.gas.record_state_cost($gas) {
            core::hint::cold_path();
            return Err(InstructionResult::OutOfGas.into());
        }
    };
}

#[collapse_debuginfo(yes)]
macro_rules! ensure_non_staticcall {
    ($ecx:expr) => {
        if $ecx.is_static {
            core::hint::cold_path();
            return Err(InstructionResult::StateChangeDuringStaticCall.into());
        }
    };
}

/// Same as `read_words_rev`, but returns the arguments in the order they were passed.
#[collapse_debuginfo(yes)]
macro_rules! read_words {
    ($sp:expr, $($words:ident),+ $(,)?) => {
        let rev![$($words),+] = unsafe { read_words_rev($sp) };
    };
}

#[collapse_debuginfo(yes)]
macro_rules! pop {
    ($sp:expr; $($x:ident),* $(,)?) => {
        $(
            $sp = $sp.sub(1);
            let $x = &mut *$sp;
        )*
    };
}

#[collapse_debuginfo(yes)]
macro_rules! try_into_usize {
    ($x:expr) => {
        match $x.to_u256().as_limbs() {
            x => {
                if (x[0] > usize::MAX as u64) | (x[1] != 0) | (x[2] != 0) | (x[3] != 0) {
                    core::hint::cold_path();
                    return Err(InstructionResult::InvalidOperandOOG.into());
                }
                x[0] as usize
            }
        }
    };
}

// Credits: <https://github.com/AuroransSolis/rustconf-2023/blob/665a645d751dfe0e483261e3abca25ab4bb9e13a/reverse-tokens/src/main.rs>
#[collapse_debuginfo(yes)]
macro_rules! rev {
	(@rev [$first:tt$(, $rest:tt)*] [$($rev:tt),*]) => {
		rev! {
			@rev [$($rest),*][$first $(, $rev)*]
		}
	};
	(@rev [] [$($rev:tt),*]) => {
		[$($rev)*] // NOTE: Extra `[]` to make this an array pattern.
	};
	($($tt:tt)+) => {
		rev! {
			@rev [$($tt),+] []
		}
	};
}

#[collapse_debuginfo(yes)]
macro_rules! debug_unreachable {
    ($($t:tt)*) => {
        if cfg!(debug_assertions) {
            unreachable!($($t)*);
        } else {
            unsafe { core::hint::unreachable_unchecked() };
        }
    };
}

#[collapse_debuginfo(yes)]
macro_rules! assume {
    ($e:expr $(,)?) => {
        if !$e {
            debug_unreachable!(stringify!($e));
        }
    };

    ($e:expr, $($t:tt)+) => {
        if !$e {
            debug_unreachable!($($t)+);
        }
    };
}
