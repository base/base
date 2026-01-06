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
	proposerCfg := opspresets.LongRunningFPProposerConfig()
	proposerCfg.EnvFilePath = ".env.proposer"
	challengerCfg := opspresets.DefaultFPChallengerConfig()
	challengerCfg.EnvFilePath = ".env.challenger"
	sys, dgf := setupFaultProofSystem(t, proposerCfg, opspresets.LongRunningL2ChainConfig(), opspresets.WithChallenger(challengerCfg))

	utils.RunUntilShutdown(60*time.Second, func() error {
		checkLatestGame(t, sys, dgf, nil)
		return nil
	})
}

// TestFaultProofProposer_FastFinality_LongRunning runs until shutdown, logging progress.
func TestFaultProofProposer_FastFinality_LongRunning(gt *testing.T) {
	t := devtest.SerialT(gt)
	proposerCfg := opspresets.LongRunningFastFinalityFPProposerConfig()
	proposerCfg.EnvFilePath = ".env.proposer"
	challengerCfg := opspresets.DefaultFPChallengerConfig()
	challengerCfg.EnvFilePath = ".env.challenger"
	sys, dgf := setupFaultProofSystem(t, proposerCfg, opspresets.LongRunningL2ChainConfig(), opspresets.WithChallenger(challengerCfg))

	utils.RunUntilShutdown(60*time.Second, func() error {
		checkLatestGame(t, sys, dgf, &proposerCfg)
		return nil
	})
}
