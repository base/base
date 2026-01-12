#!/usr/bin/env bash
# Main test runner for bundler-spec-tests
#
# Usage:
#   ./run-tests.sh <version> [pytest-args...]
#
# Examples:
#   ./run-tests.sh v0.6                    # Run all v0.6 tests
#   ./run-tests.sh v0.7                    # Run all v0.7 tests
#   ./run-tests.sh v0.6 -k "sendUserOp"    # Run specific tests
#   ./run-tests.sh v0.7 -v                 # Verbose output
#   ./run-tests.sh all                     # Run both v0.6 and v0.7

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../../../.." && pwd)"
REPOS_DIR="$SCRIPT_DIR/repos"
LOG_DIR="$SCRIPT_DIR/logs"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m'

# EntryPoint addresses
ENTRYPOINT_V06="0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789"
ENTRYPOINT_V07="0x0000000071727De22E5E9d8BAf0edAc6f37da032"

usage() {
    echo "Usage: $0 <version> [pytest-args...]"
    echo ""
    echo "Versions:"
    echo "  v0.6, 0.6    - Run v0.6 spec tests"
    echo "  v0.7, 0.7    - Run v0.7 spec tests"
    echo "  all          - Run both v0.6 and v0.7 tests"
    echo ""
    echo "Examples:"
    echo "  $0 v0.6                    # Run all v0.6 tests"
    echo "  $0 v0.7 -k 'sendUserOp'    # Run specific tests"
    echo "  $0 all -v                  # Run all tests verbose"
    echo ""
    echo "Environment variables:"
    echo "  NODE_PORT    - RPC port (default: 8545)"
    echo "  SKIP_BUILD   - Skip building the node if set"
    exit 1
}

check_setup() {
    local version="$1"
    local repo_dir="$REPOS_DIR/$version"
    
    if [ ! -d "$repo_dir" ]; then
        echo -e "${RED}Error: bundler-spec-tests not set up for $version${NC}"
        echo -e "Run: ${YELLOW}./e2e/bundler-spec-tests/setup.sh${NC}"
        exit 1
    fi
}

run_version_tests() {
    local version="$1"
    shift
    local pytest_args=("$@")
    
    local repo_dir="$REPOS_DIR/$version"
    local entrypoint
    local node_port="${NODE_PORT:-8545}"
    
    case "$version" in
        v0.6|0.6)
            version="v0.6"
            entrypoint="$ENTRYPOINT_V06"
            ;;
        v0.7|0.7)
            version="v0.7"
            entrypoint="$ENTRYPOINT_V07"
            ;;
    esac
    
    check_setup "$version"
    
    echo ""
    echo -e "${BLUE}═══════════════════════════════════════════════════════════════${NC}"
    echo -e "${BLUE}  Running Bundler Spec Tests - $version${NC}"
    echo -e "${BLUE}═══════════════════════════════════════════════════════════════${NC}"
    echo ""
    echo -e "EntryPoint: ${YELLOW}$entrypoint${NC}"
    echo -e "Node URL:   ${YELLOW}http://localhost:$node_port${NC}"
    echo -e "Test repo:  ${YELLOW}$repo_dir${NC}"
    echo ""
    
    mkdir -p "$LOG_DIR"
    
    # Export for launcher script
    export ENTRYPOINT_VERSION="$version"
    export NODE_PORT="$node_port"
    export LOG_DIR="$LOG_DIR"
    
    cd "$repo_dir"
    
    # Run tests with our launcher script
    # The bundler-spec-tests framework expects:
    #   --url: bundler RPC URL
    #   --entry-point: EntryPoint contract address  
    #   --ethereum-node: Ethereum node RPC URL
    #   --launcher-script: Script to start/stop the node
    pdm test \
        --url "http://localhost:$node_port" \
        --entry-point "$entrypoint" \
        --ethereum-node "http://localhost:$node_port" \
        --launcher-script "$SCRIPT_DIR/launcher.sh" \
        "${pytest_args[@]}"
    
    local exit_code=$?
    
    if [ $exit_code -eq 0 ]; then
        echo -e "${GREEN}✓ $version tests passed${NC}"
    else
        echo -e "${RED}✗ $version tests failed (exit code: $exit_code)${NC}"
    fi
    
    return $exit_code
}

main() {
    if [ $# -lt 1 ]; then
        usage
    fi
    
    local version="$1"
    shift
    local pytest_args=("$@")
    
    # Build node first (unless SKIP_BUILD is set)
    if [ -z "${SKIP_BUILD:-}" ]; then
        echo -e "${BLUE}Building base-reth-node...${NC}"
        cd "$ROOT_DIR"
        cargo build --release -p base-reth-node
    fi
    
    local exit_code=0
    
    case "$version" in
        v0.6|0.6)
            run_version_tests "v0.6" "${pytest_args[@]}" || exit_code=$?
            ;;
        v0.7|0.7)
            run_version_tests "v0.7" "${pytest_args[@]}" || exit_code=$?
            ;;
        all)
            echo -e "${BLUE}Running tests for all versions...${NC}"
            
            run_version_tests "v0.6" "${pytest_args[@]}" || exit_code=$?
            
            # Continue with v0.7 even if v0.6 fails
            run_version_tests "v0.7" "${pytest_args[@]}" || exit_code=$?
            ;;
        *)
            echo -e "${RED}Unknown version: $version${NC}"
            usage
            ;;
    esac
    
    echo ""
    if [ $exit_code -eq 0 ]; then
        echo -e "${GREEN}═══════════════════════════════════════════════════════════════${NC}"
        echo -e "${GREEN}  All tests passed!${NC}"
        echo -e "${GREEN}═══════════════════════════════════════════════════════════════${NC}"
    else
        echo -e "${RED}═══════════════════════════════════════════════════════════════${NC}"
        echo -e "${RED}  Some tests failed (exit code: $exit_code)${NC}"
        echo -e "${RED}═══════════════════════════════════════════════════════════════${NC}"
    fi
    
    exit $exit_code
}

main "$@"
