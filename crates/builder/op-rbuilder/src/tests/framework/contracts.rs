pub mod flashblocks_number_contract {
    use alloy_sol_types::sol;
    sol!(
        // https://github.com/Uniswap/flashblocks_number_contract/tree/c21ca0aedc3ff4d1eecf20cd55abeb984080bc78
        #[sol(rpc, abi)]
        FlashblocksNumber,
        "src/tests/framework/artifacts/contracts/FlashblocksNumberContract.json",
    );
}
