// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

import {FreeTransferERC20} from "./FreeTransferERC20.sol";

/// @notice Minimal mock Uniswap V3 router for load testing.
/// @dev Implements exactInputSingle without real pool math. Uses FreeTransferERC20
///      tokens which allow unrestricted transferFrom and have a public mint function.
///
///      Setup: pre-mint 1 billion tokens of each type to this router's address so
///      the output-token reserve never runs out during a benchmark run.
contract MockUniswapV3Router {
    /// @notice Parameters for a single-hop exact-input swap.
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    /// @notice Execute a single-hop exact-input swap.
    /// @dev Pulls `amountIn` of `tokenIn` from the caller (no approval required with
    ///      FreeTransferERC20), then pushes `amountIn` of `tokenOut` to `recipient`
    ///      from the router's own balance.
    /// @param params Swap parameters.
    /// @return amountOut The amount of tokenOut received (equal to amountIn).
    function exactInputSingle(ExactInputSingleParams calldata params)
        external
        payable
        returns (uint256 amountOut)
    {
        FreeTransferERC20(params.tokenIn).transferFrom(
            msg.sender,
            address(this),
            params.amountIn
        );
        FreeTransferERC20(params.tokenOut).transferFrom(
            address(this),
            params.recipient,
            params.amountIn
        );
        return params.amountIn;
    }
}
