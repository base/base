#!/usr/bin/env bash
# Setup script for bundler-spec-tests
# Clones the test repositories for v0.6 and v0.7 and installs dependencies

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPOS_DIR="$SCRIPT_DIR/repos"
LOGS_DIR="$SCRIPT_DIR/logs"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${BLUE}═══════════════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}  Bundler Spec Tests Setup${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════════════${NC}"
echo ""

# Check prerequisites
check_prereqs() {
    local missing=()
    
    if ! command -v python3 &> /dev/null; then
        missing+=("python3")
    fi
    
    if ! command -v pdm &> /dev/null; then
        missing+=("pdm (install with: pip install pdm)")
    fi
    
    if ! command -v git &> /dev/null; then
        missing+=("git")
    fi
    
    if [ ${#missing[@]} -ne 0 ]; then
        echo -e "${RED}Missing prerequisites:${NC}"
        for prereq in "${missing[@]}"; do
            echo -e "  - $prereq"
        done
        exit 1
    fi
    
    echo -e "${GREEN}✓ All prerequisites installed${NC}"
}

# Clone or update a specific version
setup_version() {
    local version=$1
    local branch=$2
    local repo_dir="$REPOS_DIR/$version"
    
    echo ""
    echo -e "${BLUE}Setting up $version (branch: $branch)...${NC}"
    
    if [ -d "$repo_dir" ]; then
        echo -e "  ${YELLOW}Repository exists, updating...${NC}"
        cd "$repo_dir"
        git fetch origin
        git checkout "$branch"
        git pull origin "$branch"
        git submodule update --init --recursive
    else
        echo -e "  Cloning bundler-spec-tests..."
        git clone --branch "$branch" --recursive \
            https://github.com/eth-infinitism/bundler-spec-tests.git \
            "$repo_dir"
        cd "$repo_dir"
    fi
    
    echo -e "  Installing Python dependencies..."
    pdm install
    pdm run update-deps 2>/dev/null || true
    
    echo -e "  ${GREEN}✓ $version setup complete${NC}"
}

# Main
main() {
    check_prereqs
    
    # Create directories
    mkdir -p "$REPOS_DIR"
    mkdir -p "$LOGS_DIR"
    
    # Setup v0.6
    setup_version "v0.6" "releases/v0.6"
    
    # Setup v0.7
    setup_version "v0.7" "releases/v0.7"
    
    echo ""
    echo -e "${BLUE}═══════════════════════════════════════════════════════════════${NC}"
    echo -e "${GREEN}  Setup Complete!${NC}"
    echo -e "${BLUE}═══════════════════════════════════════════════════════════════${NC}"
    echo ""
    echo -e "Repositories cloned to:"
    echo -e "  - ${YELLOW}$REPOS_DIR/v0.6${NC}"
    echo -e "  - ${YELLOW}$REPOS_DIR/v0.7${NC}"
    echo ""
    echo -e "Next steps:"
    echo -e "  1. Build the node: ${YELLOW}cargo build --release${NC}"
    echo -e "  2. Run tests:      ${YELLOW}./e2e/bundler-spec-tests/run-tests.sh v0.6${NC}"
    echo -e "                     ${YELLOW}./e2e/bundler-spec-tests/run-tests.sh v0.7${NC}"
}

main "$@"
