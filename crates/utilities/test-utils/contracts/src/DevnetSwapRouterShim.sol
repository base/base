// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

interface IERC20Like {
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}

/// @notice Minimal router shim for devnet WETH/USDC load-test validation.
/// @dev Supports the exact calldata shapes generated for Uniswap V3 and Aerodrome CL swaps.
contract DevnetSwapRouterShim {
    struct UniswapExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    struct AerodromeExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        int24 tickSpacing;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    event Swap(
        address indexed caller,
        address indexed tokenIn,
        address indexed tokenOut,
        address recipient,
        uint256 amountIn,
        uint256 amountOut,
        bytes4 selector
    );

    error DeadlineExpired(uint256 deadline, uint256 timestamp);
    error InsufficientOutput(uint256 amountOut, uint256 amountOutMinimum);
    error MissingTokenAddress();
    error TransferFailed(address token, address from, address to, uint256 amount);
    error UnexpectedValue(uint256 value);
    error UnsupportedPair(address tokenIn, address tokenOut);
    error ZeroAmount();
    error ZeroRecipient();

    address public immutable WETH;
    address public immutable USDC;
    uint256 public immutable USDC_PER_WETH;

    constructor(address weth_, address usdc_, uint256 usdcPerWeth_) {
        if (weth_ == address(0) || usdc_ == address(0)) {
            revert MissingTokenAddress();
        }
        if (usdcPerWeth_ == 0) {
            revert ZeroAmount();
        }

        WETH = weth_;
        USDC = usdc_;
        USDC_PER_WETH = usdcPerWeth_;
    }

    function exactInputSingle(UniswapExactInputSingleParams calldata params)
        external
        payable
        returns (uint256 amountOut)
    {
        amountOut = _swap(
            params.tokenIn, params.tokenOut, params.recipient, params.amountIn, params.amountOutMinimum, msg.sig
        );
    }

    function exactInputSingle(AerodromeExactInputSingleParams calldata params)
        external
        payable
        returns (uint256 amountOut)
    {
        if (params.deadline < block.timestamp) {
            revert DeadlineExpired(params.deadline, block.timestamp);
        }

        amountOut =
            _swap(params.tokenIn, params.tokenOut, params.recipient, params.amountIn, params.amountOutMinimum, msg.sig);
    }

    function quoteExactInput(address tokenIn, address tokenOut, uint256 amountIn)
        public
        view
        returns (uint256 amountOut)
    {
        if (amountIn == 0) {
            revert ZeroAmount();
        }

        if (tokenIn == WETH && tokenOut == USDC) {
            return amountIn * USDC_PER_WETH / 1 ether;
        }
        if (tokenIn == USDC && tokenOut == WETH) {
            return amountIn * 1 ether / USDC_PER_WETH;
        }

        revert UnsupportedPair(tokenIn, tokenOut);
    }

    function _swap(
        address tokenIn,
        address tokenOut,
        address recipient,
        uint256 amountIn,
        uint256 amountOutMinimum,
        bytes4 selector
    ) internal returns (uint256 amountOut) {
        if (msg.value != 0) {
            revert UnexpectedValue(msg.value);
        }
        if (recipient == address(0)) {
            revert ZeroRecipient();
        }

        amountOut = quoteExactInput(tokenIn, tokenOut, amountIn);
        if (amountOut < amountOutMinimum) {
            revert InsufficientOutput(amountOut, amountOutMinimum);
        }

        bool pulled = IERC20Like(tokenIn).transferFrom(msg.sender, address(this), amountIn);
        if (!pulled) {
            revert TransferFailed(tokenIn, msg.sender, address(this), amountIn);
        }

        bool paid = IERC20Like(tokenOut).transfer(recipient, amountOut);
        if (!paid) {
            revert TransferFailed(tokenOut, address(this), recipient, amountOut);
        }

        emit Swap(msg.sender, tokenIn, tokenOut, recipient, amountIn, amountOut, selector);
    }
}
