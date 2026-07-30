//! The stablecoin-specific `IB20Stablecoin` wire surface frozen at Beryl, the stablecoin's
//! activation fork. Unchanged since; later versions alias this surface (see [`super`]).

use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IB20Stablecoin {
        function currency() external view returns (string);
    }
}
