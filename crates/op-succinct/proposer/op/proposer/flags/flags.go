package flags

import (
	"fmt"
	"time"

	"github.com/urfave/cli/v2"

	opservice "github.com/ethereum-optimism/optimism/op-service"
	oplog "github.com/ethereum-optimism/optimism/op-service/log"
	opmetrics "github.com/ethereum-optimism/optimism/op-service/metrics"
	"github.com/ethereum-optimism/optimism/op-service/oppprof"
	oprpc "github.com/ethereum-optimism/optimism/op-service/rpc"
	"github.com/ethereum-optimism/optimism/op-service/txmgr"
)

const EnvVarPrefix = "OP_PROPOSER"

func prefixEnvVars(name string) []string {
	return opservice.PrefixEnvVar(EnvVarPrefix, name)
}

var (
	// Required Flags
	L1EthRpcFlag = &cli.StringFlag{
		Name:    "l1-eth-rpc",
		Usage:   "HTTP provider URL for L1",
		EnvVars: prefixEnvVars("L1_RPC"),
	}
	RollupRpcFlag = &cli.StringFlag{
		Name:    "rollup-rpc",
		Usage:   "HTTP provider URL for the rollup node. A comma-separated list enables the active rollup provider.",
		EnvVars: prefixEnvVars("L2_NODE_RPC"),
	}
	BeaconRpcFlag = &cli.StringFlag{
		Name:    "beacon-rpc",
		Usage:   "HTTP provider URL for the beacon node",
		EnvVars: prefixEnvVars("L1_BEACON_RPC"),
	}

	// Optional flags
	L2OOAddressFlag = &cli.StringFlag{
		Name:    "l2oo-address",
		Usage:   "Address of the L2OutputOracle contract",
		EnvVars: prefixEnvVars("L2OO_ADDRESS"),
	}
	PollIntervalFlag = &cli.DurationFlag{
		Name:    "poll-interval",
		Usage:   "How frequently to poll L2 for new blocks (legacy L2OO)",
		Value:   12 * time.Second,
		EnvVars: prefixEnvVars("POLL_INTERVAL"),
	}
	AllowNonFinalizedFlag = &cli.BoolFlag{
		Name:    "allow-non-finalized",
		Usage:   "Allow the proposer to submit proposals for L2 blocks derived from non-finalized L1 blocks.",
		EnvVars: prefixEnvVars("ALLOW_NON_FINALIZED"),
	}
	ActiveSequencerCheckDurationFlag = &cli.DurationFlag{
		Name:    "active-sequencer-check-duration",
		Usage:   "The duration between checks to determine the active sequencer endpoint.",
		Value:   2 * time.Minute,
		EnvVars: prefixEnvVars("ACTIVE_SEQUENCER_CHECK_DURATION"),
	}
	WaitNodeSyncFlag = &cli.BoolFlag{
		Name: "wait-node-sync",
		Usage: "Indicates if, during startup, the proposer should wait for the rollup node to sync to " +
			"the current L1 tip before proceeding with its driver loop.",
		Value:   false,
		EnvVars: prefixEnvVars("WAIT_NODE_SYNC"),
	}
	DbPathFlag = &cli.StringFlag{
		Name:    "db-path",
		Usage:   "Path to the database folder used to track OP Succinct proof generation. The DB file is always stored at DbPathFlag/{chain_id}/proofs.db",
		Value:   "./op-proposer",
		EnvVars: prefixEnvVars("DB_PATH"),
	}
	UseCachedDbFlag = &cli.BoolFlag{
		Name:    "use-cached-db",
		Usage:   "Use a cached database instead of creating a new one",
		Value:   false,
		EnvVars: prefixEnvVars("USE_CACHED_DB"),
	}
	SlackTokenFlag = &cli.StringFlag{
		Name:    "slack-token",
		Usage:   "Token for the Slack API",
		EnvVars: prefixEnvVars("SLACK_TOKEN"),
	}
	MaxBlockRangePerSpanProofFlag = &cli.Uint64Flag{
		Name:    "max-block-range-per-span-proof",
		Usage:   "Maximum number of blocks to include in a single span proof",
		Value:   50,
		EnvVars: prefixEnvVars("MAX_BLOCK_RANGE_PER_SPAN_PROOF"),
	}
	ProofTimeoutFlag = &cli.Uint64Flag{
		Name:    "proof-timeout",
		Usage:   "Maximum time in seconds to spend generating a proof before giving up",
		Value:   14400,
		EnvVars: prefixEnvVars("MAX_PROOF_TIME"),
	}
	OPSuccinctServerUrlFlag = &cli.StringFlag{
		Name:    "op-succinct-server-url",
		Usage:   "URL of the OP Succinct server to request proofs from",
		Value:   "http://127.0.0.1:3000",
		EnvVars: prefixEnvVars("OP_SUCCINCT_SERVER_URL"),
	}
	MaxConcurrentProofRequestsFlag = &cli.Uint64Flag{
		Name:    "max-concurrent-proof-requests",
		Usage:   "Maximum number of proofs to generate concurrently",
		Value:   20,
		EnvVars: prefixEnvVars("MAX_CONCURRENT_PROOF_REQUESTS"),
	}
	TxCacheOutDirFlag = &cli.StringFlag{
		Name:    "tx-cache-out-dir",
		Usage:   "Cache directory for the found transactions to determine span batch boundaries",
		Value:   "/tmp/batch_decoder/transactions_cache",
		EnvVars: prefixEnvVars("TX_CACHE_OUT_DIR"),
	}
	BatchInboxFlag = &cli.StringFlag{
		Name:    "batch-inbox",
		Usage:   "Batch Inbox Address",
		EnvVars: prefixEnvVars("BATCH_INBOX"),
	}
	BatcherAddressFlag = &cli.StringFlag{
		Name:    "batcher-address",
		Usage:   "Batch Sender Address",
		EnvVars: prefixEnvVars("BATCHER_ADDRESS"),
	}

	// Legacy Flags
	L2OutputHDPathFlag = txmgr.L2OutputHDPathFlag
)

var requiredFlags = []cli.Flag{
	L1EthRpcFlag,
	RollupRpcFlag,
	BeaconRpcFlag,
}

var optionalFlags = []cli.Flag{
	L2OOAddressFlag,
	PollIntervalFlag,
	AllowNonFinalizedFlag,
	L2OutputHDPathFlag,
	ActiveSequencerCheckDurationFlag,
	WaitNodeSyncFlag,
	DbPathFlag,
	UseCachedDbFlag,
	SlackTokenFlag,
	MaxBlockRangePerSpanProofFlag,
	TxCacheOutDirFlag,
	OPSuccinctServerUrlFlag,
	ProofTimeoutFlag,
	MaxConcurrentProofRequestsFlag,
	BatchInboxFlag,
	BatcherAddressFlag,
}

func init() {
	optionalFlags = append(optionalFlags, oprpc.CLIFlags(EnvVarPrefix)...)
	optionalFlags = append(optionalFlags, oplog.CLIFlags(EnvVarPrefix)...)
	optionalFlags = append(optionalFlags, opmetrics.CLIFlags(EnvVarPrefix)...)
	optionalFlags = append(optionalFlags, oppprof.CLIFlags(EnvVarPrefix)...)
	optionalFlags = append(optionalFlags, txmgr.CLIFlags(EnvVarPrefix)...)

	Flags = append(requiredFlags, optionalFlags...)
}

// Flags contains the list of configuration options available to the binary.
var Flags []cli.Flag

func CheckRequired(ctx *cli.Context) error {
	for _, f := range requiredFlags {
		if !ctx.IsSet(f.Names()[0]) {
			return fmt.Errorf("flag %s is required", f.Names()[0])
		}
	}
	return nil
}
