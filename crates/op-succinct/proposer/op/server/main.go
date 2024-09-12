package main

import (
	"encoding/json"
	"fmt"
	"log"
	"math/big"
	"net/http"
	"sort"

	"github.com/ethereum-optimism/optimism/op-service/dial"
	"github.com/ethereum/go-ethereum/ethclient"
	"github.com/succinctlabs/op-succinct-go/proposer/utils"

	"github.com/ethereum/go-ethereum/common"
	"github.com/gorilla/mux"
)

// Span batch request is a request to find all span batches in a given block range.
type SpanBatchRequest struct {
	StartBlock  uint64 `json:"startBlock"`
	EndBlock    uint64 `json:"endBlock"`
	L2ChainID   uint64 `json:"l2ChainID"`
	L2Node      string `json:"l2Node"`
	L1RPC       string `json:"l1RPC"`
	L1Beacon    string `json:"l1Beacon"`
	BatchSender string `json:"batchSender"`
}

// Response to a span batch request.
type SpanBatchResponse struct {
	Ranges []utils.SpanBatchRange `json:"ranges"`
}

func main() {
	r := mux.NewRouter()
	r.HandleFunc("/span-batch-ranges", handleSpanBatchRanges).Methods("POST")

	fmt.Println("Server is running on :8089")
	log.Fatal(http.ListenAndServe(":8089", r))
}

// Return all of the span batches in a given L2 block range.
func handleSpanBatchRanges(w http.ResponseWriter, r *http.Request) {
	var req SpanBatchRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}

	l1BeaconClient, err := utils.SetupBeacon(req.L1Beacon)
	if err != nil {
		fmt.Printf("Error setting up beacon: %v\n", err)
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	l1Client, err := ethclient.Dial(req.L1RPC)
	if err != nil {
		fmt.Printf("Error creating L1 client: %v\n", err)
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	l2Node, err := dial.DialRollupClientWithTimeout(r.Context(), dial.DefaultDialTimeout, nil, req.L2Node)
	if err != nil {
		fmt.Printf("Error dialing L2 node: %v\n", err)
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	config := utils.BatchDecoderConfig{
		L2ChainID:    new(big.Int).SetUint64(req.L2ChainID),
		L2Node:       l2Node,
		L1RPC:        *l1Client,
		L1Beacon:     l1BeaconClient,
		BatchSender:  common.HexToAddress(req.BatchSender),
		L2StartBlock: req.StartBlock,
		L2EndBlock:   req.EndBlock,
		DataDir:      fmt.Sprintf("/tmp/batch_decoder/%d/transactions_cache", req.L2ChainID),
	}

	ranges, err := utils.GetAllSpanBatchesInL2BlockRange(config)
	if err != nil {
		fmt.Printf("Error getting span batch ranges: %v\n", err)
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	// Sort the ranges by start block
	sort.Slice(ranges, func(i, j int) bool {
		return ranges[i].Start < ranges[j].Start
	})

	response := SpanBatchResponse{
		Ranges: ranges,
	}

	fmt.Printf("Response: %v\n", response)

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(response)
}
