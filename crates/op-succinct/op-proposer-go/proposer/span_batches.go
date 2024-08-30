package proposer

import (
	"context"
	"fmt"
	"math/big"

	"github.com/ethereum-optimism/optimism/op-node/rollup"
	"github.com/ethereum/go-ethereum/accounts/abi/bind"
	"github.com/succinctlabs/op-succinct-go/proposer/db/ent"
	"github.com/succinctlabs/op-succinct-go/proposer/utils"
)

func (l *L2OutputSubmitter) DeriveNewSpanBatches(ctx context.Context) error {
	// nextBlock is equal to the highest value in the `EndBlock` column of the db, plus 1.
	latestL2EndBlock, err := l.db.GetLatestEndBlock()
	if err != nil {
		if ent.IsNotFound(err) {
			latestEndBlockU256, err := l.l2ooContract.LatestBlockNumber(&bind.CallOpts{Context: ctx})
			if err != nil {
				return fmt.Errorf("failed to get latest output index: %w", err)
			} else {
				latestL2EndBlock = latestEndBlockU256.Uint64()
			}
		} else {
			l.Log.Error("failed to get latest end requested", "err", err)
			return err
		}
	}
	newL2StartBlock := latestL2EndBlock + 1
	l.Log.Info("deriving span batch for L2 block", "nextBlock", newL2StartBlock)

	rollupClient, err := l.RollupProvider.RollupClient(ctx)
	if err != nil {
		return fmt.Errorf("failed to get rollup client: %w", err)
	}

	// Get the latest finalized L1 block.
	status, err := rollupClient.SyncStatus(ctx)
	if err != nil {
		l.Log.Error("proposer unable to get sync status", "err", err)
		return err
	}
	// Note: Originally, this used the L1 finalized block. However, to satisfy the new API, we now use the L2 finalized block.
	newL2EndBlock := status.FinalizedL2.Number

	l1BeaconClient, err := utils.SetupBeacon(l.Cfg.BeaconRpc)
	if err != nil {
		l.Log.Error("failed to setup beacon", "err", err)
		return err
	}

	// Get the rollup config for the chain to fetch the batcher address.
	rollupCfg, err := rollup.LoadOPStackRollupConfig(l.Cfg.L2ChainID)
	if err != nil {
		return fmt.Errorf("failed to load rollup config: %w", err)
	}

	config := utils.BatchDecoderConfig{
		L2ChainID:    new(big.Int).SetUint64(l.Cfg.L2ChainID),
		L2Node:       rollupClient,
		L1RPC:        *l.L1Client,
		L1Beacon:     l1BeaconClient,
		BatchSender:  rollupCfg.Genesis.SystemConfig.BatcherAddr,
		L2StartBlock: newL2StartBlock,
		L2EndBlock:   newL2EndBlock,
		DataDir:      fmt.Sprintf("/tmp/batch_decoder/%d/transactions_cache", l.Cfg.L2ChainID),
	}
	// Pull all of the batches from the l1Start to l1End from chain to disk.
	ranges, err := utils.GetAllSpanBatchesInL2BlockRange(config)
	fmt.Println("Found", len(ranges), "valid span batches.")
	if err != nil {
		l.Log.Error("failed to get span batch ranges", "err", err)
		return err
	}

	// Loop over the ranges and insert them into the DB. If the width of the span batch is greater than
	// maxBlockRangePerSpanProof, we need to split the ranges into smaller ones and insert them into the DB.
	for _, r := range ranges {
		start := r.Start
		for start <= r.End {
			end := start + l.DriverSetup.Cfg.MaxBlockRangePerSpanProof - 1
			if end > r.End {
				end = r.End
			}

			err := l.db.NewEntry("SPAN", start, end)
			if err != nil {
				l.Log.Error("failed to insert proof request", "err", err, "start", start, "end", end)
				return err
			}

			l.Log.Info("inserted span proof request", "start", start, "end", end)
			start = end + 1
		}
	}

	return nil
}
