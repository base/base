package recovery

import (
	"context"
	"testing"
	"time"

	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	opspresets "github.com/succinctlabs/op-succinct/presets"
	"github.com/succinctlabs/op-succinct/utils"
)

func TestFaultProofProposer_RestartRecovery_Basic(gt *testing.T) {
	cfg := opspresets.DefaultFaultProofConfig()
	cfg.ProposalIntervalInBlocks = 40
	runRecoveryTest(gt, cfg, 1, 20*time.Minute)
}

func TestFaultProofProposer_RestartRecovery_FastFinalityBasic(gt *testing.T) {
	cfg := opspresets.FastFinalityFaultProofConfig()
	cfg.ProposalIntervalInBlocks = 40
	runRecoveryTest(gt, cfg, 1, 20*time.Minute)
}

func TestFaultProofProposer_RestartRecovery_FastFinalityRangeSplit(gt *testing.T) {
	cfg := opspresets.FastFinalityFaultProofConfig()
	cfg.ProposalIntervalInBlocks = 40
	cfg.RangeSplitCount = 4
	cfg.MaxConcurrentRangeProofs = 4
	runRecoveryTest(gt, cfg, 1, 20*time.Minute)
}

func TestFaultProofProposer_RestartRecovery_MultipleRestarts(gt *testing.T) {
	cfg := opspresets.FastFinalityFaultProofConfig()
	cfg.ProposalIntervalInBlocks = 40
	runRecoveryTest(gt, cfg, 3, 25*time.Minute)
}

func runRecoveryTest(gt *testing.T, cfg opspresets.FaultProofConfig, restartCount int, timeout time.Duration) {
	t := devtest.ParallelT(gt)
	sys := opspresets.NewFaultProofSystem(t, cfg)
	ctx, cancel := context.WithTimeout(t.Ctx(), timeout)
	defer cancel()

	t.Logger().Info("Running faultproof recovery test",
		"fastFinalityMode", cfg.FastFinalityMode,
		"restartCount", restartCount)

	dgf := sys.DgfClient(t)
	performRestartCycles(ctx, t, sys, dgf, restartCount)
	verifyGameResolution(ctx, t, sys, dgf)

	t.Logger().Info("Recovery test completed successfully")
}

// performRestartCycles stops and restarts the proposer multiple times,
// waiting for game creation progress between each cycle.
func performRestartCycles(ctx context.Context, t devtest.T, sys *opspresets.FaultProofSystem, dgf *utils.DgfClient, count int) {
	logger := t.Logger()
	require := t.Require()

	for i := 1; i <= count; i++ {
		// Wait for at least i games to be created
		utils.WaitForGameCount(ctx, t, dgf, uint64(i))

		gameCount, err := dgf.GameCount(ctx)
		require.NoError(err, "failed to get game count before stop")
		logger.Info("Stopping proposer", "restart", i, "gameCount", gameCount)

		sys.StopProposer()
		sys.StartProposer()
		logger.Info("Proposer restarted", "restart", i)
	}
}

// verifyGameResolution waits for the first game to resolve with DefenderWins.
func verifyGameResolution(ctx context.Context, t devtest.T, sys *opspresets.FaultProofSystem, dgf *utils.DgfClient) {
	logger := t.Logger()
	require := t.Require()

	game, err := dgf.GameAtIndex(ctx, 0)
	require.NoError(err, "failed to get game from factory")

	fdg, err := utils.NewFdgClient(sys.L1EL.EthClient(), game.Proxy)
	require.NoError(err, "failed to create FDG client")

	logger.Info("Waiting for game resolution...")
	utils.WaitForDefenderWins(ctx, t, fdg)
	logger.Info("Game resolved - DefenderWins")
}
