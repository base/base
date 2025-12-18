package bootstrap

import (
	"context"
	"math"
	"testing"

	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	"github.com/ethereum-optimism/optimism/op-devstack/presets"
	"github.com/ethereum-optimism/optimism/op-devstack/sysgo"
	opspresets "github.com/succinctlabs/op-succinct/presets"
	"github.com/succinctlabs/op-succinct/utils"
)

func TestMain(m *testing.M) {
	presets.DoMain(m,
		opspresets.WithDefaultSuccinctFPProposer(&sysgo.DefaultSingleChainInteropSystemIDs{}),
		presets.WithSafeDBEnabled(),
	)
}

func TestFaultProofProposer_L2DgfDeployedAndUp(gt *testing.T) {
	t := devtest.SerialT(gt)
	sys := presets.NewMinimalWithProposer(t)
	require := t.Require()
	logger := t.Logger()

	dgfAddr := sys.L2Chain.Escape().Deployment().DisputeGameFactoryProxyAddr()
	logger.Info("Dispute Game Factory Address:", "address", dgfAddr.Hex())

	dgf, err := utils.NewDgfClient(sys.L1EL.EthClient(), dgfAddr)
	require.NoError(err, "failed to create DGF client")

	gameCount, err := dgf.GameCount(t.Ctx())
	require.NoError(err, "failed to get game count from DGF")
	logger.Info("Dispute Game Count:", "count", gameCount)
	require.Equal(uint64(0), gameCount, "expected zero dispute games initially")
}

func TestFaultProofProposer_DetectsFirstGameCreated(gt *testing.T) {
	t := devtest.SerialT(gt)
	sys := presets.NewMinimalWithProposer(t)
	require := t.Require()
	logger := t.Logger()
	ctx, cancel := context.WithTimeout(t.Ctx(), utils.ShortTimeout())
	defer cancel()

	dgfAddr := sys.L2Chain.Escape().Deployment().DisputeGameFactoryProxyAddr()
	logger.Info("Dispute Game Factory Address:", "address", dgfAddr.Hex())
	dgf, err := utils.NewDgfClient(sys.L1EL.EthClient(), dgfAddr)
	require.NoError(err, "failed to create Dispute Game Factory client")

	utils.WaitForGameCount(ctx, t, dgf, 1)

	// Get first game
	game, err := dgf.GameAtIndex(ctx, 0)
	require.NoError(err, "failed to get first game from factory")
	logger.Info("First game created", "gameType", game.GameType, "timestamp", game.Timestamp, "proxy", game.Proxy.Hex())
	fdg, err := utils.NewFdgClient(sys.L1EL.EthClient(), game.Proxy)
	require.NoError(err, "failed to create Fault Dispute Game client")

	// Verify parent index
	parentIdx, err := fdg.ParentIndex(ctx)
	require.NoError(err, "failed to read parentIndex")
	t.Logger().Info("Fault dispute game parent index", "parentIndex", parentIdx)
	require.Equal(uint32(math.MaxUint32), parentIdx, "unexpected parent index")

	// Verify root claim
	l2BlockNumber, err := fdg.L2BlockNumber(ctx)
	require.NoError(err, "failed to read L2 block number")
	logger.Info("Fault dispute game L2 block number", "l2BlockNumber", l2BlockNumber)
	rootClaim, err := fdg.RootClaim(ctx)
	require.NoError(err, "failed to read root claim")
	logger.Info("Fault dispute game root claim", "rootClaim", rootClaim)
	err = utils.VerifyOutputRoot(ctx, sys.L2EL.Escape().L2EthClient(), l2BlockNumber, rootClaim)
	require.NoError(err, "root claim verification failed")
}
