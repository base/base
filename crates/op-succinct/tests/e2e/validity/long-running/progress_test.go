package longrunning

import (
	"fmt"
	"testing"

	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	"github.com/ethereum-optimism/optimism/op-service/eth"
	opspresets "github.com/succinctlabs/op-succinct/presets"
	"github.com/succinctlabs/op-succinct/utils"
)

// TestValidityProposer_Progress verifies the proposer keeps up with L2 finalization
// and produces correct output roots. Fails if the lag between finalized L2 blocks and
// the L2OO's latest submission exceeds the allowed threshold, or if any output root is incorrect.
func TestValidityProposer_Progress(gt *testing.T) {
	t := devtest.ParallelT(gt)
	cfg := opspresets.LongRunningValidityConfig()
	sys, l2oo := setupValiditySystem(t, cfg, opspresets.LongRunningL2ChainConfig())

	err := utils.RunProgressTest(func() error {
		return checkLatestSubmission(t, sys, l2oo)
	})
	t.Require().NoError(err, "proposer progress check failed")
}

func setupValiditySystem(t devtest.T, cfg opspresets.ValidityConfig, chain opspresets.L2ChainConfig) (*opspresets.ValiditySystem, *utils.L2OOClient) {
	sys := opspresets.NewValiditySystem(t, cfg, chain)
	t.Log("=== Stack is running ===")
	return sys, sys.L2OOClient(t)
}

// checkLatestSubmission verifies the latest L2OO submission's lag and output root correctness.
func checkLatestSubmission(t devtest.T, sys *opspresets.ValiditySystem, l2oo *utils.L2OOClient) error {
	ctx := t.Ctx()
	l2Finalized := sys.L2EL.BlockRefByLabel(eth.Finalized)

	l2ooBlock, err := l2oo.LatestBlockNumber(ctx)
	if err != nil {
		return err
	}
	if l2ooBlock == 0 {
		t.Logf("L2 Finalized: %d | L2OO: no submissions yet | waiting...", l2Finalized.Number)
		return nil
	}

	// Check proposer lag
	var lag uint64
	if l2Finalized.Number > l2ooBlock {
		lag = l2Finalized.Number - l2ooBlock
	}
	maxLag := utils.MaxProposerLag()
	t.Logf("L2 Finalized: %d | L2OO Latest Block: %d | Lag: %d (max: %d)", l2Finalized.Number, l2ooBlock, lag, maxLag)
	if lag > maxLag {
		return fmt.Errorf("lag %d exceeds max %d", lag, maxLag)
	}

	// Check output root correctness
	proposal, err := l2oo.GetL2OutputAfter(ctx, l2ooBlock)
	if err != nil {
		return fmt.Errorf("get output proposal: %w", err)
	}
	return utils.VerifyOutputRoot(ctx, sys.L2EL.Escape().L2EthClient(), proposal.L2BlockNumber, proposal.OutputRoot)
}
