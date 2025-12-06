package bootstrap

import (
	"testing"

	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	"github.com/ethereum-optimism/optimism/op-devstack/presets"
	"github.com/ethereum-optimism/optimism/op-devstack/sysgo"
	opspresets "github.com/succinctlabs/op-succinct/presets"
	"github.com/succinctlabs/op-succinct/utils"
)

func TestMain(m *testing.M) {
	presets.DoMain(m,
		opspresets.WithDefaultSuccinctValidityProposer(&sysgo.DefaultSingleChainInteropSystemIDs{}),
		presets.WithSafeDBEnabled(),
	)
}

func TestValidityProposer_L2OODeployedAndUp(gt *testing.T) {
	t := devtest.SerialT(gt)
	sys := presets.NewMinimalWithProposer(t)
	require := t.Require()

	l2ooAddr := sys.L2Chain.Escape().Deployment().OPSuccinctL2OutputOracleAddr()
	t.Logger().Info("L2 Output Oracle Address", "address", l2ooAddr.Hex())

	l2oo, err := utils.NewL2OOClient(sys.L1EL.EthClient(), l2ooAddr)
	require.NoError(err, "failed to create L2OO client")

	latestBlockNumber, err := l2oo.LatestBlockNumber(t.Ctx())
	require.NoError(err, "failed to get latest block number from L2OO")
	t.Logger().Info("Latest L2 block number from L2OO", "block", latestBlockNumber)
	require.Equal(uint64(1), latestBlockNumber, "expected latest L2 block number to be 1")
}
