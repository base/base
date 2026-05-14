use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface ICounter {
        function increment() external;
        function getCount() external view returns (uint256 count);
    }
}
