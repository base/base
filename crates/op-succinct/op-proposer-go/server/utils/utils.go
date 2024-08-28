package utils

import (
	"context"
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
	L2Node            string
	L1RPC             string
	L1Beacon          string
	BatchSender       common.Address
	DataDir           string
}

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
func GetAllSpanBatchesInBlockRange(config BatchDecoderConfig) ([]SpanBatchRange, error) {
	rollupCfg, err := setupBatchDecoderConfig(&config)
	if err != nil {
		return nil, fmt.Errorf("failed to setup config: %w", err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	rollupClient, err := dial.DialRollupClientWithTimeout(ctx, dial.DefaultDialTimeout, nil, config.L2Node)
	if err != nil {
		return nil, fmt.Errorf("failed to dial rollup client: %w", err)
	}

	l1Origin, finalizedL1, err := getL1Origins(rollupClient, config.L2StartBlock, config.L2EndBlock)
	if err != nil {
		return nil, fmt.Errorf("failed to get L1 origin and finalized: %w", err)
	}

	// Clear the out directory so that loading the transaction frames is fast. Otherwise, when loading thousands of transactions,
	// this process can become quite slow.
	err = os.RemoveAll(config.DataDir)
	if err != nil {
		return nil, fmt.Errorf("failed to clear out directory: %w", err)
	}

	// Step on the L1 and store all batches posted in config.DataDir.
	err = fetchBatches(config, rollupCfg, l1Origin, finalizedL1)
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

// Get the L1 origin corresponding to the given L2 block and the latest finalized L1 block.
func getL1Origins(rollupClient *sources.RollupClient, startBlock, endBlock uint64) (uint64, uint64, error) {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	output, err := rollupClient.OutputAtBlock(ctx, startBlock)
	if err != nil {
		return 0, 0, fmt.Errorf("failed to get output at start block: %w", err)
	}
	startL1Origin := output.BlockRef.L1Origin.Number

	output, err = rollupClient.OutputAtBlock(ctx, endBlock)
	if err != nil {
		return 0, 0, fmt.Errorf("failed to get output at end block: %w", err)
	}

	// Fetch an L1 origin that is at least 10 minutes after the end block to guarantee that the batches have been posted.
	// TODO: This won't work if the L1 block time is not 12 seconds. Find a way to get the L1 block time, OR find the nearest
	// L1 origin that is after the timestamp we want.
	l1BlockTime := 12
	endL1Origin := output.BlockRef.L1Origin.Number + (uint64(60/l1BlockTime) * 10)

	return startL1Origin, endL1Origin, nil
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

// Read all of the batches posted to the BatchInbox contract in the given L1 block range.
// Once the batches are fetched, they are written to the given data directory.
func fetchBatches(config BatchDecoderConfig, rollupCfg *rollup.Config, l1Origin, finalizedL1 uint64) error {
	fetchConfig := fetch.Config{
		Start:   l1Origin,
		End:     finalizedL1,
		ChainID: rollupCfg.L1ChainID,
		BatchSenders: map[common.Address]struct{}{
			config.BatchSender: {},
		},
		BatchInbox:         config.BatchInboxAddress,
		OutDirectory:       config.DataDir,
		ConcurrentRequests: 10,
	}

	l1Client, err := ethclient.Dial(config.L1RPC)
	if err != nil {
		return fmt.Errorf("failed to dial L1 client: %w", err)
	}

	beacon, err := setupBeacon(config)
	if err != nil {
		return err
	}

	totalValid, totalInvalid := fetch.Batches(l1Client, beacon, fetchConfig)
	fmt.Printf("Fetched batches in range [%v,%v). Found %v valid & %v invalid batches\n", fetchConfig.Start, fetchConfig.End, totalValid, totalInvalid)

	return nil
}

// Setup the L1 Beacon client.
func setupBeacon(config BatchDecoderConfig) (*sources.L1BeaconClient, error) {
	if config.L1Beacon == "" {
		fmt.Println("L1 Beacon endpoint not set. Unable to fetch post-ecotone channel frames")
		return nil, nil
	}

	beaconClient := sources.NewBeaconHTTPClient(client.NewBasicHTTPClient(config.L1Beacon, nil))
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
