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
	cfg := opspresets.DefaultValidityConfig()
	waitForOutputAndVerify(gt, 1, 10*time.Minute, cfg)
}

func TestValidityProposer_ThreeSubmissions(gt *testing.T) {
	cfg := opspresets.DefaultValidityConfig()
	waitForOutputAndVerify(gt, 3, 30*time.Minute, cfg)
}

func TestValidityProposer_ProofIntervalOne(gt *testing.T) {
	cfg := opspresets.ValidityConfig{
		StartingBlock:      1,
		SubmissionInterval: 5, // Keep low since more range proofs to generate takes longer
		RangeProofInterval: 1,
	}
	waitForOutputAndVerify(gt, 1, 20*time.Minute, cfg)
}

func TestValidityProposer_ProofIntervalNotDivisible(gt *testing.T) {
	cfg := opspresets.ValidityConfig{
		StartingBlock:      1,
		SubmissionInterval: 10,
		RangeProofInterval: 7,
	}
	waitForOutputAndVerify(gt, 1, 10*time.Minute, cfg)
}

func TestValidityProposer_RangeIntervalLargerThanSubmission(gt *testing.T) {
	cfg := opspresets.ValidityConfig{
		StartingBlock:      1,
		SubmissionInterval: 5,
		RangeProofInterval: 10, // Larger than submission interval
	}
	waitForOutputAndVerify(gt, 1, 10*time.Minute, cfg)
}

func waitForOutputAndVerify(gt *testing.T, submissionCount int, timeout time.Duration, cfg opspresets.ValidityConfig) {
	t := devtest.ParallelT(gt)
	sys := opspresets.NewValiditySystem(t, cfg)
	require := t.Require()
	logger := t.Logger()
	ctx, cancel := context.WithTimeout(t.Ctx(), timeout)
	defer cancel()

	l2oo := sys.L2OOClient(t)
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

	expectedCount := cfg.ExpectedRangeCount(outputProposal.L2BlockNumber)
	utils.VerifyRangeProofsWithExpected(ctx, t, sys.DatabaseURL(), cfg.StartingBlock, outputProposal.L2BlockNumber, expectedCount)
}
