package presets

import (
	"github.com/ethereum-optimism/optimism/op-devstack/stack"
	"github.com/ethereum-optimism/optimism/op-devstack/sysgo"
	"github.com/ethereum-optimism/optimism/op-service/eth"
)

func WithSuccinctValidityProposer(dest *sysgo.DefaultSingleChainInteropSystemIDs) stack.CommonOption {
	return withSuccinctPreset(dest, func(opt *stack.CombinedOption[*sysgo.Orchestrator], ids sysgo.DefaultSingleChainInteropSystemIDs, l2ChainID eth.ChainID) {
		opt.Add(sysgo.WithSuperDeploySP1MockVerifier(ids.L1EL, l2ChainID))
		opt.Add(sysgo.WithSuperDeployOpSuccinctL2OutputOracle(ids.L1CL, ids.L1EL, ids.L2ACL, ids.L2AEL, sysgo.WithL2OOStartingBlockNumber(1)))
		opt.Add(sysgo.WithSuperSuccinctValidityProposer(ids.L2AProposer, ids.L1CL, ids.L1EL, ids.L2ACL, ids.L2AEL))
	})
}
