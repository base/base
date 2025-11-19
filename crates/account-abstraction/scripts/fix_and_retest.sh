#!/bin/bash
# Fix issues and retest
set -e

RPC_URL="http://localhost:8545"
WALLET="0x8568d5ce3da8ebeba9380bdebf0494d9a9847e94"
ENTRYPOINT="0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789"
DEPLOYER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"

echo "═══════════════════════════════════════════════════════"
echo "    Fixing Issues and Retesting"
echo "═══════════════════════════════════════════════════════"
echo ""

echo "[1/2] Depositing ETH into EntryPoint for wallet..."
echo ""

# The wallet needs to deposit into EntryPoint
# We'll send a transaction directly calling EntryPoint.depositTo(wallet)
DEPOSIT_AMOUNT="0.5" # 0.5 ETH

# Encode depositTo(address) call
DEPOSIT_CALLDATA=$(cast calldata "depositTo(address)" $WALLET)

echo "Depositing $DEPOSIT_AMOUNT ETH to EntryPoint for wallet $WALLET..."
cast send $ENTRYPOINT $DEPOSIT_CALLDATA \
  --value ${DEPOSIT_AMOUNT}ether \
  --private-key $DEPLOYER_KEY \
  --rpc-url $RPC_URL

echo ""
echo "✅ Deposited!"
echo ""

# Check balance
BALANCE=$(cast call $ENTRYPOINT "balanceOf(address)(uint256)" $WALLET --rpc-url $RPC_URL)
echo "Wallet balance in EntryPoint: $(cast to-unit $BALANCE ether) ETH"
echo ""

echo "[2/2] Resubmitting UserOperation..."
echo ""

cd /Users/ericliu/Projects/node-reth/crates/account-abstraction

cargo run --quiet --example submit_bundle -- \
  --bundler-key $DEPLOYER_KEY \
  --owner-key $DEPLOYER_KEY \
  --sender $WALLET \
  --entry-point $ENTRYPOINT \
  --chain-id 31337 \
  --rpc-url $RPC_URL \
  --nonce 0

echo ""
echo "═══════════════════════════════════════════════════════"
echo "✅ Done! Check if transaction succeeded (status = 1)"
echo "═══════════════════════════════════════════════════════"
