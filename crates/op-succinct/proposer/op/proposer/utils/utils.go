package utils

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log"
	"math/big"
	"os"
	"time"

	"github.com/ethereum-optimism/optimism/op-node/cmd/batch_decoder/fetch"
	"github.com/ethereum-optimism/optimism/op-node/cmd/batch_decoder/reassemble"
	"github.com/ethereum-optimism/optimism/op-node/rollup"
	"github.com/ethereum-optimism/optimism/op-node/rollup/derive"
	"github.com/ethereum-optimism/optimism/op-service/client"
	"github.com/ethereum-optimism/optimism/op-service/dial"
	"github.com/ethereum-optimism/optimism/op-service/eth"
	"github.com/ethereum-optimism/optimism/op-service/sources"
	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/ethclient"
)

var ErrNoSpanBatchFound = errors.New("no span batch found for the given block")
var ErrMaxDeviationExceeded = errors.New("max deviation exceeded")

// SpanBatchRange represents a range of L2 blocks covered by a span batch
type SpanBatchRange struct {
	Start uint64 `json:"start"`
	End   uint64 `json:"end"`
}

// BatchDecoderConfig is a struct that holds the configuration for the batch decoder.
type BatchDecoderConfig struct {
	L2GenesisTime     uint64
	L2GenesisBlock    uint64
	L2BlockTime       uint64
	BatchInboxAddress common.Address
	L2StartBlock      uint64
	L2EndBlock        uint64
	L2ChainID         *big.Int
	L2Node            dial.RollupClientInterface
	L1RPC             ethclient.Client
	L1Beacon          *sources.L1BeaconClient
	BatchSender       common.Address
	DataDir           string
}

// Get the rollup config from the given L2 RPC.
func GetRollupConfigFromL2Rpc(l2Rpc string) (*rollup.Config, error) {
	l2Client, err := ethclient.Dial(l2Rpc)
	if err != nil {
		return nil, fmt.Errorf("failed to dial L2 client: %w", err)
	}

	chainID, err := l2Client.ChainID(context.Background())
	if err != nil {
		return nil, fmt.Errorf("failed to get chain ID: %w", err)
	}

	rollupCfg, err := rollup.LoadOPStackRollupConfig(chainID.Uint64())
	if err != nil {
		return nil, fmt.Errorf("failed to load rollup config: %w", err)
	}

	return rollupCfg, nil
}

// GetAllSpanBatchesInBlockRange fetches span batches within a range of L2 blocks.
func GetAllSpanBatchesInL2BlockRange(config BatchDecoderConfig) ([]SpanBatchRange, error) {
	rollupCfg, err := setupBatchDecoderConfig(&config)
	if err != nil {
		return nil, fmt.Errorf("failed to setup config: %w", err)
	}

	l1Start, l1End, err := GetL1SearchBoundaries(config.L2Node, config.L1RPC, config.L2StartBlock, config.L2EndBlock)
	if err != nil {
		return nil, fmt.Errorf("failed to get L1 origin and finalized: %w", err)
	}

	// Fetch the batches posted to the BatchInbox contract in the given L1 block range and store them in config.DataDir.
	err = fetchBatchesBetweenL1Blocks(config, rollupCfg, l1Start, l1End)
	if err != nil {
		return nil, fmt.Errorf("failed to fetch batches: %w", err)
	}

	// Reassemble the batches into span batches from the stored transaction frames in config.DataDir.
	reassembleConfig := reassemble.Config{
		BatchInbox:    config.BatchInboxAddress,
		InDirectory:   config.DataDir,
		OutDirectory:  "",
		L2ChainID:     config.L2ChainID,
		L2GenesisTime: config.L2GenesisTime,
		L2BlockTime:   config.L2BlockTime,
	}

	// Get all span batch ranges in the given L2 block range.
	ranges, err := GetSpanBatchRanges(reassembleConfig, rollupCfg, config.L2StartBlock, config.L2EndBlock, 1000000)
	if err != nil {
		return nil, fmt.Errorf("failed to get span batch ranges: %w", err)
	}

	return ranges, nil
}

func TimestampToBlock(rollupCfg *rollup.Config, l2Timestamp uint64) uint64 {
	return ((l2Timestamp - rollupCfg.Genesis.L2Time) / rollupCfg.BlockTime) + rollupCfg.Genesis.L2.Number
}

// Get the block ranges for each span batch in the given L2 block range.
func GetSpanBatchRanges(config reassemble.Config, rollupCfg *rollup.Config, startBlock, endBlock, maxSpanBatchDeviation uint64) ([]SpanBatchRange, error) {
	frames := reassemble.LoadFrames(config.InDirectory, config.BatchInbox)
	framesByChannel := make(map[derive.ChannelID][]reassemble.FrameWithMetadata)
	for _, frame := range frames {
		framesByChannel[frame.Frame.ID] = append(framesByChannel[frame.Frame.ID], frame)
	}

	var ranges []SpanBatchRange

	for id, frames := range framesByChannel {
		ch := processFrames(config, rollupCfg, id, frames)
		if len(ch.Batches) == 0 {
			log.Fatalf("no span batches in channel")
		}

		for idx, b := range ch.Batches {
			batchStartBlock := TimestampToBlock(rollupCfg, b.GetTimestamp())
			spanBatch, success := b.AsSpanBatch()
			if !success {
				log.Fatalf("couldn't convert batch %v to span batch\n", idx)
			}
			blockCount := spanBatch.GetBlockCount()
			batchEndBlock := batchStartBlock + uint64(blockCount) - 1

			if batchStartBlock > endBlock || batchEndBlock < startBlock {
				continue
			} else {
				ranges = append(ranges, SpanBatchRange{Start: max(startBlock, batchStartBlock), End: min(endBlock, batchEndBlock)})
			}
		}
	}

	return ranges, nil
}

// Find the span batch that contains the given L2 block. If no span batch contains the given block, return the start block of the span batch that is closest to the given block.
func GetSpanBatchRange(config reassemble.Config, rollupCfg *rollup.Config, l2Block, maxSpanBatchDeviation uint64) (uint64, uint64, error) {
	frames := reassemble.LoadFrames(config.InDirectory, config.BatchInbox)
	framesByChannel := make(map[derive.ChannelID][]reassemble.FrameWithMetadata)
	for _, frame := range frames {
		framesByChannel[frame.Frame.ID] = append(framesByChannel[frame.Frame.ID], frame)
	}
	for id, frames := range framesByChannel {
		ch := processFrames(config, rollupCfg, id, frames)
		if len(ch.Batches) == 0 {
			log.Fatalf("no span batches in channel")
			return 0, 0, errors.New("no span batches in channel")
		}

		for idx, b := range ch.Batches {
			startBlock := TimestampToBlock(rollupCfg, b.GetTimestamp())
			spanBatch, success := b.AsSpanBatch()
			if !success {
				log.Fatalf("couldn't convert batch %v to span batch\n", idx)
				return 0, 0, errors.New("couldn't convert batch to span batch")
			}
			blockCount := spanBatch.GetBlockCount()
			endBlock := startBlock + uint64(blockCount) - 1
			if l2Block >= startBlock && l2Block <= endBlock {
				return startBlock, endBlock, nil
			} else if l2Block+maxSpanBatchDeviation < startBlock {
				return l2Block, startBlock - 1, ErrMaxDeviationExceeded
			}
		}
	}
	return 0, 0, ErrNoSpanBatchFound
}

// Set up the batch decoder config.
func setupBatchDecoderConfig(config *BatchDecoderConfig) (*rollup.Config, error) {
	rollupCfg, err := rollup.LoadOPStackRollupConfig(config.L2ChainID.Uint64())
	if err != nil {
		return nil, err
	}

	if config.L2GenesisTime != rollupCfg.Genesis.L2Time {
		config.L2GenesisTime = rollupCfg.Genesis.L2Time
		fmt.Printf("L2GenesisTime overridden: %v\n", config.L2GenesisTime)
	}
	if config.L2GenesisBlock != rollupCfg.Genesis.L2.Number {
		config.L2GenesisBlock = rollupCfg.Genesis.L2.Number
		fmt.Printf("L2GenesisBlock overridden: %v\n", config.L2GenesisBlock)
	}
	if config.L2BlockTime != rollupCfg.BlockTime {
		config.L2BlockTime = rollupCfg.BlockTime
		fmt.Printf("L2BlockTime overridden: %v\n", config.L2BlockTime)
	}
	if config.BatchInboxAddress != rollupCfg.BatchInboxAddress {
		config.BatchInboxAddress = rollupCfg.BatchInboxAddress
		fmt.Printf("BatchInboxAddress overridden: %v\n", config.BatchInboxAddress)
	}

	return rollupCfg, nil
}

// Get the L1 boundaries corresponding to the given L2 block range. Specifically, get the L1 origin
// for the first block and an L1 block 10 minutes after the last block to ensure that the batches
// were posted to L1 for these blocks in that period. Pick blocks where it's nearly guaranteeed that
// the relevant batches were posted to L1.
func GetL1SearchBoundaries(rollupClient dial.RollupClientInterface, l1Client ethclient.Client, startBlock, endBlock uint64) (uint64, uint64, error) {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	output, err := rollupClient.OutputAtBlock(ctx, startBlock)
	if err != nil {
		return 0, 0, fmt.Errorf("failed to get output at start block: %w", err)
	}
	startL1Origin := output.BlockRef.L1Origin.Number

	// Get the diff in seconds between startL1Origin and startL1Origin -1 to get the L1 block time.
	block, err := l1Client.BlockByNumber(ctx, big.NewInt(int64(startL1Origin)))
	if err != nil {
		return 0, 0, fmt.Errorf("failed to get block at start L1 origin: %w", err)
	}
	startBlockTime := block.Time()

	// Get the L1 block time by retrieving the timestamp diff between two consecutive L1 blocks.
	block, err = l1Client.BlockByNumber(ctx, big.NewInt(int64(startL1Origin-1)))
	if err != nil {
		return 0, 0, fmt.Errorf("failed to get block at start L1 origin - 1: %w", err)
	}
	l1BlockTime := startBlockTime - block.Time()

	// Get the L1 origin for the last block.
	output, err = rollupClient.OutputAtBlock(ctx, endBlock)
	if err != nil {
		return 0, 0, fmt.Errorf("failed to get output at end block: %w", err)
	}

	// Fetch an L1 block that is at least 10 minutes after the end block to guarantee that the batches have been posted.
	endL1Origin := output.BlockRef.L1Origin.Number + (uint64(60/l1BlockTime) * 10)

	return startL1Origin, endL1Origin, nil
}

// Read all of the batches posted to the BatchInbox contract in the given L1 block range. Once the
// batches are fetched, they are written to the given data directory.
func fetchBatchesBetweenL1Blocks(config BatchDecoderConfig, rollupCfg *rollup.Config, l1Start, l1End uint64) error {
	// Clear the out directory so that loading the transaction frames is fast. Otherwise, when loading thousands of transactions,
	// this process can become quite slow.
	err := os.RemoveAll(config.DataDir)
	if err != nil {
		return fmt.Errorf("failed to clear out directory: %w", err)
	}

	fetchConfig := fetch.Config{
		Start:   l1Start,
		End:     l1End,
		ChainID: rollupCfg.L1ChainID,
		BatchSenders: map[common.Address]struct{}{
			config.BatchSender: {},
		},
		BatchInbox:         config.BatchInboxAddress,
		OutDirectory:       config.DataDir,
		ConcurrentRequests: 10,
	}

	totalValid, totalInvalid := fetch.Batches(&config.L1RPC, config.L1Beacon, fetchConfig)

	fmt.Printf("Fetched batches in range [%v,%v). Found %v valid & %v invalid batches\n", fetchConfig.Start, fetchConfig.End, totalValid, totalInvalid)

	return nil
}

// Setup the L1 Beacon client.
func SetupBeacon(l1BeaconUrl string) (*sources.L1BeaconClient, error) {
	if l1BeaconUrl == "" {
		fmt.Println("L1 Beacon endpoint not set. Unable to fetch post-ecotone channel frames")
		return nil, nil
	}

	beaconClient := sources.NewBeaconHTTPClient(client.NewBasicHTTPClient(l1BeaconUrl, nil))
	beaconCfg := sources.L1BeaconClientConfig{FetchAllSidecars: false}
	beacon := sources.NewL1BeaconClient(beaconClient, beaconCfg)

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	_, err := beacon.GetVersion(ctx)
	if err != nil {
		return nil, fmt.Errorf("failed to check L1 Beacon API version: %w", err)
	}

	return beacon, nil
}

// Copied from op-proposer-go/op-node/cmd/batch_decoder/utils/reassemble.go, because it wasn't exported.
// TODO: Ask Optimism team to export this function.
func processFrames(cfg reassemble.Config, rollupCfg *rollup.Config, id derive.ChannelID, frames []reassemble.FrameWithMetadata) reassemble.ChannelWithMetadata {
	spec := rollup.NewChainSpec(rollupCfg)
	ch := derive.NewChannel(id, eth.L1BlockRef{Number: frames[0].InclusionBlock})
	invalidFrame := false

	for _, frame := range frames {
		if ch.IsReady() {
			fmt.Printf("Channel %v is ready despite having more frames\n", id.String())
			invalidFrame = true
			break
		}
		if err := ch.AddFrame(frame.Frame, eth.L1BlockRef{Number: frame.InclusionBlock, Time: frame.Timestamp}); err != nil {
			fmt.Printf("Error adding to channel %v. Err: %v\n", id.String(), err)
			invalidFrame = true
		}
	}

	var (
		batches    []derive.Batch
		batchTypes []int
		comprAlgos []derive.CompressionAlgo
	)

	invalidBatches := false
	if ch.IsReady() {
		br, err := derive.BatchReader(ch.Reader(), spec.MaxRLPBytesPerChannel(ch.HighestBlock().Time), rollupCfg.IsFjord(ch.HighestBlock().Time))
		if err == nil {
			for batchData, err := br(); err != io.EOF; batchData, err = br() {
				if err != nil {
					fmt.Printf("Error reading batchData for channel %v. Err: %v\n", id.String(), err)
					invalidBatches = true
				} else {
					comprAlgos = append(comprAlgos, batchData.ComprAlgo)
					batchType := batchData.GetBatchType()
					batchTypes = append(batchTypes, int(batchType))
					switch batchType {
					case derive.SingularBatchType:
						singularBatch, err := derive.GetSingularBatch(batchData)
						if err != nil {
							invalidBatches = true
							fmt.Printf("Error converting singularBatch from batchData for channel %v. Err: %v\n", id.String(), err)
						}
						// singularBatch will be nil when errored
						batches = append(batches, singularBatch)
					case derive.SpanBatchType:
						spanBatch, err := derive.DeriveSpanBatch(batchData, cfg.L2BlockTime, cfg.L2GenesisTime, cfg.L2ChainID)
						if err != nil {
							invalidBatches = true
							fmt.Printf("Error deriving spanBatch from batchData for channel %v. Err: %v\n", id.String(), err)
						}
						// spanBatch will be nil when errored
						batches = append(batches, spanBatch)
					default:
						fmt.Printf("unrecognized batch type: %d for channel %v.\n", batchData.GetBatchType(), id.String())
					}
				}
			}
		} else {
			fmt.Printf("Error creating batch reader for channel %v. Err: %v\n", id.String(), err)
		}
	} else {
		fmt.Printf("Channel %v is not ready\n", id.String())
	}

	return reassemble.ChannelWithMetadata{
		ID:             id,
		Frames:         frames,
		IsReady:        ch.IsReady(),
		InvalidFrames:  invalidFrame,
		InvalidBatches: invalidBatches,
		Batches:        batches,
		BatchTypes:     batchTypes,
		ComprAlgos:     comprAlgos,
	}
}
