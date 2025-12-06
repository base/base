// These tests intentionally omit TestMain because each test creates its own
// isolated system via NewValiditySystem() with per-test configuration.
package proving

import (
	"context"
	"testing"
	"time"

	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	"github.com/ethereum-optimism/optimism/op-service/eth"
	opspresets "github.com/succinctlabs/op-succinct/presets"
	"github.com/succinctlabs/op-succinct/utils"
)

func TestValidityProposer_SingleSubmission(gt *testing.T) {
	t := devtest.ParallelT(gt)
	cfg := opspresets.DefaultValidityConfig()
	waitForOutputAndVerify(t, 1, 10*time.Minute, cfg)
}

func TestValidityProposer_ThreeSubmissions(gt *testing.T) {
	t := devtest.ParallelT(gt)
	cfg := opspresets.DefaultValidityConfig()
	waitForOutputAndVerify(t, 3, 30*time.Minute, cfg)
}

func TestValidityProposer_ProofIntervalOne(gt *testing.T) {
	t := devtest.ParallelT(gt)
	cfg := opspresets.ValidityConfig{
		StartingBlock:      1,
		SubmissionInterval: 5, // Keep low since more range proofs to generate takes longer
		RangeProofInterval: 1,
	}
	waitForOutputAndVerify(t, 1, 20*time.Minute, cfg)
}

func TestValidityProposer_ProofIntervalNotDivisible(gt *testing.T) {
	t := devtest.ParallelT(gt)
	cfg := opspresets.ValidityConfig{
		StartingBlock:      1,
		SubmissionInterval: 10,
		RangeProofInterval: 7,
	}
	waitForOutputAndVerify(t, 1, 10*time.Minute, cfg)
}

func TestValidityProposer_RangeIntervalLargerThanSubmission(gt *testing.T) {
	t := devtest.ParallelT(gt)
	cfg := opspresets.ValidityConfig{
		StartingBlock:      1,
		SubmissionInterval: 5,
		RangeProofInterval: 10, // Larger than submission interval
	}
	waitForOutputAndVerify(t, 1, 10*time.Minute, cfg)
}

func waitForOutputAndVerify(t devtest.T, submissionCount int, timeout time.Duration, cfg opspresets.ValidityConfig) {
	sys := opspresets.NewValiditySystem(t, cfg)
	require := t.Require()
	logger := t.Logger()
	ctx, cancel := context.WithTimeout(t.Ctx(), timeout)
	defer cancel()

	l2ooAddr := sys.L2Chain.Escape().Deployment().OPSuccinctL2OutputOracleAddr()
	l2oo, err := utils.NewL2OOClient(sys.L1EL.EthClient(), l2ooAddr)
	require.NoError(err, "failed to create L2OO client")

	expectedOutputBlock := cfg.ExpectedOutputBlock(submissionCount)
	logger.Info("Waiting for output", "expectedBlock", expectedOutputBlock, "submissions", submissionCount)

	utils.WaitForLatestBlockNumber(ctx, t, l2oo, expectedOutputBlock)

	outputProposal, err := l2oo.GetL2OutputAfter(ctx, expectedOutputBlock)
	require.NoError(err, "failed to get output proposal from L2OO")

	// Verify L2 block number matches expected
	require.Equal(expectedOutputBlock, outputProposal.L2BlockNumber, "L2 block number mismatch")

	// Verify output root matches expected L2 state
	expectedOutput, err := sys.L2EL.Escape().L2EthClient().OutputV0AtBlockNumber(ctx, outputProposal.L2BlockNumber)
	require.NoError(err, "failed to get expected output from L2")
	require.Equal(eth.OutputRoot(expectedOutput), outputProposal.OutputRoot, "output root mismatch")

	logger.Info("Output verified", "block", outputProposal.L2BlockNumber)

	verifyRangeProofs(ctx, t, sys, cfg, outputProposal.L2BlockNumber)
}

func verifyRangeProofs(ctx context.Context, t devtest.T, sys *opspresets.ValiditySystem, cfg opspresets.ValidityConfig, outputBlock uint64) {
	require := t.Require()
	logger := t.Logger()

	ranges, err := utils.FetchRangeProofs(ctx, sys.DatabaseURL(), cfg.StartingBlock, outputBlock)
	require.NoError(err, "failed to fetch range proofs")

	for i, r := range ranges {
		logger.Info("Range proof", "index", i, "start", r.StartBlock, "end", r.EndBlock)
	}

	expectedCount := cfg.ExpectedRangeCount(outputBlock)
	err = utils.VerifyRanges(ranges, int64(cfg.StartingBlock), int64(outputBlock), expectedCount)
	require.NoError(err, "range verification failed")

	logger.Info("Range proofs verified", "count", len(ranges), "expected", expectedCount)
}
