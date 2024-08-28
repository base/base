package main

import (
	"encoding/json"
	"fmt"
	"log"
	"math/big"
	"net/http"
	"sort"

	"github.com/succinctlabs/op-succinct-go/server/utils"

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

	fmt.Println("Server is running on :8080")
	log.Fatal(http.ListenAndServe(":8080", r))
}

// Return all of the span batches in a given L2 block range.
func handleSpanBatchRanges(w http.ResponseWriter, r *http.Request) {
	var req SpanBatchRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}

	config := utils.BatchDecoderConfig{
		L2ChainID:    new(big.Int).SetUint64(req.L2ChainID),
		L2Node:       req.L2Node,
		L1RPC:        req.L1RPC,
		L1Beacon:     req.L1Beacon,
		BatchSender:  common.HexToAddress(req.BatchSender),
		L2StartBlock: req.StartBlock,
		L2EndBlock:   req.EndBlock,
		DataDir: fmt.Sprintf("/tmp/batch_decoder/%d/transactions_cache", req.L2ChainID),
	}

	ranges, err := utils.GetAllSpanBatchesInBlockRange(config)
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
