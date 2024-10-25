package proposer

import (
	"context"
	"flag"
	"fmt"
	"math/big"
	"os"
	"testing"

	"github.com/ethereum-optimism/optimism/op-proposer/metrics"
	opservice "github.com/ethereum-optimism/optimism/op-service"
	oplog "github.com/ethereum-optimism/optimism/op-service/log"
	"github.com/ethereum-optimism/optimism/op-service/txmgr"
	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/core/types"
	"github.com/joho/godotenv"
	opsuccinctbindings "github.com/succinctlabs/op-succinct-go/bindings"
	"github.com/succinctlabs/op-succinct-go/proposer/flags"
	"github.com/urfave/cli/v2"
)

// Tests checkpointing a block hash on an L2OO contract.
func TestCheckpointBlockHash(t *testing.T) {
	app := cli.NewApp()
	app.Flags = flags.Flags
	flagSet := flag.NewFlagSet("test", flag.ContinueOnError)
	for _, f := range app.Flags {
		f.Apply(flagSet)
	}
	cliCtx := cli.NewContext(app, flagSet, nil)

	err := godotenv.Load("../../.env")
	if err != nil {
		t.Fatalf("Error loading .env file: %v", err)
	}

	err = cliCtx.Set(flags.L1EthRpcFlag.Name, os.Getenv("L1_RPC"))
	if err != nil {
		t.Fatalf("failed to set L1EthRpcFlag: %v", err)
	}
	err = cliCtx.Set(flags.RollupRpcFlag.Name, os.Getenv("L2_NODE_RPC"))
	if err != nil {
		t.Fatalf("failed to set RollupRpcFlag: %v", err)
	}
	err = cliCtx.Set(flags.L2OOAddressFlag.Name, os.Getenv("L2OO_ADDRESS"))
	if err != nil {
		t.Fatalf("failed to set L2OOAddressFlag: %v", err)
	}
	err = cliCtx.Set(flags.PollIntervalFlag.Name, os.Getenv("POLL_INTERVAL"))
	if err != nil {
		t.Fatalf("failed to set PollIntervalFlag: %v", err)
	}
	err = cliCtx.Set(flags.BeaconRpcFlag.Name, os.Getenv("L1_BEACON_RPC"))
	if err != nil {
		t.Fatalf("failed to set BeaconRpcFlag: %v", err)
	}
	err = cliCtx.Set(txmgr.PrivateKeyFlagName, os.Getenv("PRIVATE_KEY"))
	if err != nil {
		t.Fatalf("failed to set PrivateKeyFlag: %v", err)
	}

	if err := flags.CheckRequired(cliCtx); err != nil {
		t.Fatalf("failed to check required flags: %v", err)
	}
	cfg := NewConfig(cliCtx)
	if err := cfg.Check(); err != nil {
		t.Fatalf("invalid CLI flags: %v", err)
	}

	l := oplog.NewLogger(oplog.AppOut(cliCtx), cfg.LogConfig)
	oplog.SetGlobalLogHandler(l.Handler())
	opservice.ValidateEnvVars(flags.EnvVarPrefix, flags.Flags, l)

	txMgrConfig := txmgr.ReadCLIConfig(cliCtx)
	procName := "default"
	metrics := metrics.NewMetrics(procName)
	txManager, err := txmgr.NewSimpleTxManager("proposer", l, metrics, txMgrConfig)
	if err != nil {
		t.Fatalf("failed to create tx manager: %v", err)
	}

	l2ooABI, err := opsuccinctbindings.OPSuccinctL2OutputOracleMetaData.GetAbi()
	if err != nil {
		t.Fatalf("failed to get L2OutputOracle ABI: %v", err)
	}

	var receipt *types.Receipt
	blockNumber := big.NewInt(6601696)
	data, err := l2ooABI.Pack("checkpointBlockHash", blockNumber)
	if err != nil {
		t.Fatalf("failed to pack checkpointBlockHash: %v", err)
	}

	ctx := context.Background()

	fmt.Println("txMgr RPC URL:", txMgrConfig.L1RPCURL)
	fmt.Println("Private Key:", txMgrConfig.PrivateKey)

	l2OutputOracleAddr := common.HexToAddress("0x7c1bb3c873f3c3f9d895ecafbb06ae3b93948447")
	receipt, err = txManager.Send(ctx, txmgr.TxCandidate{
		TxData:   data,
		To:       &l2OutputOracleAddr,
		GasLimit: 1000000,
	})
	if err != nil {
		t.Fatalf("failed to send transaction: %v", err)
	}

	if receipt.Status == types.ReceiptStatusFailed {
		t.Fatalf("checkpoint blockhash tx successfully published but reverted %v", receipt.TxHash)
	} else {
		t.Fatalf("checkpoint blockhash tx successfully published %v", receipt.TxHash)
	}
}
