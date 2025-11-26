package presets

import (
	"os"

	"github.com/ethereum-optimism/optimism/op-chain-ops/devkeys"
	"github.com/ethereum-optimism/optimism/op-deployer/pkg/deployer/artifacts"
	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	"github.com/ethereum-optimism/optimism/op-devstack/stack"
	"github.com/ethereum-optimism/optimism/op-devstack/sysgo"
	"github.com/ethereum-optimism/optimism/op-e2e/e2eutils/intentbuilder"
	"github.com/ethereum-optimism/optimism/op-service/eth"
)

const (
	DefaultL1ID = 900
	DefaultL2ID = 901
)

type succinctConfigurator func(*stack.CombinedOption[*sysgo.Orchestrator], sysgo.DefaultSingleChainInteropSystemIDs, eth.ChainID)

func withSuccinctPreset(dest *sysgo.DefaultSingleChainInteropSystemIDs, configure succinctConfigurator) stack.CommonOption {
	l1ChainID := eth.ChainIDFromUInt64(DefaultL1ID)
	l2ChainID := eth.ChainIDFromUInt64(DefaultL2ID)
	ids := sysgo.NewDefaultSingleChainInteropSystemIDs(l1ChainID, l2ChainID)

	opt := stack.Combine[*sysgo.Orchestrator]()
	opt.Add(stack.BeforeDeploy(func(o *sysgo.Orchestrator) {
		o.P().Logger().Info("Setting up")
	}))

	opt.Add(sysgo.WithMnemonicKeys(devkeys.TestMnemonic))

	artifactsPath := os.Getenv("OP_DEPLOYER_ARTIFACTS")
	if artifactsPath == "" {
		panic("OP_DEPLOYER_ARTIFACTS is not set")
	}

	opt.Add(sysgo.WithDeployer(),
		sysgo.WithDeployerPipelineOption(
			sysgo.WithDeployerCacheDir(artifactsPath),
		),
		sysgo.WithDeployerOptions(
			func(_ devtest.P, _ devkeys.Keys, builder intentbuilder.Builder) {
				builder.WithGlobalOverride("l2BlockTime", uint64(1))
				builder.WithL1ContractsLocator(artifacts.MustNewFileLocator(artifactsPath))
				builder.WithL2ContractsLocator(artifacts.MustNewFileLocator(artifactsPath))
			},
			sysgo.WithCommons(ids.L1.ChainID()),
			sysgo.WithPrefundedL2(ids.L1.ChainID(), ids.L2A.ChainID()),
		),
	)

	opt.Add(sysgo.WithL1Nodes(ids.L1EL, ids.L1CL))

	opt.Add(sysgo.WithSupervisor(ids.Supervisor, ids.Cluster, ids.L1EL))
	opt.Add(sysgo.WithL2ELNode(ids.L2AEL, sysgo.L2ELWithSupervisor(ids.Supervisor)))
	opt.Add(sysgo.WithL2CLNode(ids.L2ACL, ids.L1CL, ids.L1EL, ids.L2AEL, sysgo.L2CLSequencer(), sysgo.L2CLIndexing()))
	opt.Add(sysgo.WithManagedBySupervisor(ids.L2ACL, ids.Supervisor))

	opt.Add(sysgo.WithBatcher(ids.L2ABatcher, ids.L1EL, ids.L2ACL, ids.L2AEL))
	opt.Add(sysgo.WithTestSequencer(ids.TestSequencer, ids.L1CL, ids.L2ACL, ids.L1EL, ids.L2AEL))
	opt.Add(sysgo.WithFaucets([]stack.L1ELNodeID{ids.L1EL}, []stack.L2ELNodeID{ids.L2AEL}))

	configure(&opt, ids, l2ChainID)

	opt.Add(sysgo.WithL2MetricsDashboard())
	opt.Add(stack.Finally(func(orch *sysgo.Orchestrator) {
		*dest = ids
	}))

	return stack.MakeCommon(opt)
}
