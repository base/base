macro_rules! matrix_tests {
    ($run:ident) => {
        #[cfg(feature = "llvm")]
        mod llvm {
            #[allow(unused_imports)]
            use similar_asserts_2_0::assert_eq;

            use super::*;

            fn run_llvm(compiler: &mut EvmCompiler<revmc_llvm::EvmLlvmBackend>) {
                if std::env::var_os("REVMC_TEST_DUMP").is_some() {
                    crate::tests::set_test_dump(compiler, module_path!());
                }
                $run(compiler);
            }

            #[test]
            fn unopt() {
                crate::tests::with_jit_compiler(revmc_backend::OptimizationLevel::None, run_llvm);
            }

            #[test]
            fn opt() {
                crate::tests::with_jit_compiler(
                    revmc_backend::OptimizationLevel::default(),
                    run_llvm,
                );
            }
        }
    };

    ($name:ident = | $compiler:ident | $e:expr) => {
        mod $name {
            #[allow(unused_imports)]
            use similar_asserts_2_0::assert_eq;

            use super::*;

            fn run_generic<B: Backend>($compiler: &mut EvmCompiler<B>) {
                $e;
            }

            matrix_tests!(run_generic);
        }
    };
    ($name:ident = $run:ident) => {
        mod $name {
            #[allow(unused_imports)]
            use similar_asserts_2_0::assert_eq;

            use super::*;

            matrix_tests!($run);
        }
    };
}

macro_rules! build_push32 {
    ($code:ident[$i:ident], $x:expr) => {{
        $code[$i] = op::PUSH32;
        $i += 1;
        $code[$i..$i + 32].copy_from_slice(&$x.to_be_bytes::<32>());
        $i += 32;
    }};
}

macro_rules! tests {
    ($($group:ident { $($t:tt)* })*) => { uint! {
        $(
            mod $group {
                use super::*;
                #[allow(unused_imports)]
                use similar_asserts_2_0::assert_eq;

                tests!(@cases $($t)*);
            }
        )*
    }};

    (@cases $( $name:ident($($t:tt)*) ),* $(,)?) => {
        $(
            matrix_tests!($name = |jit| run_test_case(tests!(@case $($t)*), jit));
            tests!(@maybe_opaque $name($($t)*));
        )*
    };

    // Generate companion tests for every const/dynamic operand combination.
    // Dynamic operands use MSTORE+MLOAD to make them invisible to the compiler.
    (@maybe_opaque $name:ident(@raw { $($fields:tt)* })) => {};
    (@maybe_opaque $name:ident($op:expr, $a:expr => $($ret:expr),* $(; $($_r1:tt)*)?)) => {
        paste::paste! {
            matrix_tests!([<$name _dyn>] = |jit| run_test_case(
                &TestCase {
                    bytecode: &bytecode_unop_opaque($op, $a),
                    expected_stack: &[$($ret),*],
                    expected_memory: MEMORY_WHAT_INTERPRETER_SAYS,
                    expected_gas: GAS_WHAT_INTERPRETER_SAYS,
                    ..Default::default()
                },
                jit,
            ));
        }
    };
    (@maybe_opaque $name:ident($op:expr, $a:expr, $b:expr => $($ret:expr),* $(; $($_r2:tt)*)?)) => {
        paste::paste! {
            tests!(@mixed_binop [<$name _const_dyn>] $op, $a, $b, true, false => $($ret),*);
            tests!(@mixed_binop [<$name _dyn_const>] $op, $a, $b, false, true => $($ret),*);
            tests!(@mixed_binop [<$name _dyn_dyn>] $op, $a, $b, false, false => $($ret),*);
        }
    };
    (@maybe_opaque $name:ident($op:expr, $a:expr, $b:expr, $c:expr => $($ret:expr),* $(; $($_r3:tt)*)?)) => {
        paste::paste! {
            tests!(@mixed_ternop [<$name _const_const_dyn>] $op, $a, $b, $c, true, true, false => $($ret),*);
            tests!(@mixed_ternop [<$name _const_dyn_const>] $op, $a, $b, $c, true, false, true => $($ret),*);
            tests!(@mixed_ternop [<$name _const_dyn_dyn>] $op, $a, $b, $c, true, false, false => $($ret),*);
            tests!(@mixed_ternop [<$name _dyn_const_const>] $op, $a, $b, $c, false, true, true => $($ret),*);
            tests!(@mixed_ternop [<$name _dyn_const_dyn>] $op, $a, $b, $c, false, true, false => $($ret),*);
            tests!(@mixed_ternop [<$name _dyn_dyn_const>] $op, $a, $b, $c, false, false, true => $($ret),*);
            tests!(@mixed_ternop [<$name _dyn_dyn_dyn>] $op, $a, $b, $c, false, false, false => $($ret),*);
        }
    };

    (@mixed_binop $name:ident $op:expr, $a:expr, $b:expr, $a_const:expr, $b_const:expr => $($ret:expr),*) => {
        matrix_tests!($name = |jit| run_test_case(
            &TestCase {
                bytecode: &bytecode_binop_mixed($op, $a, $b, $a_const, $b_const),
                expected_stack: &[$($ret),*],
                expected_memory: MEMORY_WHAT_INTERPRETER_SAYS,
                expected_gas: GAS_WHAT_INTERPRETER_SAYS,
                ..Default::default()
            },
            jit,
        ));
    };

    (@mixed_ternop $name:ident $op:expr, $a:expr, $b:expr, $c:expr, $a_const:expr, $b_const:expr, $c_const:expr => $($ret:expr),*) => {
        matrix_tests!($name = |jit| run_test_case(
            &TestCase {
                bytecode: &bytecode_ternop_mixed($op, $a, $b, $c, $a_const, $b_const, $c_const),
                expected_stack: &[$($ret),*],
                expected_memory: MEMORY_WHAT_INTERPRETER_SAYS,
                expected_gas: GAS_WHAT_INTERPRETER_SAYS,
                ..Default::default()
            },
            jit,
        ));
    };

    (@case @raw { $($fields:tt)* }) => { &TestCase { $($fields)* ..Default::default() } };

    (@case $op:expr $(, $args:expr)* $(,)? => $($ret:expr),* $(,)? $(; op_gas($op_gas:expr))?) => {
        &TestCase {
            bytecode: &tests!(@bytecode $op, $($args),*),
            expected_stack: &[$($ret),*],
            expected_gas: tests!(@gas $op $(, $op_gas)?; $($args),*),
            ..Default::default()
        }
    };

    (@bytecode $op:expr, $a:expr) => { bytecode_unop($op, $a) };
    (@bytecode $op:expr, $a:expr, $b:expr) => { bytecode_binop($op, $a, $b) };
    (@bytecode $op:expr, $a:expr, $b:expr, $c:expr) => { bytecode_ternop($op, $a, $b, $c) };

    (@gas $op:expr; $($args:expr),+) => {
        tests!(@gas
            $op,
            DEF_OPINFOS[$op as usize].base_gas() as u64;
            $($args),+
        )
    };
    (@gas $op:expr, $op_gas:expr; $($args:expr),+) => {
        $op_gas + tests!(@gas_base $($args),+)
    };
    (@gas_base $a:expr) => { 3 };
    (@gas_base $a:expr, $b:expr) => { 6 };
    (@gas_base $a:expr, $b:expr, $c:expr) => { 9 };
}
