package utils

import (
	"fmt"
	"os"
	"path/filepath"
	"runtime"
)

// UseNetworkProver returns true if network proving is enabled (NETWORK_PRIVATE_KEY is set).
func UseNetworkProver() bool {
	return os.Getenv("NETWORK_PRIVATE_KEY") != ""
}

// RepoRoot returns the op-succinct repo root.
// Assumes this file is at tests/utils/artifacts.go
func RepoRoot() string {
	_, file, _, ok := runtime.Caller(0)
	if !ok {
		return ""
	}
	return filepath.Dir(filepath.Dir(filepath.Dir(file)))
}

// SymlinkSuccinctArtifacts creates symlinks so op-deployer can find OP Succinct artifacts.
func SymlinkSuccinctArtifacts(repoRoot string) error {
	artifactsDir := filepath.Join(repoRoot, "tests", "artifacts", "src", "forge-artifacts")
	contractsOut := filepath.Join(repoRoot, "contracts", "out")

	if _, err := os.Stat(contractsOut); os.IsNotExist(err) {
		return fmt.Errorf("contracts/out directory does not exist: %s (run 'forge build' first)", contractsOut)
	}

	if err := os.MkdirAll(artifactsDir, 0o755); err != nil {
		return fmt.Errorf("failed to create artifacts directory %s: %w", artifactsDir, err)
	}

	for _, name := range []string{
		// Deploy scripts
		"DeployMockVerifier.s.sol",
		"DeployVerifier.s.sol",
		// SP1 verifier contracts and dependencies
		"SP1MockVerifier.sol",
		"SP1VerifierPlonk.sol",
		"SP1VerifierGroth16.sol",
		"ISP1Verifier.sol",
		"PlonkVerifier.sol",
		"Groth16Verifier.sol",
	} {
		target := filepath.Join(artifactsDir, name)
		if _, err := os.Lstat(target); err == nil {
			continue
		}
		source, err := filepath.Rel(artifactsDir, filepath.Join(contractsOut, name))
		if err != nil {
			return fmt.Errorf("failed to compute relative path for %s: %w", name, err)
		}
		if err := os.Symlink(source, target); err != nil && !os.IsExist(err) {
			return fmt.Errorf("failed to create symlink for %s: %w", name, err)
		}
	}
	return nil
}
