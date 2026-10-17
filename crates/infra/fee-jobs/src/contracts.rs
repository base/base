use alloy_sol_types::sol;

sol! {
    /// L2 fee disbursement entry points and scheduling getters.
    interface FeeDisburser {
        /// Disburses accumulated fees.
        function disburseFees() external;
        /// Timestamp of the last disbursement, in seconds.
        function lastDisbursementTime() external view returns (uint256);
        /// Minimum time between disbursements, in seconds.
        function FEE_DISBURSEMENT_INTERVAL() external view returns (uint256);
    }

    /// L1 fee processing entry point.
    interface BalanceTracker {
        /// Processes accumulated fees.
        function processFees() external;
    }

    /// Basenames registrar revenue withdrawal entry point.
    interface RegistrarController {
        /// Withdraws the controller's ETH balance.
        function withdrawETH() external;
    }
}
