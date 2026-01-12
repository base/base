#!/usr/bin/env bash
# Deploy ERC-4337 EntryPoints and SimpleAccountFactories
# Uses the eth-infinitism hardhat-deploy scripts for deterministic CREATE2 deployment
#
# Usage:
#   ./deploy_contracts.sh [versions]
#
# Examples:
#   ./deploy_contracts.sh                  # Deploy all versions (0.6, 0.7, 0.8)
#   ./deploy_contracts.sh 0.6              # Deploy only v0.6
#   ./deploy_contracts.sh 0.6 0.7          # Deploy v0.6 and v0.7
#   VERSIONS="0.6,0.7" ./deploy_contracts.sh  # Using env var
#   ACCOUNT_INDEX=5 ./deploy_contracts.sh  # Use account #5 instead of #0
#
# Environment variables:
#   RPC_URL       - RPC endpoint (default: http://localhost:8547)
#   VERSIONS      - Comma-separated versions to deploy (default: 0.6,0.7,0.8)
#   ACCOUNT_INDEX - Which account from mnemonic to use (default: 0)
#                   Use a different index if account 0 is used by block builder

set -e

# Configuration
RPC_URL="${RPC_URL:-http://localhost:8547}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONTRACTS_DIR="$SCRIPT_DIR/contracts/solidity"

# Parse versions from args or env var
if [ $# -gt 0 ]; then
    VERSIONS="$*"
    VERSIONS="${VERSIONS// /,}"  # Replace spaces with commas
else
    VERSIONS="${VERSIONS:-0.6,0.7,0.8}"
fi

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m'

# Check requirements
if ! command -v node &> /dev/null; then
    echo -e "${RED}Error: 'node' is required but not installed${NC}"
    exit 1
fi

if ! command -v yarn &> /dev/null; then
    echo -e "${RED}Error: 'yarn' is required but not installed${NC}"
    echo "Install: npm install -g yarn"
    exit 1
fi

echo -e "${BLUE}════════════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}  ERC-4337 Contract Deployment Script${NC}"
echo -e "${BLUE}  (Using deterministic CREATE2 deployment)${NC}"
echo -e "${BLUE}════════════════════════════════════════════════════════════${NC}"
echo ""
echo -e "RPC URL:  ${YELLOW}$RPC_URL${NC}"
echo -e "Versions: ${YELLOW}$VERSIONS${NC}"
echo ""

# Function to deploy using hardhat with inline network config
deploy_version() {
    local version=$1
    local submodule_dir=$2
    
    echo -e "${BLUE}Deploying v$version...${NC}"
    
    cd "$submodule_dir"
    
    # Clean up any stale pending transactions from previous failed deployments
    # This prevents hardhat-deploy from trying to re-broadcast old transactions
    if [ -f "deployments/localreth/.pendingTransactions" ]; then
        echo -e "  Cleaning up stale pending transactions..."
        rm -f "deployments/localreth/.pendingTransactions"
    fi
    
    # Install dependencies if needed
    if [ ! -d "node_modules" ]; then
        echo -e "  Installing dependencies (this may take a minute)..."
        yarn install --ignore-engines
    fi
    
    # Create a temporary hardhat config that imports the original and adds our network
    # Use ACCOUNT_INDEX to derive a different account from the mnemonic (default: 0)
    local account_index="${ACCOUNT_INDEX:-0}"
    cat > hardhat.config.local.ts << CONFIGEOF
import baseConfig from './hardhat.config'
import { HardhatUserConfig } from 'hardhat/config'

const config: HardhatUserConfig = {
  ...baseConfig,
  networks: {
    ...baseConfig.networks,
    localreth: {
      url: process.env.RPC_URL || 'http://localhost:8547',
      accounts: {
        mnemonic: 'test test test test test test test test test test test junk',
        path: "m/44'/60'/0'/0",
        initialIndex: ${account_index},
        count: 1
      }
    }
  }
}

export default config
CONFIGEOF

    # Temporarily patch the SimpleAccountFactory deploy script to allow any chain ID
    local factory_deploy="deploy/2_deploy_SimpleAccountFactory.ts"
    local factory_backup="/tmp/2_deploy_SimpleAccountFactory.ts.bak.$$"
    if [ -f "$factory_deploy" ]; then
        cp "$factory_deploy" "$factory_backup"
        # Remove the chain ID check (replace the if block that returns early)
        sed -i.tmp 's/if (network.chainId !== 31337 && network.chainId !== 1337)/if (false)/' "$factory_deploy"
        rm -f "${factory_deploy}.tmp"
    fi

    # Run deploy with our network using the local config
    echo -e "  Running hardhat deploy..."
    RPC_URL="$RPC_URL" npx hardhat deploy --network localreth --config hardhat.config.local.ts 2>&1 | while read line; do
        echo "  $line"
    done
    local deploy_status=${PIPESTATUS[0]}
    
    # Clean up temporary config
    rm -f hardhat.config.local.ts
    
    # Restore original factory deploy script
    if [ -f "$factory_backup" ]; then
        mv "$factory_backup" "$factory_deploy"
    fi
    
    if [ $deploy_status -ne 0 ]; then
        echo -e "  ${RED}✗ v$version deployment failed${NC}"
        return 1
    fi
    
    echo -e "  ${GREEN}✓ v$version deployed${NC}"
    echo ""
}

# Convert versions string to array and deploy each
IFS=',' read -ra VERSION_ARRAY <<< "$VERSIONS"
TOTAL=${#VERSION_ARRAY[@]}
COUNT=0

for version in "${VERSION_ARRAY[@]}"; do
    # Trim whitespace
    version=$(echo "$version" | tr -d ' ')
    COUNT=$((COUNT + 1))
    
    case "$version" in
        0.6|v0.6)
            echo -e "${BLUE}[$COUNT/$TOTAL] Deploying v0.6 contracts...${NC}"
            deploy_version "0.6" "$CONTRACTS_DIR/v0_6/lib/account-abstraction"
            ;;
        0.7|v0.7)
            echo -e "${BLUE}[$COUNT/$TOTAL] Deploying v0.7 contracts...${NC}"
            deploy_version "0.7" "$CONTRACTS_DIR/v0_7/lib/account-abstraction"
            ;;
        0.8|v0.8)
            echo -e "${BLUE}[$COUNT/$TOTAL] Deploying v0.8 contracts...${NC}"
            deploy_version "0.8" "$CONTRACTS_DIR/v0_8/lib/account-abstraction"
            ;;
        *)
            echo -e "${RED}Unknown version: $version (valid: 0.6, 0.7, 0.8)${NC}"
            exit 1
            ;;
    esac
done

# Summary
echo -e "${BLUE}════════════════════════════════════════════════════════════${NC}"
echo -e "${GREEN}  Deployment Complete!${NC}"
echo -e "${BLUE}════════════════════════════════════════════════════════════${NC}"
echo ""
echo -e "${YELLOW}Expected canonical addresses:${NC}"
echo -e "  EntryPoint v0.6:     0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789"
echo -e "  EntryPoint v0.7:     0x0000000071727De22E5E9d8BAf0edAc6f37da032"
echo -e "  EntryPoint v0.8:     0x4337084D9e255Ff0702461CF8895CE9E3B5Ff108"
echo ""
echo -e "${YELLOW}Note:${NC} Check deployment output above for actual addresses."
echo -e "Deployments are saved to each submodule's deployments/ folder."
