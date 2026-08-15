// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

import {Script} from "forge-std/Script.sol";
import {console} from "forge-std/console.sol";
import {DevnetSwapRouterShim} from "../src/DevnetSwapRouterShim.sol";
import {DevnetUSDC} from "../src/DevnetUSDC.sol";

interface IWETH {
    function deposit() external payable;
    function transfer(address to, uint256 amount) external returns (bool);
}

contract DeployRealTokenSwapDevnet is Script {
    address internal constant WETH = 0x4200000000000000000000000000000000000006;

    uint256 internal constant DEFAULT_USDC_PER_WETH = 1_000e6;
    uint256 internal constant DEFAULT_ROUTER_USDC_LIQUIDITY = 100_000e6;
    uint256 internal constant DEFAULT_ROUTER_WETH_LIQUIDITY = 100 ether;

    error MissingWethPredeploy(address weth);
    error WethTransferFailed(address router, uint256 amount);

    function run() public {
        if (WETH.code.length == 0) {
            revert MissingWethPredeploy(WETH);
        }

        uint256 usdcPerWeth = vm.envOr("DEVNET_USDC_PER_WETH", DEFAULT_USDC_PER_WETH);
        uint256 routerUsdcLiquidity = vm.envOr("DEVNET_ROUTER_USDC_LIQUIDITY", DEFAULT_ROUTER_USDC_LIQUIDITY);
        uint256 routerWethLiquidity = vm.envOr("DEVNET_ROUTER_WETH_LIQUIDITY", DEFAULT_ROUTER_WETH_LIQUIDITY);

        vm.startBroadcast();

        DevnetUSDC usdc = new DevnetUSDC();
        DevnetSwapRouterShim uniswapRouter = new DevnetSwapRouterShim(WETH, address(usdc), usdcPerWeth);
        DevnetSwapRouterShim aerodromeRouter = new DevnetSwapRouterShim(WETH, address(usdc), usdcPerWeth);

        usdc.mint(address(uniswapRouter), routerUsdcLiquidity);
        usdc.mint(address(aerodromeRouter), routerUsdcLiquidity);

        uint256 totalWethLiquidity = routerWethLiquidity * 2;
        IWETH(WETH).deposit{value: totalWethLiquidity}();

        if (!IWETH(WETH).transfer(address(uniswapRouter), routerWethLiquidity)) {
            revert WethTransferFailed(address(uniswapRouter), routerWethLiquidity);
        }
        if (!IWETH(WETH).transfer(address(aerodromeRouter), routerWethLiquidity)) {
            revert WethTransferFailed(address(aerodromeRouter), routerWethLiquidity);
        }

        console.log("WETH:", WETH);
        console.log("USDC:", address(usdc));
        console.log("Uniswap router shim:", address(uniswapRouter));
        console.log("Aerodrome router shim:", address(aerodromeRouter));
        console.log("USDC per WETH:", usdcPerWeth);
        console.log("USDC liquidity per router:", routerUsdcLiquidity);
        console.log("WETH liquidity per router:", routerWethLiquidity);

        vm.stopBroadcast();
    }
}
