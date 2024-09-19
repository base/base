package main

import (
	"context"
	"fmt"
	"os"
	"testing"

	"github.com/ethereum-optimism/optimism/op-service/dial"
	"github.com/ethereum/go-ethereum/ethclient"
	"github.com/joho/godotenv"
	"github.com/succinctlabs/op-succinct-go/proposer/utils"
)

// This test fetches span batches for a recent block range and confirms that the number of span batches is non-zero.
// Sanity check to ensure that the span batch fetching logic is working correctly.
func TestHandleSpanBatchRanges(t *testing.T) {

	// Load environment variables if the .env file exists.
	_ = godotenv.Load()

	l2Rpc := os.Getenv("L2_RPC")
	l2Node := os.Getenv("L2_NODE_RPC")
	l1RPC := os.Getenv("L1_RPC")
	l1Beacon := os.Getenv("L1_BEACON_RPC")

	if l2Rpc == "" || l1RPC == "" || l1Beacon == "" {
		t.Fatalf("Required environment variables are not set")
	}

	// Get the L2 chain ID from the L2 RPC.
	l2Client, err := ethclient.Dial(l2Rpc)
	if err != nil {
		t.Fatalf("Failed to connect to L2 RPC: %v", err)
	}
	chainID, err := l2Client.ChainID(context.Background())
	if err != nil {
		t.Fatalf("Failed to get chain ID: %v", err)
	}
	// Load the rollup config for the given L2 chain ID.
	rollupCfg, err := utils.LoadOPStackRollupConfigFromChainID(chainID.Uint64())
	if err != nil {
		t.Fatalf("Failed to get rollup config: %v", err)
	}

	// Get a recent block from the L2 RPC
	client, err := ethclient.Dial(l2Rpc)
	if err != nil {
		t.Fatalf("Failed to connect to L2 RPC: %v", err)
	}

	block, err := client.BlockNumber(context.Background())
	if err != nil {
		t.Fatalf("Failed to get block number: %v", err)
	}

	startBlock := block - 10000
	endBlock := block - 9000

	l1BeaconClient, err := utils.SetupBeacon(l1Beacon)
	if err != nil {
		t.Fatalf("Failed to setup beacon: %v", err)
	}

	l1Client, err := ethclient.Dial(l1RPC)
	if err != nil {
		t.Fatalf("Failed to connect to L1 RPC: %v", err)
	}

	rollupClient, err := dial.DialRollupClientWithTimeout(context.Background(), dial.DefaultDialTimeout, nil, l2Node)
	if err != nil {
		t.Fatalf("Failed to connect to L2 RPC: %v", err)
	}

	config := utils.BatchDecoderConfig{
		L2ChainID:    rollupCfg.L2ChainID,
		L2Node:       rollupClient,
		L1RPC:        *l1Client,
		L1Beacon:     l1BeaconClient,
		BatchSender:  rollupCfg.Genesis.SystemConfig.BatcherAddr,
		L2StartBlock: startBlock,
		L2EndBlock:   endBlock,
		DataDir:      fmt.Sprintf("/tmp/batch_decoder/%d/transactions_cache", rollupCfg.L2ChainID),
	}

	ranges, err := utils.GetAllSpanBatchesInL2BlockRange(config)
	if err != nil {
		t.Fatalf("Failed to get span batch ranges: %v", err)
	}

	// Check that the number of span batches is non-zero
	if len(ranges) == 0 {
		t.Errorf("Expected non-zero span batches, got 0")
	}

	// Print the number of span batches found
	t.Logf("Number of span batches found: %d", len(ranges))

	// Optionally, you can add more specific checks on the ranges returned
	for i, r := range ranges {
		t.Logf("Range %d: Start: %d, End: %d", i, r.Start, r.End)
	}
}
