package presets

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"

	bss "github.com/ethereum-optimism/optimism/op-batcher/batcher"
	"github.com/ethereum-optimism/optimism/op-chain-ops/devkeys"
	"github.com/ethereum-optimism/optimism/op-deployer/pkg/deployer/artifacts"
	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	"github.com/ethereum-optimism/optimism/op-devstack/dsl"
	"github.com/ethereum-optimism/optimism/op-devstack/presets"
	"github.com/ethereum-optimism/optimism/op-devstack/shim"
	"github.com/ethereum-optimism/optimism/op-devstack/stack"
	"github.com/ethereum-optimism/optimism/op-devstack/stack/match"
	"github.com/ethereum-optimism/optimism/op-devstack/sysgo"
	"github.com/ethereum-optimism/optimism/op-e2e/e2eutils/intentbuilder"
	"github.com/ethereum-optimism/optimism/op-service/eth"
	"github.com/ethereum/go-ethereum/common/hexutil"
	"github.com/ethereum/go-ethereum/crypto"
	"github.com/succinctlabs/op-succinct/utils"
)

const (
	DefaultL1ID = 900
	DefaultL2ID = 901
)

// L2ChainConfig holds L2 chain parameters configured at deploy time.
type L2ChainConfig struct {
	// L2BlockTime is the L2 block time in seconds
	L2BlockTime uint64
	// EnvFilePath is the path to write RPC endpoints.
	// Two files are created: <path> (127.0.0.1) and <path>.docker (host.docker.internal).
	// If relative, it's resolved against the `/tests` directory.
	EnvFilePath string
	// L1ConfigDir is the directory to write L1 chain config.
	// If relative, it's resolved against the repo root. Defaults to "configs/L1".
	L1ConfigDir string
}

// DefaultL2ChainConfig returns the default L2 chain config for tests (1s block time).
func DefaultL2ChainConfig() L2ChainConfig {
	return L2ChainConfig{L2BlockTime: 1, L1ConfigDir: "configs/L1"}
}

// LongRunningL2ChainConfig returns the L2 chain config for long-running tests (2s block time).
func LongRunningL2ChainConfig() L2ChainConfig {
	return L2ChainConfig{L2BlockTime: 2, L1ConfigDir: "configs/L1"}
}

// ApplyOverrides applies chain parameters as deployer global overrides.
func (c L2ChainConfig) ApplyOverrides(builder intentbuilder.Builder) {
	builder.WithGlobalOverride("l2BlockTime", c.L2BlockTime)
}

type succinctConfigurator func(*stack.CombinedOption[*sysgo.Orchestrator], sysgo.DefaultSingleChainInteropSystemIDs, eth.ChainID)

// WithSuccinctNodes creates a minimal setup with just L1/L2 nodes running
func WithSuccinctNodes(dest *sysgo.DefaultSingleChainInteropSystemIDs, chain L2ChainConfig) stack.CommonOption {
	return withSuccinctPresetCore(dest, chain, 10, nil, nil)
}

func withSuccinctPreset(dest *sysgo.DefaultSingleChainInteropSystemIDs, chain L2ChainConfig, maxBlocksPerSpanBatch int, aggProofMode *string, configure succinctConfigurator) stack.CommonOption {
	return withSuccinctPresetCore(dest, chain, maxBlocksPerSpanBatch, aggProofMode, configure)
}

func withSuccinctPresetCore(dest *sysgo.DefaultSingleChainInteropSystemIDs, chain L2ChainConfig, maxBlocksPerSpanBatch int, aggProofMode *string, configure succinctConfigurator) stack.CommonOption {
	// Ensure OP Succinct artifact symlinks exist before deployer loads artifacts
	if root := utils.RepoRoot(); root != "" {
		if err := utils.SymlinkSuccinctArtifacts(root); err != nil {
			panic("failed to symlink Succinct artifacts: " + err.Error())
		}
	}

	sp1ProofMode := "plonk"
	if aggProofMode != nil {
		sp1ProofMode = *aggProofMode
	}

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
				chain.ApplyOverrides(builder)
				builder.WithL1ContractsLocator(artifacts.MustNewFileLocator(artifactsPath))
				builder.WithL2ContractsLocator(artifacts.MustNewFileLocator(artifactsPath))
			},
			sysgo.WithSP1ProofMode(sp1ProofMode),
			sysgo.WithCommons(ids.L1.ChainID()),
			sysgo.WithPrefundedL2(ids.L1.ChainID(), ids.L2A.ChainID()),
		),
	)
	// Configure batcher to accumulate more blocks before submitting.
	// MaxBlocksPerSpanBatch: max L2 blocks per span batch (matches proposer interval)
	// MaxChannelDuration: max L1 blocks before forcing submission
	const l1BlockTime = uint64(6) // L1 block time in test environment (see sysgo/deployer.go)
	l2TimeSeconds := uint64(maxBlocksPerSpanBatch) * chain.L2BlockTime
	maxChannelDuration := (l2TimeSeconds + l1BlockTime - 1) / l1BlockTime
	opt.Add(sysgo.WithBatcherOption(func(id stack.L2BatcherID, cfg *bss.CLIConfig) {
		cfg.MaxBlocksPerSpanBatch = maxBlocksPerSpanBatch
		cfg.MaxChannelDuration = maxChannelDuration
	}))

	opt.Add(sysgo.WithL1Nodes(ids.L1EL, ids.L1CL))

	opt.Add(sysgo.WithSupervisor(ids.Supervisor, ids.Cluster, ids.L1EL))
	opt.Add(sysgo.WithL2ELNode(ids.L2AEL, sysgo.L2ELWithSupervisor(ids.Supervisor)))
	opt.Add(sysgo.WithL2CLNode(ids.L2ACL, ids.L1CL, ids.L1EL, ids.L2AEL, sysgo.L2CLSequencer(), sysgo.L2CLIndexing()))
	opt.Add(sysgo.WithManagedBySupervisor(ids.L2ACL, ids.Supervisor))

	opt.Add(sysgo.WithBatcher(ids.L2ABatcher, ids.L1EL, ids.L2ACL, ids.L2AEL))
	opt.Add(sysgo.WithTestSequencer(ids.TestSequencer, ids.L1CL, ids.L2ACL, ids.L1EL, ids.L2AEL))
	opt.Add(sysgo.WithFaucets([]stack.L1ELNodeID{ids.L1EL}, []stack.L2ELNodeID{ids.L2AEL}))

	if configure != nil {
		configure(&opt, ids, l2ChainID)
	}

	opt.Add(sysgo.WithL2MetricsDashboard())
	opt.Add(stack.Finally(func(orch *sysgo.Orchestrator) {
		if chain.EnvFilePath != "" {
			writeEnvFiles(orch, ids, l1ChainID, chain.EnvFilePath, chain.L1ConfigDir)
		}
		*dest = ids
	}))

	return stack.MakeCommon(opt)
}

// NewSystem creates a new test system with the given stack option.
// This is a unified function for creating both validity and fault proof test systems.
func NewSystem(t devtest.T, opt stack.CommonOption) *presets.MinimalWithProposer {
	sys, _, _ := newSystemWithProposer(t, opt, nil)
	return sys
}

// NewSystemNodesOnly creates a test system without a proposer (nodes only).
func NewSystemNodesOnly(t devtest.T, opt stack.CommonOption) *presets.Minimal {
	minimal, _, _ := newSystemCore(t, opt)
	return minimal
}

// newSystemWithProposer creates a new test system and returns the orchestrator and proposer backend.
// If ids is nil, the proposer backend is not retrieved.
func newSystemWithProposer(t devtest.T, opt stack.CommonOption, ids *sysgo.DefaultSingleChainInteropSystemIDs) (*presets.MinimalWithProposer, *sysgo.Orchestrator, sysgo.L2ProposerBackend) {
	minimal, orch, system := newSystemCore(t, opt)

	l2 := system.L2Network(match.Assume(t, match.L2ChainA))
	proposer := l2.L2Proposer(match.Assume(t, match.FirstL2Proposer))

	sys := &presets.MinimalWithProposer{
		Minimal:    *minimal,
		L2Proposer: dsl.NewL2Proposer(proposer),
	}

	var prop sysgo.L2ProposerBackend
	if ids != nil {
		var ok bool
		prop, ok = orch.GetProposer(ids.L2AProposer)
		t.Require().True(ok, "proposer not found")
	}

	return sys, orch, prop
}

// newSystemCore creates the orchestrator and minimal system shared by all system constructors.
func newSystemCore(t devtest.T, opt stack.CommonOption) (*presets.Minimal, *sysgo.Orchestrator, stack.ExtensibleSystem) {
	p := devtest.NewP(t.Ctx(), t.Logger(), func(now bool) {
		t.Errorf("test failed")
		if now {
			t.FailNow()
		}
	}, func() {
		t.SkipNow()
	})
	t.Cleanup(p.Close)

	combinedOpt := stack.Combine(opt, presets.WithSafeDBEnabled())

	orch := sysgo.NewOrchestrator(p, stack.SystemHook(combinedOpt))
	var orchIntf stack.Orchestrator = orch
	stack.ApplyOptionLifecycle(combinedOpt, orchIntf)

	system := shim.NewSystem(t)
	orch.Hydrate(system)

	return presets.MinimalFromSystem(t, system, orch), orch, system
}

// writeEnvFiles writes RPC endpoints and L1 chain config for external proposer use.
// Creates two env files: <envFilePath> (127.0.0.1) and <envFilePath>.docker (host.docker.internal).
// envFilePath is resolved against tests/, l1ConfigDir is resolved against repo root.
func writeEnvFiles(orch *sysgo.Orchestrator, ids sysgo.DefaultSingleChainInteropSystemIDs, l1ChainID eth.ChainID, envFilePath, l1ConfigDir string) {
	logger := orch.P().Logger()
	require := orch.P().Require()

	repoRoot := utils.RepoRoot()
	testsDir := filepath.Join(repoRoot, "tests")

	// Resolve envFilePath relative to tests/
	envPath := envFilePath
	if !filepath.IsAbs(envFilePath) {
		envPath = filepath.Join(testsDir, envFilePath)
	}
	dockerEnvPath := envPath + ".docker"

	// Resolve l1ConfigDir relative to repo root
	if !filepath.IsAbs(l1ConfigDir) {
		l1ConfigDir = filepath.Join(repoRoot, l1ConfigDir)
	}

	// Fetch nodes and keys
	l1EL, ok := orch.GetL1EL(ids.L1EL)
	require.True(ok, "l1 EL node required")
	l1CL, ok := orch.GetL1CL(ids.L1CL)
	require.True(ok, "l1 CL node required")
	l2EL, ok := orch.GetL2EL(ids.L2AEL)
	require.True(ok, "l2 EL node required")
	l2CL, ok := orch.GetL2CL(ids.L2ACL)
	require.True(ok, "l2 CL node required")
	l1Net, ok := orch.GetL1Net(l1ChainID)
	require.True(ok, "l1 network required")
	privKey, err := orch.GetKeys().Secret(devkeys.L1ProxyAdminOwnerRole.Key(l1ChainID.ToBig()))
	require.NoError(err, "failed to get private key")

	// Write L1 chain config
	err = os.MkdirAll(l1ConfigDir, 0o755)
	require.NoError(err, "failed to create L1 config dir")
	l1ConfigPath := filepath.Join(l1ConfigDir, l1ChainID.String()+".json")
	l1ChainConfig, err := json.Marshal(l1Net.Genesis().Config)
	require.NoError(err, "failed to marshal L1 chain config")
	err = os.WriteFile(l1ConfigPath, l1ChainConfig, 0o644)
	require.NoError(err, "failed to write L1 chain config")
	logger.Info("Wrote L1 chain config", "path", l1ConfigPath)

	// Build env vars
	l2RPC := strings.ReplaceAll(l2EL.UserRPC(), "ws://", "http://")
	privKeyHex := hexutil.Encode(crypto.FromECDSA(privKey))
	hostEnvVars := map[string]string{
		"L1_RPC":        l1EL.UserRPC(),
		"L1_BEACON_RPC": l1CL.BeaconRPC(),
		"L2_RPC":        l2RPC,
		"L2_NODE_RPC":   l2CL.UserRPC(),
		"PRIVATE_KEY":   privKeyHex,
	}

	// Write host env file (127.0.0.1)
	err = sysgo.WriteEnvFile(envPath, hostEnvVars)
	require.NoError(err, "failed to write .env file")
	logger.Info("Wrote .env file", "path", envPath)

	// Write Docker env file (host.docker.internal, container paths)
	toDocker := func(url string) string {
		return strings.ReplaceAll(url, "127.0.0.1", "host.docker.internal")
	}
	dockerEnvVars := map[string]string{
		"L1_RPC":        toDocker(l1EL.UserRPC()),
		"L1_BEACON_RPC": toDocker(l1CL.BeaconRPC()),
		"L2_RPC":        toDocker(l2RPC),
		"L2_NODE_RPC":   toDocker(l2CL.UserRPC()),
		"PRIVATE_KEY":   privKeyHex,
	}
	err = sysgo.WriteEnvFile(dockerEnvPath, dockerEnvVars)
	require.NoError(err, "failed to write .env.docker file")
	logger.Info("Wrote .env.docker file", "path", dockerEnvPath)
}
