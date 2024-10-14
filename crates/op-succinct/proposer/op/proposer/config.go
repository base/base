package proposer

import (
	"errors"
	"fmt"
	"log"
	"path/filepath"
	"time"

	"github.com/urfave/cli/v2"

	"github.com/ethereum-optimism/optimism/op-service/dial"
	oplog "github.com/ethereum-optimism/optimism/op-service/log"
	opmetrics "github.com/ethereum-optimism/optimism/op-service/metrics"
	"github.com/ethereum-optimism/optimism/op-service/oppprof"
	oprpc "github.com/ethereum-optimism/optimism/op-service/rpc"
	"github.com/ethereum-optimism/optimism/op-service/txmgr"
	"github.com/succinctlabs/op-succinct-go/proposer/flags"
)

// CLIConfig is a well typed config that is parsed from the CLI params.
// This also contains config options for auxiliary services.
// It is transformed into a `Config` before the L2 output submitter is started.
type CLIConfig struct {
	/* Required Params */

	// L1EthRpc is the HTTP provider URL for L1.
	L1EthRpc string

	// RollupRpc is the HTTP provider URL for the rollup node. A comma-separated list enables the active rollup provider.
	RollupRpc string

	// L2OOAddress is the L2OutputOracle contract address.
	L2OOAddress string

	// PollInterval is the delay between querying L2 for more transaction
	// and creating a new batch.
	PollInterval time.Duration

	// AllowNonFinalized can be set to true to propose outputs
	// for L2 blocks derived from non-finalized L1 data.
	AllowNonFinalized bool

	TxMgrConfig txmgr.CLIConfig

	RPCConfig oprpc.CLIConfig

	LogConfig oplog.CLIConfig

	MetricsConfig opmetrics.CLIConfig

	PprofConfig oppprof.CLIConfig

	// OutputRetryInterval is the delay between retrying output fetch if one fails.
	OutputRetryInterval time.Duration

	// DisputeGameType is the type of dispute game to create when submitting an output proposal.
	DisputeGameType uint32

	// ActiveSequencerCheckDuration is the duration between checks to determine the active sequencer endpoint.
	ActiveSequencerCheckDuration time.Duration

	// Whether to wait for the sequencer to sync to a recent block at startup.
	WaitNodeSync bool

	// Additional fields required for OP Succinct Proposer.

	// Path to the database that tracks proof generation.
	DbPath string

	// UseCachedDb is a flag to use a cached database instead of creating a new one.
	UseCachedDb bool

	// SlackToken is the token for the Slack API.
	SlackToken string

	// L1 Beacon RPC URL used to determine span batch boundaries.
	BeaconRpc string
	// Directory to store the transaction cache when determining span batch boundaries.
	TxCacheOutDir string
	// The max size (in blocks) of a proof we will attempt to generate. If span batches are larger, we break them up.
	MaxBlockRangePerSpanProof uint64
	// The Chain ID of the L2 chain.
	L2ChainID uint64
	// The maximum amount of time we will spend waiting for a proof before giving up and trying again.
	ProofTimeout uint64
	// The URL of the OP Succinct server to request proofs from.
	OPSuccinctServerUrl string
	// The maximum proofs that can be requested from the server concurrently.
	MaxConcurrentProofRequests uint64
	// The batch inbox on L1 to read batches from. Note that this is ignored if L2 Chain ID is in rollup config.
	BatchInbox string
	// The batcher address to include transactions from. Note that this is ignored if L2 Chain ID is in rollup config.
	BatcherAddress string
}

func (c *CLIConfig) Check() error {
	if err := c.RPCConfig.Check(); err != nil {
		return err
	}
	if err := c.MetricsConfig.Check(); err != nil {
		return err
	}
	if err := c.PprofConfig.Check(); err != nil {
		return err
	}
	if err := c.TxMgrConfig.Check(); err != nil {
		return err
	}

	if c.L2OOAddress == "" {
		return errors.New("the `L2OutputOracle` address was not provided")
	}

	return nil
}

// NewConfig parses the Config from the provided flags or environment variables.
func NewConfig(ctx *cli.Context) *CLIConfig {
	// Get the L2 chain ID from the rollup config
	rollupClient, err := dial.DialRollupClientWithTimeout(ctx.Context, dial.DefaultDialTimeout, nil, ctx.String(flags.RollupRpcFlag.Name))
	if err != nil {
		log.Fatal(err)
	}

	rollupConfig, err := rollupClient.RollupConfig(ctx.Context)
	if err != nil {
		log.Fatal(err)
	}

	dbPath := ctx.String(flags.DbPathFlag.Name)
	dbPath = filepath.Join(dbPath, fmt.Sprintf("%d", rollupConfig.L2ChainID.Uint64()), "proofs.db")

	return &CLIConfig{
		// Required Flags
		L1EthRpc:     ctx.String(flags.L1EthRpcFlag.Name),
		RollupRpc:    ctx.String(flags.RollupRpcFlag.Name),
		L2OOAddress:  ctx.String(flags.L2OOAddressFlag.Name),
		PollInterval: ctx.Duration(flags.PollIntervalFlag.Name),
		TxMgrConfig:  txmgr.ReadCLIConfig(ctx),
		BeaconRpc:    ctx.String(flags.BeaconRpcFlag.Name),
		L2ChainID:    rollupConfig.L2ChainID.Uint64(),

		// Optional Flags
		AllowNonFinalized:            ctx.Bool(flags.AllowNonFinalizedFlag.Name),
		RPCConfig:                    oprpc.ReadCLIConfig(ctx),
		LogConfig:                    oplog.ReadCLIConfig(ctx),
		MetricsConfig:                opmetrics.ReadCLIConfig(ctx),
		PprofConfig:                  oppprof.ReadCLIConfig(ctx),
		ActiveSequencerCheckDuration: ctx.Duration(flags.ActiveSequencerCheckDurationFlag.Name),
		WaitNodeSync:                 ctx.Bool(flags.WaitNodeSyncFlag.Name),
		DbPath:                       dbPath,
		UseCachedDb:                  ctx.Bool(flags.UseCachedDbFlag.Name),
		SlackToken:                   ctx.String(flags.SlackTokenFlag.Name),
		MaxBlockRangePerSpanProof:    ctx.Uint64(flags.MaxBlockRangePerSpanProofFlag.Name),
		ProofTimeout:                 ctx.Uint64(flags.ProofTimeoutFlag.Name),
		TxCacheOutDir:                ctx.String(flags.TxCacheOutDirFlag.Name),
		OPSuccinctServerUrl:          ctx.String(flags.OPSuccinctServerUrlFlag.Name),
		MaxConcurrentProofRequests:   ctx.Uint64(flags.MaxConcurrentProofRequestsFlag.Name),
		BatchInbox:                   ctx.String(flags.BatchInboxFlag.Name),
		BatcherAddress:               ctx.String(flags.BatcherAddressFlag.Name),
	}
}
