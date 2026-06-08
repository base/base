// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

import {Script} from "forge-std/Script.sol";
import {console} from "forge-std/console.sol";
import {MockERC20} from "solmate/test/utils/mocks/MockERC20.sol";

interface IUniswapV3Factory {
    function createPool(address tokenA, address tokenB, uint24 fee) external returns (address pool);
    function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address pool);
}

interface IUniswapV3Pool {
    function initialize(uint160 sqrtPriceX96) external;
}

interface INonfungiblePositionManager {
    struct MintParams {
        address token0;
        address token1;
        uint24 fee;
        int24 tickLower;
        int24 tickUpper;
        uint256 amount0Desired;
        uint256 amount1Desired;
        uint256 amount0Min;
        uint256 amount1Min;
        address recipient;
        uint256 deadline;
    }

    function mint(MintParams calldata params)
        external
        payable
        returns (uint256 tokenId, uint128 liquidity, uint256 amount0, uint256 amount1);
}

interface ICLFactory {
    function getPool(address tokenA, address tokenB, int24 tickSpacing) external view returns (address pool);
    function createPool(address tokenA, address tokenB, int24 tickSpacing, uint160 sqrtPriceX96)
        external
        returns (address pool);
}

interface ICLPositionManager {
    struct MintParams {
        address token0;
        address token1;
        int24 tickSpacing;
        int24 tickLower;
        int24 tickUpper;
        uint256 amount0Desired;
        uint256 amount1Desired;
        uint256 amount0Min;
        uint256 amount1Min;
        address recipient;
        uint256 deadline;
        uint160 sqrtPriceX96;
    }

    function mint(MintParams calldata params)
        external
        payable
        returns (uint256 tokenId, uint128 liquidity, uint256 amount0, uint256 amount1);
}

contract SeedDexPools is Script {
    // 1:1 price ratio: sqrt(1) * 2^96
    uint160 constant SQRT_PRICE_1_1 = 79228162514264337593543950336;

    uint256 constant LIQUIDITY_AMOUNT = 100_000 ether;

    function run() public {
        address tokenA = vm.envAddress("TOKEN_A");
        address tokenB = vm.envAddress("TOKEN_B");

        vm.startBroadcast();

        if (vm.envOr("UNISWAP_V3_FACTORY", address(0)) != address(0)) {
            _seedUniswapV3(tokenA, tokenB);
        }

        if (vm.envOr("AERODROME_CL_FACTORY", address(0)) != address(0)) {
            _seedAerodromeCl(tokenA, tokenB);
        }

        vm.stopBroadcast();
    }

    function _seedUniswapV3(address tokenA, address tokenB) internal {
        address factory = vm.envAddress("UNISWAP_V3_FACTORY");
        address positionManager = vm.envAddress("UNISWAP_V3_POSITION_MANAGER");
        uint24 fee = uint24(vm.envOr("UNISWAP_V3_FEE", uint256(3000)));

        (address token0, address token1) = tokenA < tokenB ? (tokenA, tokenB) : (tokenB, tokenA);

        address pool = IUniswapV3Factory(factory).getPool(token0, token1, fee);
        if (pool == address(0)) {
            pool = IUniswapV3Factory(factory).createPool(token0, token1, fee);
            IUniswapV3Pool(pool).initialize(SQRT_PRICE_1_1);
            console.log("Uniswap V3 pool created:", pool);
        } else {
            console.log("Uniswap V3 pool exists:", pool);
        }

        MockERC20(token0).approve(positionManager, type(uint256).max);
        MockERC20(token1).approve(positionManager, type(uint256).max);

        // Full-range position: tickLower=-887220, tickUpper=887220
        // These are the widest valid ticks divisible by common tick spacings (60 for 0.3% fee).
        INonfungiblePositionManager(positionManager).mint(
            INonfungiblePositionManager.MintParams({
                token0: token0,
                token1: token1,
                fee: fee,
                tickLower: -887220,
                tickUpper: 887220,
                amount0Desired: LIQUIDITY_AMOUNT,
                amount1Desired: LIQUIDITY_AMOUNT,
                amount0Min: 0,
                amount1Min: 0,
                recipient: msg.sender,
                deadline: block.timestamp + 1 hours
            })
        );
        console.log("Uniswap V3 liquidity seeded");
    }

    function _seedAerodromeCl(address tokenA, address tokenB) internal {
        address clFactory = vm.envAddress("AERODROME_CL_FACTORY");
        address clPositionManager = vm.envAddress("AERODROME_CL_POSITION_MANAGER");
        int24 tickSpacing = int24(int256(vm.envOr("AERODROME_TICK_SPACING", uint256(100))));

        (address token0, address token1) = tokenA < tokenB ? (tokenA, tokenB) : (tokenB, tokenA);

        address pool = ICLFactory(clFactory).getPool(token0, token1, tickSpacing);
        if (pool == address(0)) {
            pool = ICLFactory(clFactory).createPool(token0, token1, tickSpacing, SQRT_PRICE_1_1);
            console.log("Aerodrome CL pool created:", pool);
        } else {
            console.log("Aerodrome CL pool exists:", pool);
        }

        MockERC20(token0).approve(clPositionManager, type(uint256).max);
        MockERC20(token1).approve(clPositionManager, type(uint256).max);

        int24 maxTick = (int24(887272) / tickSpacing) * tickSpacing;
        ICLPositionManager(clPositionManager).mint(
            ICLPositionManager.MintParams({
                token0: token0,
                token1: token1,
                tickSpacing: tickSpacing,
                tickLower: -maxTick,
                tickUpper: maxTick,
                amount0Desired: LIQUIDITY_AMOUNT,
                amount1Desired: LIQUIDITY_AMOUNT,
                amount0Min: 0,
                amount1Min: 0,
                recipient: msg.sender,
                deadline: block.timestamp + 1 hours,
                sqrtPriceX96: 0
            })
        );
        console.log("Aerodrome CL liquidity seeded");
    }
}
