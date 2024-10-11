package utils

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"math/big"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
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

// CustomBytes32 is a wrapper around eth.Bytes32 that can unmarshal from both
// full-length and minimal hex strings.
type CustomBytes32 eth.Bytes32

// Unmarshal some data into a CustomBytes32.
func (b *CustomBytes32) UnmarshalJSON(data []byte) error {
	var s string
	if err := json.Unmarshal(data, &s); err != nil {
		return err
	}

	// Remove "0x" prefix if present.
	s = strings.TrimPrefix(s, "0x")

	// Pad the string to 64 characters (32 bytes) with leading zeros.
	s = fmt.Sprintf("%064s", s)

	// Add back the "0x" prefix.
	s = "0x" + s

	bytes, err := common.ParseHexOrString(s)
	if err != nil {
		return err
	}

	if len(bytes) != 32 {
		return fmt.Errorf("invalid length for Bytes32: got %d, want 32", len(bytes))
	}

	copy((*b)[:], bytes)
	return nil
}

// LoadOPStackRollupConfigFromChainID loads and parses the rollup config for the given L2 chain ID.
func LoadOPStackRollupConfigFromChainID(l2ChainId uint64) (*rollup.Config, error) {
	// Determine the path to the rollup config file.
	_, currentFile, _, _ := runtime.Caller(0)
	currentDir := filepath.Dir(currentFile)
	path := filepath.Join(currentDir, "..", "..", "..", "..", "configs", strconv.FormatUint(l2ChainId, 10), "rollup.json")

	// Read the rollup config file.
	rollupCfg, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("failed to read rollup config: %w", err)
	}

	// Parse the JSON config.
	var rawConfig map[string]interface{}
	if err := json.Unmarshal(rollupCfg, &rawConfig); err != nil {
		return nil, fmt.Errorf("failed to unmarshal rollup config: %w", err)
	}

	// Convert the Rust SuperchainConfig types to Go types, as they differ in a few places.
	convertedConfig, err := convertConfigTypes(rawConfig)
	if err != nil {
		return nil, fmt.Errorf("failed to convert config types: %w", err)
	}

	// Marshal the converted config back to JSON.
	modifiedConfig, err := json.Marshal(convertedConfig)
	if err != nil {
		return nil, fmt.Errorf("failed to re-marshal modified config: %w", err)
	}

	// Unmarshal into the actual rollup.Config struct.
	var config rollup.Config
	if err := json.Unmarshal(modifiedConfig, &config); err != nil {
		return nil, fmt.Errorf("failed to unmarshal modified rollup config: %w", err)
	}

	return &config, nil
}

// The JSON serialization of the Rust superchain-primitives types differ from the Go types (ex. U256 instead of Bytes32, U64 instead of uint64, etc.)
// This function converts the Rust types in the rollup config JSON to the Go types.
func convertConfigTypes(rawConfig map[string]interface{}) (map[string]interface{}, error) {
	// Convert genesis block numbers.
	if genesis, ok := rawConfig["genesis"].(map[string]interface{}); ok {
		convertBlockNumber(genesis, "l1")
		convertBlockNumber(genesis, "l2")
		convertSystemConfig(genesis)
	}

	// Convert base fee parameters.
	convertBaseFeeParams(rawConfig, "base_fee_params")
	convertBaseFeeParams(rawConfig, "canyon_base_fee_params")

	return rawConfig, nil
}

// convertBlockNumber converts the block number from hex string to integer.
func convertBlockNumber(data map[string]interface{}, key string) {
	if block, ok := data[key].(map[string]interface{}); ok {
		if number, ok := block["number"].(string); ok {
			if intNumber, err := strconv.ParseInt(strings.TrimPrefix(number, "0x"), 16, 64); err == nil {
				block["number"] = intNumber
			}
		}
	}
}

// convertSystemConfig converts the overhead and scalar fields in the system config.
func convertSystemConfig(genesis map[string]interface{}) {
	if systemConfig, ok := genesis["system_config"].(map[string]interface{}); ok {
		convertBytes32Field(systemConfig, "overhead")
		convertBytes32Field(systemConfig, "scalar")
	}
}

// convertBytes32Field converts a hex string to CustomBytes32 which can unmarshal from both
// full-length and minimal hex strings.
func convertBytes32Field(data map[string]interface{}, key string) {
	if value, ok := data[key].(string); ok {
		var customValue CustomBytes32
		if err := customValue.UnmarshalJSON([]byte(`"` + value + `"`)); err == nil {
			data[key] = eth.Bytes32(customValue)
		}
	}
}

// convertBaseFeeParams converts the max_change_denominator from hex string to integer.
func convertBaseFeeParams(rawConfig map[string]interface{}, key string) {
	if params, ok := rawConfig[key].(map[string]interface{}); ok {
		if maxChangeDenominator, ok := params["max_change_denominator"].(string); ok {
			if intValue, err := strconv.ParseInt(strings.TrimPrefix(maxChangeDenominator, "0x"), 16, 64); err == nil {
				params["max_change_denominator"] = intValue
			}
		}
	}
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

// / Get the L2 block number for the given L2 timestamp.
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
				// If AsSpanBatch fails, return the entire range.
				log.Printf("couldn't convert batch %v to span batch\n", idx)
				ranges = append(ranges, SpanBatchRange{Start: startBlock, End: endBlock})
				return ranges, nil
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

// Set up the batch decoder config.
func setupBatchDecoderConfig(config *BatchDecoderConfig) (*rollup.Config, error) {
	rollupCfg, err := LoadOPStackRollupConfigFromChainID(config.L2ChainID.Uint64())
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
