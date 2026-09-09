// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

import {Test} from "forge-std/Test.sol";
import {MockERC20} from "solmate/test/utils/mocks/MockERC20.sol";
import {DevnetSwapRouterShim} from "../src/DevnetSwapRouterShim.sol";
import {DevnetUSDC} from "../src/DevnetUSDC.sol";

contract DevnetSwapRouterShimTest is Test {
    uint256 internal constant USDC_PER_WETH = 1_000e6;

    address internal user = address(0x1234);

    MockERC20 internal weth;
    DevnetUSDC internal usdc;
    DevnetSwapRouterShim internal router;

    function setUp() public {
        weth = new MockERC20("Wrapped Ether", "WETH", 18);
        usdc = new DevnetUSDC();
        router = new DevnetSwapRouterShim(address(weth), address(usdc), USDC_PER_WETH);

        weth.mint(user, 1 ether);
        usdc.mint(user, 100e6);

        weth.mint(address(router), 10 ether);
        usdc.mint(address(router), 10_000e6);
    }

    function testUniswapExactInputSingleWethToUsdc() public {
        vm.startPrank(user);
        weth.approve(address(router), type(uint256).max);

        uint256 amountOut = router.exactInputSingle(
            DevnetSwapRouterShim.UniswapExactInputSingleParams({
                tokenIn: address(weth),
                tokenOut: address(usdc),
                fee: 500,
                recipient: user,
                amountIn: 0.01 ether,
                amountOutMinimum: 10e6,
                sqrtPriceLimitX96: 0
            })
        );

        vm.stopPrank();

        assertEq(amountOut, 10e6);
        assertEq(weth.balanceOf(user), 0.99 ether);
        assertEq(usdc.balanceOf(user), 110e6);
    }

    function testAerodromeExactInputSingleUsdcToWeth() public {
        vm.startPrank(user);
        usdc.approve(address(router), type(uint256).max);

        uint256 amountOut = router.exactInputSingle(
            DevnetSwapRouterShim.AerodromeExactInputSingleParams({
                tokenIn: address(usdc),
                tokenOut: address(weth),
                tickSpacing: 100,
                recipient: user,
                deadline: block.timestamp,
                amountIn: 1e6,
                amountOutMinimum: 0.001 ether,
                sqrtPriceLimitX96: 0
            })
        );

        vm.stopPrank();

        assertEq(amountOut, 0.001 ether);
        assertEq(usdc.balanceOf(user), 99e6);
        assertEq(weth.balanceOf(user), 1.001 ether);
    }

    function testRevertsWhenAllowanceIsMissing() public {
        vm.expectRevert();
        vm.prank(user);
        router.exactInputSingle(
            DevnetSwapRouterShim.UniswapExactInputSingleParams({
                tokenIn: address(weth),
                tokenOut: address(usdc),
                fee: 500,
                recipient: user,
                amountIn: 0.01 ether,
                amountOutMinimum: 10e6,
                sqrtPriceLimitX96: 0
            })
        );
    }

    function testRevertsOnUnsupportedPair() public {
        MockERC20 other = new MockERC20("Other", "OTHER", 18);
        other.mint(user, 1 ether);

        vm.startPrank(user);
        other.approve(address(router), type(uint256).max);
        vm.expectRevert(
            abi.encodeWithSelector(DevnetSwapRouterShim.UnsupportedPair.selector, address(other), address(usdc))
        );
        router.exactInputSingle(
            DevnetSwapRouterShim.UniswapExactInputSingleParams({
                tokenIn: address(other),
                tokenOut: address(usdc),
                fee: 500,
                recipient: user,
                amountIn: 0.01 ether,
                amountOutMinimum: 0,
                sqrtPriceLimitX96: 0
            })
        );
        vm.stopPrank();
    }
}
