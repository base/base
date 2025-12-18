package longrunning

import (
	"testing"
	"time"

	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	opspresets "github.com/succinctlabs/op-succinct/presets"
	"github.com/succinctlabs/op-succinct/utils"
)

// TestFaultProofProposer_LongRunning runs until shutdown, logging progress.
func TestFaultProofProposer_LongRunning(gt *testing.T) {
	t := devtest.SerialT(gt)
	cfg := opspresets.LongRunningFaultProofConfig()
	cfg.EnvFilePath = "../../../.env.faultproof"
	sys, dgf := setupFaultProofSystem(t, cfg, opspresets.LongRunningL2ChainConfig())

	utils.RunUntilShutdown(60*time.Second, func() error {
		checkLatestGame(t, sys, dgf, nil)
		return nil
	})
}

// TestFaultProofProposer_FastFinality_LongRunning runs until shutdown, logging progress.
func TestFaultProofProposer_FastFinality_LongRunning(gt *testing.T) {
	t := devtest.SerialT(gt)
	cfg := opspresets.LongRunningFastFinalityFaultProofConfig()
	cfg.EnvFilePath = "../../../.env.faultproof"
	sys, dgf := setupFaultProofSystem(t, cfg, opspresets.LongRunningL2ChainConfig())

	utils.RunUntilShutdown(60*time.Second, func() error {
		checkLatestGame(t, sys, dgf, &cfg)
		return nil
	})
}
