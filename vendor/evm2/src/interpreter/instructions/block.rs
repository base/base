use evm2_macros::instruction;

use crate::{
    EvmFeatures,
    constants::BLOCK_HASH_HISTORY,
    interpreter::{Host, Word},
    utils::{address_to_word, b256_to_word, word_to_usize_saturated},
};

#[instruction]
pub(crate) fn blockhash(cx: _, [number]: [Word]) -> Result<out> {
    *out = if let Some(diff) = cx.state.host().block_env().number.checked_sub(*number) {
        if diff == 0 || diff > BLOCK_HASH_HISTORY {
            Word::ZERO
        } else {
            b256_to_word(cx.state.host().block_hash(number)?)
        }
    } else {
        Word::ZERO
    };
}

#[instruction]
pub(crate) fn coinbase(cx: _) -> out {
    *out = address_to_word(&cx.state.host().block_env().beneficiary);
}

#[instruction]
pub(crate) fn timestamp(cx: _) -> out {
    *out = cx.state.host().block_env().timestamp;
}

#[instruction]
pub(crate) fn block_number(cx: _) -> out {
    *out = cx.state.host().block_env().number;
}

#[instruction]
pub(crate) fn difficulty(cx: _) -> out {
    *out = if cx.state.feature(EvmFeatures::EIP4399) {
        cx.state.host().block_env().prevrandao
    } else {
        cx.state.host().block_env().difficulty
    };
}

#[instruction]
pub(crate) fn gaslimit(cx: _) -> out {
    *out = cx.state.host().block_env().gas_limit;
}

#[instruction]
pub(crate) fn chainid(cx: _) -> Result<out> {
    *out = cx.state.tx().chain_id;
}

#[instruction]
pub(crate) fn selfbalance(cx: _) -> Result<out> {
    let destination = &cx.state.message().destination;
    *out = cx.state.host().load_account(destination, false, false)?.balance;
}

#[instruction]
pub(crate) fn basefee(cx: _) -> Result<out> {
    *out = cx.state.host().block_env().basefee;
}

#[instruction]
pub(crate) fn blobhash(cx: _, [index]: [Word]) -> Result<out> {
    let index = word_to_usize_saturated(*index);
    *out = cx.state.tx().blob_hashes.get(index).copied().unwrap_or_default();
}

#[instruction]
pub(crate) fn blobbasefee(cx: _) -> Result<out> {
    *out = cx.state.host().block_env().blob_basefee;
}

#[instruction]
pub(crate) fn slotnum(cx: _) -> Result<out> {
    *out = cx.state.host().block_env().slot_num;
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::assert_matches;

    use alloy_primitives::{Address, B256};

    use crate::{
        SpecId,
        env::{BlockEnv, BlockEnvExt, TxEnvExt},
        interpreter::{InstrStop, MessageExt, Word, op},
        test_utils::{RunConfig, TestHost, TestTypes, push, run},
        utils::{address_to_word, b256_to_word},
    };

    fn test_host(block: BlockEnv<TestTypes>) -> TestHost {
        TestHost { block, ..TestHost::default() }
    }

    #[test]
    fn blockhash_opcode() {
        let mut host = test_host(BlockEnvExt { number: Word::from(10), ..BlockEnvExt::default() });
        let mut code = Vec::new();
        push(&mut code, 9);
        code.push(op::BLOCKHASH);
        code.push(op::STOP);

        let interp = run(RunConfig::new(code).host(&mut host));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [b256_to_word(B256::with_last_byte(9))]);

        let mut code = Vec::new();
        push(&mut code, 10);
        code.push(op::BLOCKHASH);
        code.push(op::STOP);
        let interp = run(RunConfig::new(code).host(&mut host));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [0]);
    }

    #[test]
    fn missing_blockhash_is_fatal() {
        let mut host = test_host(BlockEnvExt { number: Word::from(10), ..BlockEnvExt::default() });
        host.missing_block_hash = true;
        let mut code = Vec::new();
        push(&mut code, 9);
        code.push(op::BLOCKHASH);
        code.push(op::STOP);

        let interp = run(RunConfig::new(code).host(&mut host));
        assert_matches!(interp.err, InstrStop::FatalExternalError);
    }

    #[test]
    fn coinbase_opcode() {
        let beneficiary = Address::from([0x44; 20]);
        let mut host = test_host(BlockEnvExt { beneficiary, ..BlockEnvExt::default() });
        let interp = run(RunConfig::new([op::COINBASE, op::STOP]).host(&mut host));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [address_to_word(&beneficiary)]);
    }

    #[test]
    fn timestamp_opcode() {
        let mut host =
            test_host(BlockEnvExt { timestamp: Word::from(12), ..BlockEnvExt::default() });
        let interp = run(RunConfig::new([op::TIMESTAMP, op::STOP]).host(&mut host));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(12)]);
    }

    #[test]
    fn number_opcode() {
        let mut host = test_host(BlockEnvExt { number: Word::from(13), ..BlockEnvExt::default() });
        let interp = run(RunConfig::new([op::NUMBER, op::STOP]).host(&mut host));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(13)]);
    }

    #[test]
    fn difficulty_opcode() {
        let randao = B256::with_last_byte(0x55);
        let mut host = test_host(BlockEnvExt {
            difficulty: Word::from(14),
            prevrandao: b256_to_word(randao),
            ..BlockEnvExt::default()
        });
        let interp =
            run(RunConfig::new([op::DIFFICULTY, op::STOP]).host(&mut host).spec(SpecId::FRONTIER));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(14)]);

        let interp =
            run(RunConfig::new([op::DIFFICULTY, op::STOP]).host(&mut host).spec(SpecId::MERGE));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [b256_to_word(randao)]);
    }

    #[test]
    fn gaslimit_opcode() {
        let mut host =
            test_host(BlockEnvExt { gas_limit: Word::from(15), ..BlockEnvExt::default() });
        let interp = run(RunConfig::new([op::GASLIMIT, op::STOP]).host(&mut host));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(15)]);
    }

    #[test]
    fn chainid_opcode() {
        let mut host = TestHost::default();
        let tx_env = TxEnvExt { chain_id: Word::from(1), ..TxEnvExt::default() };
        let interp = run(RunConfig::new([op::CHAINID, op::STOP])
            .host(&mut host)
            .tx_env(tx_env)
            .spec(SpecId::ISTANBUL));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(1)]);
    }

    #[test]
    fn selfbalance_opcode() {
        let address = Address::from([0x66; 20]);
        let mut host = TestHost::default();
        let message = MessageExt { destination: address, gas_limit: 10_000, ..Default::default() };
        let interp =
            run(RunConfig::new([op::SELFBALANCE, op::STOP]).host(&mut host).message(message));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [address_to_word(&address)]);
    }

    #[test]
    fn basefee_opcode() {
        let mut host = test_host(BlockEnvExt { basefee: Word::from(16), ..BlockEnvExt::default() });
        let interp =
            run(RunConfig::new([op::BASEFEE, op::STOP]).host(&mut host).spec(SpecId::LONDON));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(16)]);
    }

    #[test]
    fn blobhash_opcode() {
        let hash = B256::with_last_byte(0x42);
        let mut host = TestHost::default();
        let tx_env =
            TxEnvExt { blob_hashes: Vec::from([b256_to_word(hash)]), ..TxEnvExt::default() };

        let interp = run(RunConfig::new([op::PUSH0, op::BLOBHASH, op::STOP])
            .host(&mut host)
            .tx_env(tx_env.clone())
            .spec(SpecId::CANCUN));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [b256_to_word(hash)]);

        let interp = run(RunConfig::new([op::PUSH1, 0x01, op::BLOBHASH, op::STOP])
            .host(&mut host)
            .tx_env(tx_env)
            .spec(SpecId::CANCUN));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [0]);
    }

    #[test]
    fn blobbasefee_opcode() {
        let mut host =
            test_host(BlockEnvExt { blob_basefee: Word::from(17), ..BlockEnvExt::default() });
        let interp =
            run(RunConfig::new([op::BLOBBASEFEE, op::STOP]).host(&mut host).spec(SpecId::CANCUN));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(17)]);
    }

    #[test]
    fn slotnum_opcode() {
        let mut host =
            test_host(BlockEnvExt { slot_num: Word::from(18), ..BlockEnvExt::default() });
        let interp =
            run(RunConfig::new([op::SLOTNUM, op::STOP]).host(&mut host).spec(SpecId::AMSTERDAM));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(18)]);
    }
}
