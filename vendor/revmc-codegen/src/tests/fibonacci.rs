use paste::paste;
use revm_bytecode::opcode as op;
use revm_interpreter::InstructionResult;
use revm_primitives::U256;

use super::{DEF_SPEC, with_evm_context};
use crate::{Backend, EvmCompiler};

macro_rules! fibonacci_tests {
    ($($i:expr),* $(,)?) => {paste! {
        $(
            matrix_tests!([<native_ $i>] = |jit| run_fibonacci_test(jit, $i, false));
            matrix_tests!([<dynamic_ $i>] = |jit| run_fibonacci_test(jit, $i, true));
        )*
    }};
}

fibonacci_tests!(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 20, 30, 40, 50, 100, 1000);

fn run_fibonacci_test<B: Backend>(compiler: &mut EvmCompiler<B>, input: u16, dynamic: bool) {
    let code = mk_fibonacci_code(input, dynamic);

    unsafe { compiler.clear() }.unwrap();
    compiler.inspect_stack(true);
    let f = unsafe { compiler.jit("fib", &code, DEF_SPEC) }.unwrap();

    with_evm_context(&code, DEF_SPEC, |ecx, stack, stack_len| {
        if dynamic {
            stack.set(0, U256::from(input).into());
            *stack_len = 1;
        }
        let r = unsafe { f.call(stack, stack_len, ecx) };
        assert_eq!(r, InstructionResult::Stop);
        // Apparently the code does `fibonacci(input + 1)`.
        assert_eq!(*stack_len, 1);
        assert_eq!(unsafe { stack.as_slice(*stack_len) }[0].to_u256(), fibonacci_rust(input + 1));
    });
}

fn mk_fibonacci_code(input: u16, dynamic: bool) -> Vec<u8> {
    if dynamic {
        [&[op::JUMPDEST; 3][..], FIBONACCI_CODE].concat()
    } else {
        let input = input.to_be_bytes();
        [[op::PUSH2].as_slice(), input.as_slice(), FIBONACCI_CODE].concat()
    }
}

// Modified from jitevm: https://github.com/paradigmxyz/jitevm/blob/f82261fc8a1a6c1a3d40025a910ba0ce3fcaed71/src/test_data.rs#L3
#[rustfmt::skip]
const FIBONACCI_CODE: &[u8] = &[
    // Expects the code to be offset 3 bytes.
    // JUMPDEST, JUMPDEST, JUMPDEST,

    // 1st/2nd fib number
    op::PUSH1, 0,
    op::PUSH1, 1,
    // 7

    // MAINLOOP:
    op::JUMPDEST,
    op::DUP3,
    op::ISZERO,
    op::PUSH1, 28, // cleanup
    op::JUMPI,

    // fib step
    op::DUP2,
    op::DUP2,
    op::ADD,
    op::SWAP2,
    op::POP,
    op::SWAP1,
    // 19

    // decrement fib step counter
    op::SWAP2,
    op::PUSH1, 1,
    op::SWAP1,
    op::SUB,
    op::SWAP2,
    op::PUSH1, 7, // goto MAINLOOP
    op::JUMP,
    // 28

    // CLEANUP:
    op::JUMPDEST,
    op::SWAP2,
    op::POP,
    op::POP,
    // done: requested fib number is the only element on the stack!
    op::STOP,
];

fn fibonacci_rust(n: u16) -> U256 {
    let mut a = U256::from(0);
    let mut b = U256::from(1);
    for _ in 0..n {
        let tmp = a;
        a = b;
        b = b.wrapping_add(tmp);
    }
    a
}

#[test]
fn test_fibonacci_rust() {
    revm_primitives::uint! {
        assert_eq!(fibonacci_rust(0), 0_U256);
        assert_eq!(fibonacci_rust(1), 1_U256);
        assert_eq!(fibonacci_rust(2), 1_U256);
        assert_eq!(fibonacci_rust(3), 2_U256);
        assert_eq!(fibonacci_rust(4), 3_U256);
        assert_eq!(fibonacci_rust(5), 5_U256);
        assert_eq!(fibonacci_rust(6), 8_U256);
        assert_eq!(fibonacci_rust(7), 13_U256);
        assert_eq!(fibonacci_rust(8), 21_U256);
        assert_eq!(fibonacci_rust(9), 34_U256);
        assert_eq!(fibonacci_rust(10), 55_U256);
        assert_eq!(fibonacci_rust(100), 354224848179261915075_U256);
        assert_eq!(fibonacci_rust(1000), 0x2e3510283c1d60b00930b7e8803c312b4c8e6d5286805fc70b594dc75cc0604b_U256);
    }
}
