package proposer

import (
	"context"
	"fmt"
	"slices"
	"sync"

	"github.com/ethereum-optimism/optimism/op-service/dial"
	"github.com/ethereum-optimism/optimism/op-service/sources"
	"github.com/ethereum/go-ethereum/accounts/abi/bind"
	"github.com/succinctlabs/op-succinct-go/proposer/db/ent"
	"github.com/succinctlabs/op-succinct-go/proposer/db/ent/proofrequest"
	"golang.org/x/sync/errgroup"
)

type Span struct {
	Start uint64
	End   uint64
}

// GetL1HeadForL2Block returns the L1 block from which the L2 block can be derived.
func (l *L2OutputSubmitter) GetL1HeadForL2Block(ctx context.Context, rollupClient *sources.RollupClient, l2End uint64) (uint64, error) {
	status, err := rollupClient.SyncStatus(ctx)
	if err != nil {
		return 0, fmt.Errorf("failed to get sync status: %w", err)
	}
	latestL1Block := status.HeadL1.Number

	// Get the L1 origin of the end block.
	outputResponse, err := rollupClient.OutputAtBlock(ctx, l2End)
	if err != nil {
		return 0, fmt.Errorf("failed to get l1 origin: %w", err)
	}
	L2EndL1Origin := outputResponse.BlockRef.L1Origin.Number

	// Search forward from the L1 origin of the L2 end block until we find a safe head greater than the L2 end block.
	for currentL1Block := L2EndL1Origin; currentL1Block <= latestL1Block; currentL1Block++ {
		safeHead, err := rollupClient.SafeHeadAtL1Block(ctx, currentL1Block)
		if err != nil {
			return 0, fmt.Errorf("failed to get safe head: %w", err)
		}
		// If the safe head is greater than or equal to the L2 end block at this L1 block, then we can derive the L2 end block from this L1 block.
		if safeHead.SafeHead.Number >= l2End {
			return currentL1Block, nil
		}
	}

	return 0, fmt.Errorf("could not find an L1 block with an L2 safe head greater than the L2 end block")
}

func (l *L2OutputSubmitter) isSafeDBActivated(ctx context.Context, rollupClient *sources.RollupClient) (bool, error) {
	// Get the sync status of the rollup node.
	status, err := rollupClient.SyncStatus(ctx)
	if err != nil {
		return false, fmt.Errorf("failed to get sync status: %w", err)
	}

	// Attempt querying the safe head at the latest L1 block.
	_, err = rollupClient.SafeHeadAtL1Block(ctx, status.HeadL1.Number)
	if err != nil {
		return false, fmt.Errorf("failed to get safe head: %w", err)
	}

	return true, nil
}

// SplitRangeBasedOnSafeHeads splits a range into spans based on safe head boundaries.
// This is useful when we want to ensure that each span aligns with L2 safe head boundaries.
func (l *L2OutputSubmitter) SplitRangeBasedOnSafeHeads(ctx context.Context, l2Start, l2End uint64) ([]Span, error) {
	spans := []Span{}
	currentStart := l2Start

	rollupClient, err := dial.DialRollupClientWithTimeout(ctx, dial.DefaultDialTimeout, l.Log, l.Cfg.RollupRpc)
	if err != nil {
		return nil, err
	}

	L1Head, err := l.GetL1HeadForL2Block(ctx, rollupClient, l2End)
	if err != nil {
		return nil, fmt.Errorf("failed to get l1 head for l2 block: %w", err)
	}

	l2StartOutput, err := rollupClient.OutputAtBlock(ctx, l2Start)
	if err != nil {
		return nil, fmt.Errorf("failed to get l2 start output: %w", err)
	}
	L2StartL1Origin := l2StartOutput.BlockRef.L1Origin.Number

	safeHeads := make(map[uint64]struct{})
	mu := sync.Mutex{}
	g := errgroup.Group{}
	g.SetLimit(10)

	// Get all of the safe heads between the L2 start block and the L1 head. Use parallel requests to speed up the process.
	// This is useful for when a chain is behind.
	for currentL1Block := L2StartL1Origin; currentL1Block <= L1Head; currentL1Block++ {
		l1Block := currentL1Block
		g.Go(func() error {
			safeHead, err := rollupClient.SafeHeadAtL1Block(ctx, l1Block)
			if err != nil {
				return fmt.Errorf("failed to get safe head at block %d: %w", l1Block, err)
			}

			mu.Lock()
			safeHeads[safeHead.SafeHead.Number] = struct{}{}
			mu.Unlock()
			return nil
		})
	}

	if err := g.Wait(); err != nil {
		return nil, fmt.Errorf("failed while getting safe heads: %w", err)
	}

	uniqueSafeHeads := make([]uint64, 0, len(safeHeads))
	for safeHead := range safeHeads {
		uniqueSafeHeads = append(uniqueSafeHeads, safeHead)
	}
	slices.Sort(uniqueSafeHeads)

	// Loop over all of the safe heads and create spans.
	for _, safeHead := range uniqueSafeHeads {
		if safeHead > currentStart {
			rangeStart := currentStart
			for rangeStart+l.Cfg.MaxBlockRangePerSpanProof < min(l2End, safeHead) {
				spans = append(spans, Span{
					Start: rangeStart,
					End:   rangeStart + l.Cfg.MaxBlockRangePerSpanProof,
				})
				rangeStart += l.Cfg.MaxBlockRangePerSpanProof
			}
			spans = append(spans, Span{
				Start: rangeStart,
				End:   min(l2End, safeHead),
			})
			currentStart = safeHead
		}
	}

	return spans, nil
}

// CreateSpans creates a list of spans of size MaxBlockRangePerSpanProof from start to end. Note: The end of span i = start of span i+1.
func (l *L2OutputSubmitter) SplitRangeBasic(start, end uint64) []Span {
	spans := []Span{}
	// Create spans of size MaxBlockRangePerSpanProof from start to end.
	// Each span starts where the previous one ended.
	// Continue until we can't fit another full span before reaching end.
	for i := start; i+l.Cfg.MaxBlockRangePerSpanProof <= end; i += l.Cfg.MaxBlockRangePerSpanProof {
		spans = append(spans, Span{Start: i, End: i + l.Cfg.MaxBlockRangePerSpanProof})
	}
	return spans
}

func (l *L2OutputSubmitter) GetRangeProofBoundaries(ctx context.Context) error {
	// nextBlock is equal to the highest value in the `EndBlock` column of the DB, plus 1.
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
	newL2StartBlock := latestL2EndBlock

	rollupClient, err := dial.DialRollupClientWithTimeout(ctx, dial.DefaultDialTimeout, l.Log, l.Cfg.RollupRpc)
	if err != nil {
		return err
	}

	// Get the latest finalized L2 block.
	status, err := rollupClient.SyncStatus(ctx)
	if err != nil {
		l.Log.Error("proposer unable to get sync status", "err", err)
		return err
	}
	// Note: Originally, this used the L1 finalized block. However, to satisfy the new API, we now use the L2 finalized block.
	newL2EndBlock := status.FinalizedL2.Number

	// Check if the safeDB is activated on the L2 node. If it is, we use the safeHead based range
	// splitting algorithm. Otherwise, we use the simple range splitting algorithm.
	safeDBActivated, err := l.isSafeDBActivated(ctx, rollupClient)
	if err != nil {
		l.Log.Warn("safeDB is not activated. Using simple range splitting algorithm.", "err", err)
	}

	var spans []Span
	// If the safeDB is activated, we use the safeHead based range splitting algorithm.
	// Otherwise, we use the simple range splitting algorithm.
	if safeDBActivated {
		spans, err = l.SplitRangeBasedOnSafeHeads(ctx, newL2StartBlock, newL2EndBlock)
		if err != nil {
			return fmt.Errorf("failed to split range based on safe heads: %w", err)
		}
	} else {
		spans = l.SplitRangeBasic(newL2StartBlock, newL2EndBlock)
	}


	// Add each span to the DB. If there are no spans, we will not create any proofs.
	for _, span := range spans {
		err := l.db.NewEntry(proofrequest.TypeSPAN, span.Start, span.End)
		l.Log.Info("New range proof request.", "start", span.Start, "end", span.End)
		if err != nil {
			l.Log.Error("failed to add span to db", "err", err)
			return err
		}
	}

	return nil
}
