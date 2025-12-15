package recovery

import (
	"context"
	"testing"
	"time"

	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	"github.com/ethereum-optimism/optimism/op-service/eth"
	opspresets "github.com/succinctlabs/op-succinct/presets"
	"github.com/succinctlabs/op-succinct/utils"
)

const (
	// proposerStopWait is the time to wait after stopping the proposer before restarting.
	// Must exceed LoopInterval (default 1s) to ensure the database lock expires,
	// allowing the restarted proposer to acquire it.
	proposerStopWait = 2 * time.Second
)

func TestValidityProposer_RestartRecovery_Basic(gt *testing.T) {
	cfg := opspresets.DefaultValidityConfig()
	runRecoveryTest(gt, cfg, 1, 1, 20*time.Minute)
}

func TestValidityProposer_RestartRecovery_ThreeSubmissions(gt *testing.T) {
	cfg := opspresets.DefaultValidityConfig()
	runRecoveryTest(gt, cfg, 1, 3, 20*time.Minute)
}

func TestValidityProposer_RestartRecovery_RangeSplit(gt *testing.T) {
	cfg := opspresets.DefaultValidityConfig()
	cfg.SubmissionInterval = 20
	cfg.RangeProofInterval = 5
	runRecoveryTest(gt, cfg, 1, 1, 20*time.Minute)
}

func TestValidityProposer_RestartRecovery_MultipleRestarts(gt *testing.T) {
	cfg := opspresets.DefaultValidityConfig()
	runRecoveryTest(gt, cfg, 3, 1, 20*time.Minute)
}

func runRecoveryTest(gt *testing.T, cfg opspresets.ValidityConfig, restartCount, expectedSubmissions int, timeout time.Duration) {
	t := devtest.ParallelT(gt)
	sys := opspresets.NewValiditySystem(t, cfg)
	ctx, cancel := context.WithTimeout(t.Ctx(), timeout)
	defer cancel()

	t.Logger().Info("Running validity recovery test",
		"restartCount", restartCount,
		"expectedSubmissions", expectedSubmissions)

	performRestartCycles(ctx, t, sys, restartCount)

	l2oo := sys.L2OOClient(t)
	expectedBlock := cfg.ExpectedOutputBlock(expectedSubmissions)
	verifySubmission(ctx, t, sys, l2oo, expectedBlock)
	verifyRangeProofs(ctx, t, sys, l2oo, &cfg)

	t.Logger().Info("Recovery test completed successfully")
}

// performRestartCycles stops and restarts the proposer multiple times,
// verifying data persistence after each cycle.
func performRestartCycles(ctx context.Context, t devtest.T, sys *opspresets.ValiditySystem, count int) {
	logger := t.Logger()
	require := t.Require()
	for i := 1; i <= count; i++ {
		utils.WaitForRangeProofProgress(ctx, t, sys.DatabaseURL(), i)

		countBefore, err := utils.CountRangeProofRequests(ctx, sys.DatabaseURL())
		require.NoError(err, "failed to count range proofs before stop")
		logger.Info("Stopping proposer", "restart", i, "rangeProofRequests", countBefore)

		sys.StopProposer()
		time.Sleep(proposerStopWait)

		countAfterStop, err := utils.CountRangeProofRequests(ctx, sys.DatabaseURL())
		require.NoError(err, "failed to count range proofs after stop")
		require.Equal(countBefore, countAfterStop, "data should persist after stop")

		sys.StartProposer()
		logger.Info("Proposer restarted", "restart", i)
	}
}

// verifySubmission waits for the expected block and verifies the output root.
func verifySubmission(ctx context.Context, t devtest.T, sys *opspresets.ValiditySystem, l2oo *utils.L2OOClient, expectedBlock uint64) {
	logger := t.Logger()
	require := t.Require()

	logger.Info("Waiting for output", "expectedBlock", expectedBlock)
	utils.WaitForLatestBlockNumber(ctx, t, l2oo, expectedBlock)

	outputProposal, err := l2oo.GetL2OutputAfter(ctx, expectedBlock)
	require.NoError(err, "failed to get output proposal")
	require.Equal(expectedBlock, outputProposal.L2BlockNumber, "L2 block number mismatch")

	expectedOutput, err := sys.L2EL.Escape().L2EthClient().OutputV0AtBlockNumber(ctx, outputProposal.L2BlockNumber)
	require.NoError(err, "failed to get expected output")
	require.Equal(eth.OutputRoot(expectedOutput), outputProposal.OutputRoot, "output root mismatch")

	logger.Info("Output verified", "block", outputProposal.L2BlockNumber)
}

// verifyRangeProofs verifies range proofs in the database match the L2OO state.
func verifyRangeProofs(ctx context.Context, t devtest.T, sys *opspresets.ValiditySystem, l2oo *utils.L2OOClient, cfg *opspresets.ValidityConfig) {
	latestBlock, err := l2oo.LatestBlockNumber(ctx)
	t.Require().NoError(err, "failed to get latest block number")

	expectedCount := cfg.ExpectedRangeCount(latestBlock)
	utils.VerifyRangeProofsWithExpected(ctx, t, sys.DatabaseURL(), cfg.StartingBlock, latestBlock, expectedCount)
}
