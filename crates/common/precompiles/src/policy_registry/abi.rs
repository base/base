use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IPolicyRegistry {
        function helloWorld() external view returns (bool);
    }
}
