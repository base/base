package proposer

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"strings"
	"time"

	"github.com/ethereum/go-ethereum/accounts/abi/bind"
	"github.com/succinctlabs/op-succinct-go/proposer/db/ent"
	"github.com/succinctlabs/op-succinct-go/proposer/db/ent/proofrequest"
)

const PROOF_STATUS_TIMEOUT = 30 * time.Second

// Process all of requests in PROVING state.
func (l *L2OutputSubmitter) ProcessProvingRequests() error {
	// Get all proof requests that are currently in the PROVING state.
	reqs, err := l.db.GetAllProofsWithStatus(proofrequest.StatusPROVING)
	if err != nil {
		return err
	}
	for _, req := range reqs {
		proofStatus, err := l.GetProofStatus(req.ProverRequestID)
		if err != nil {
			l.Log.Error("failed to get proof status for ID", "id", req.ProverRequestID, "err", err)

			// Record the error for the get proof status call.
			l.Metr.RecordError("get_proof_status", 1)
			return err
		}
		if proofStatus.FulfillmentStatus == SP1FulfillmentStatusFulfilled {
			// Update the proof in the DB and update status to COMPLETE.
			l.Log.Info("Fulfilled Proof", "id", req.ProverRequestID)
			err = l.db.AddFulfilledProof(req.ID, proofStatus.Proof)
			if err != nil {
				l.Log.Error("failed to update completed proof status", "err", err)
				return err
			}
			continue
		}

		if proofStatus.FulfillmentStatus == SP1FulfillmentStatusUnfulfillable {
			// Record the failure reason.
			l.Log.Info("Proof is unfulfillable", "id", req.ProverRequestID)
			l.Metr.RecordProveFailure("unfulfillable")

			err = l.RetryRequest(req, proofStatus)
			if err != nil {
				return fmt.Errorf("failed to retry request: %w", err)
			}
		}
	}

	return nil
}

// Process all of requests in WITNESSGEN state.
func (l *L2OutputSubmitter) ProcessWitnessgenRequests() error {
	// Get all proof requests that are currently in the WITNESSGEN state.
	reqs, err := l.db.GetAllProofsWithStatus(proofrequest.StatusWITNESSGEN)
	if err != nil {
		return err
	}
	for _, req := range reqs {
		// If the request has been in the WITNESSGEN state for longer than the timeout, set status to FAILED.
		// This is a catch-all in case the witness generation state update failed.
		if req.LastUpdatedTime+uint64(l.Cfg.WitnessGenTimeout) < uint64(time.Now().Unix()) {
			// Retry the request if it timed out.
			l.RetryRequest(req, ProofStatusResponse{})
		}
	}

	return nil
}

// Retry a proof request. Sets the status of a proof to FAILED and retries the proof based on the optional proof status response.
// If an error response is received:
// - Range Proof: Split in two if the block range is > 1. Retry the same request if range is 1 block.
// - Agg Proof: Retry the same request.
func (l *L2OutputSubmitter) RetryRequest(req *ent.ProofRequest, status ProofStatusResponse) error {
	err := l.db.UpdateProofStatus(req.ID, proofrequest.StatusFAILED)
	if err != nil {
		l.Log.Error("failed to update proof status", "err", err)
		return err
	}

	// If there's an execution error AND the request is a SPAN proof AND the block range is > 1, split the request into two requests.
	// This is likely caused by an SP1 OOM due to a large block range with many transactions.
	// TODO: This solution can be removed once the embedded allocator is used, because then the programs
	// will never OOM.
	if req.Type == proofrequest.TypeSPAN && status.ExecutionStatus == SP1ExecutionStatusUnexecutable && req.EndBlock-req.StartBlock > 1 {
		// Split the request into two requests.
		midBlock := (req.StartBlock + req.EndBlock) / 2
		err = l.db.NewEntry(req.Type, req.StartBlock, midBlock)
		if err != nil {
			l.Log.Error("failed to retry first half of proof request", "err", err)
			return err
		}
		err = l.db.NewEntry(req.Type, midBlock+1, req.EndBlock)
		if err != nil {
			l.Log.Error("failed to retry second half of proof request", "err", err)
			return err
		}
	} else {
		// Retry the same request.
		err = l.db.NewEntry(req.Type, req.StartBlock, req.EndBlock)
		if err != nil {
			l.Log.Error("failed to retry proof request", "err", err)
			return err
		}
	}

	return nil
}

func (l *L2OutputSubmitter) RequestQueuedProofs(ctx context.Context) error {
	nextProofToRequest, err := l.db.GetNextUnrequestedProof()
	if err != nil {
		return fmt.Errorf("failed to get unrequested proofs: %w", err)
	}
	if nextProofToRequest == nil {
		return nil
	}

	if nextProofToRequest.Type == proofrequest.TypeAGG {
		if nextProofToRequest.L1BlockHash == "" {
			blockNumber, blockHash, err := l.checkpointBlockHash(ctx)
			if err != nil {
				l.Log.Error("failed to checkpoint block hash", "err", err)
				return err
			}
			nextProofToRequest, err = l.db.AddL1BlockInfoToAggRequest(nextProofToRequest.StartBlock, nextProofToRequest.EndBlock, blockNumber, blockHash.Hex())
			if err != nil {
				l.Log.Error("failed to add L1 block info to AGG request", "err", err)
			}

			// wait for the next loop so that we have the version with the block info added
			return nil
		} else {
			l.Log.Info("found agg proof with already checkpointed l1 block info")
		}
	} else {
		witnessGenProofs, err := l.db.GetNumberOfRequestsWithStatuses(proofrequest.StatusWITNESSGEN)
		if err != nil {
			return fmt.Errorf("failed to count witnessgen proofs: %w", err)
		}
		provingProofs, err := l.db.GetNumberOfRequestsWithStatuses(proofrequest.StatusPROVING)
		if err != nil {
			return fmt.Errorf("failed to count proving proofs: %w", err)
		}

		// The number of witness generation requests is capped at MAX_CONCURRENT_WITNESS_GEN. This prevents overloading the machine with processes spawned by the witness generation server.
		// Once https://github.com/anton-rs/kona/issues/553 is fixed, we may be able to remove this check.
		if witnessGenProofs >= int(l.Cfg.MaxConcurrentWitnessGen) {
			l.Log.Info("max witness generation reached, waiting for next cycle")
			return nil
		}

		// The total number of concurrent proofs is capped at MAX_CONCURRENT_PROOF_REQUESTS.
		if (witnessGenProofs + provingProofs) >= int(l.Cfg.MaxConcurrentProofRequests) {
			l.Log.Info("max concurrent proof requests reached, waiting for next cycle")
			return nil
		}
	}
	go func(p ent.ProofRequest) {
		l.Log.Info("requesting proof from server", "type", p.Type, "start", p.StartBlock, "end", p.EndBlock, "id", p.ID)
		// Set the proof status to WITNESSGEN.
		err = l.db.UpdateProofStatus(p.ID, proofrequest.StatusWITNESSGEN)
		if err != nil {
			l.Log.Error("failed to update proof status", "err", err)
			return
		}

		// Request the type of proof depending on the mock configuration.
		err = l.RequestProof(p, l.Cfg.Mock)
		if err != nil {
			// If the proof fails to be requested, we should add it to the queue to be retried.
			err = l.RetryRequest(nextProofToRequest, ProofStatusResponse{})
			if err != nil {
				l.Log.Error("failed to retry request", "err", err)
			}

		}
	}(*nextProofToRequest)

	return nil
}

// Use the L2OO contract to look up the range of blocks that the next proof must cover.
// Check the DB to see if we have sufficient span proofs to request an agg proof that covers this range.
// If so, queue up the agg proof in the DB to be requested later.
func (l *L2OutputSubmitter) DeriveAggProofs(ctx context.Context) error {
	latest, err := l.l2ooContract.LatestBlockNumber(&bind.CallOpts{Context: ctx})
	if err != nil {
		return fmt.Errorf("failed to get latest L2OO output: %w", err)
	}

	// This fetches the next block number, which is the currentBlock + submissionInterval.
	minTo, err := l.l2ooContract.NextBlockNumber(&bind.CallOpts{Context: ctx})
	if err != nil {
		return fmt.Errorf("failed to get next L2OO output: %w", err)
	}

	created, end, err := l.db.TryCreateAggProofFromSpanProofs(latest.Uint64(), minTo.Uint64())
	if err != nil {
		return fmt.Errorf("failed to create agg proof from span proofs: %w", err)
	}
	if created {
		l.Log.Info("created new AGG proof", "from", latest.Uint64(), "to", end)
	}

	return nil
}

func (l *L2OutputSubmitter) prepareProofRequest(p ent.ProofRequest) ([]byte, error) {
	if p.Type == proofrequest.TypeSPAN {
		if p.StartBlock >= p.EndBlock {
			return nil, fmt.Errorf("l2Start must be less than l2End")
		}

		requestBody := SpanProofRequest{
			Start: p.StartBlock,
			End:   p.EndBlock,
		}
		jsonBody, err := json.Marshal(requestBody)
		if err != nil {
			return nil, fmt.Errorf("failed to marshal request body: %w", err)
		}
		return jsonBody, nil
	} else {
		subproofs, err := l.db.GetConsecutiveSpanProofs(p.StartBlock, p.EndBlock)
		if err != nil {
			return nil, fmt.Errorf("failed to get subproofs: %w", err)
		}
		requestBody := AggProofRequest{
			Subproofs: subproofs,
			L1Head:    p.L1BlockHash,
		}
		jsonBody, err := json.Marshal(requestBody)
		if err != nil {
			return nil, fmt.Errorf("failed to marshal request body: %w", err)
		}
		return jsonBody, nil
	}
}

// RequestProof handles both mock and real proof requests
func (l *L2OutputSubmitter) RequestProof(p ent.ProofRequest, isMock bool) error {
	jsonBody, err := l.prepareProofRequest(p)
	if err != nil {
		return err
	}

	if isMock {
		proofData, err := l.requestMockProof(p.Type, jsonBody)
		if err != nil {
			return fmt.Errorf("mock proof request failed: %w", err)
		}

		// For mock proofs, once the "mock proof" has been generated, set the status to PROVING. AddFulfilledProof expects the proof to be in the PROVING status.
		err = l.db.UpdateProofStatus(p.ID, proofrequest.StatusPROVING)
		if err != nil {
			return fmt.Errorf("failed to set proof status to proving: %w", err)
		}
		return l.db.AddFulfilledProof(p.ID, proofData)
	}

	// Request a real proof from the witness generation server. Returns the proof ID from the network.
	proofID, err := l.requestRealProof(p.Type, jsonBody)
	if err != nil {
		return fmt.Errorf("real proof request failed: %w", err)
	}

	// Set the proof status to PROVING once the prover ID has been retrieved. Only proofs with status PROVING, SUCCESS or FAILED have a prover request ID.
	err = l.db.UpdateProofStatus(p.ID, proofrequest.StatusPROVING)
	if err != nil {
		return fmt.Errorf("failed to set proof status to proving: %w", err)
	}

	return l.db.SetProverRequestID(p.ID, proofID)
}

func (l *L2OutputSubmitter) requestRealProof(proofType proofrequest.Type, jsonBody []byte) ([]byte, error) {
	resp, err := l.makeProofRequest(proofType, jsonBody)
	if err != nil {
		return nil, err
	}

	var response WitnessGenerationResponse
	if err := json.Unmarshal(resp, &response); err != nil {
		return nil, fmt.Errorf("error decoding JSON response: %w", err)
	}
	// Format the proof ID as a hex string.
	proofIdHex := fmt.Sprintf("%x", response.ProofID)
	l.Log.Info("successfully submitted proof", "proofID", proofIdHex)
	return response.ProofID, nil
}

// Request a mock proof from the witness generation server.
func (l *L2OutputSubmitter) requestMockProof(proofType proofrequest.Type, jsonBody []byte) ([]byte, error) {
	resp, err := l.makeProofRequest(proofType, jsonBody)
	if err != nil {
		return nil, err
	}

	var response ProofStatusResponse
	if err := json.Unmarshal(resp, &response); err != nil {
		return nil, fmt.Errorf("error decoding JSON response: %w", err)
	}

	return response.Proof, nil
}

// Make a proof request to the witness generation server for the correct proof type.
func (l *L2OutputSubmitter) makeProofRequest(proofType proofrequest.Type, jsonBody []byte) ([]byte, error) {
	urlPath := l.getProofEndpoint(proofType)
	req, err := http.NewRequest("POST", l.Cfg.OPSuccinctServerUrl+"/"+urlPath, bytes.NewBuffer(jsonBody))
	if err != nil {
		return nil, fmt.Errorf("failed to create request: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")

	timeout := time.Duration(l.Cfg.WitnessGenTimeout) * time.Second
	client := &http.Client{Timeout: timeout}
	resp, err := client.Do(req)
	if err != nil {
		if netErr, ok := err.(net.Error); ok && netErr.Timeout() {
			l.Log.Error("Witness generation request timed out", "err", err)
			l.Metr.RecordWitnessGenFailure("Timeout")
			return nil, fmt.Errorf("request timed out after %s: %w", timeout, err)
		}
		return nil, fmt.Errorf("failed to send request: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		l.Log.Error("Witness generation request failed", "status", resp.StatusCode, "body", resp.Body)
		l.Metr.RecordWitnessGenFailure("Failed")
		return nil, fmt.Errorf("received non-200 status code: %d", resp.StatusCode)
	}

	return io.ReadAll(resp.Body)
}

func (l *L2OutputSubmitter) getProofEndpoint(proofType proofrequest.Type) string {
	switch {
	case proofType == proofrequest.TypeAGG && l.Cfg.Mock:
		return "request_mock_agg_proof"
	case proofType == proofrequest.TypeAGG:
		return "request_agg_proof"
	case l.Cfg.Mock:
		return "request_mock_span_proof"
	default:
		return "request_span_proof"
	}
}

// Get the status of a proof given its ID.
func (l *L2OutputSubmitter) GetProofStatus(proofId string) (ProofStatusResponse, error) {
	req, err := http.NewRequest("GET", l.Cfg.OPSuccinctServerUrl+"/status/"+proofId, nil)
	if err != nil {
		return ProofStatusResponse{}, fmt.Errorf("failed to create request: %w", err)
	}

	client := &http.Client{
		Timeout: PROOF_STATUS_TIMEOUT,
	}
	resp, err := client.Do(req)
	if err != nil {
		if err, ok := err.(net.Error); ok && err.Timeout() {
			return ProofStatusResponse{}, fmt.Errorf("request timed out after %s: %w", PROOF_STATUS_TIMEOUT, err)
		}
		return ProofStatusResponse{}, fmt.Errorf("failed to send request: %w", err)
	}
	defer resp.Body.Close()

	// If the response status code is not 200, return an error.
	if resp.StatusCode != http.StatusOK {
		return ProofStatusResponse{}, fmt.Errorf("received non-200 status code: %d", resp.StatusCode)
	}

	// Read the response body
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return ProofStatusResponse{}, fmt.Errorf("error reading the response body: %v", err)
	}

	// Create a variable of the Response type
	var proofStatus ProofStatusResponse

	// Unmarshal the JSON into the response variable
	err = json.Unmarshal(body, &proofStatus)
	if err != nil {
		return ProofStatusResponse{}, fmt.Errorf("error decoding JSON response: %v", err)
	}

	return proofStatus, nil
}

// Validate the contract's configuration of the aggregation and range verification keys as well
// as the rollup config hash.
func (l *L2OutputSubmitter) ValidateConfig(address string) error {
	l.Log.Info("requesting config validation", "address", address)
	requestBody := ValidateConfigRequest{
		Address: address,
	}
	jsonBody, err := json.Marshal(requestBody)
	if err != nil {
		return fmt.Errorf("failed to marshal request body: %w", err)
	}

	req, err := http.NewRequest("POST", l.Cfg.OPSuccinctServerUrl+"/validate_config", bytes.NewBuffer(jsonBody))
	if err != nil {
		return fmt.Errorf("failed to create request: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")

	client := &http.Client{
		Timeout: PROOF_STATUS_TIMEOUT,
	}

	// Attempt to validate the config up to 5 times with exponential backoff.
	maxRetries := 5
	backoff := 1 * time.Second
	var resp *http.Response

	for i := 0; i < maxRetries; i++ {
		resp, err = client.Do(req)
		if err == nil && resp.StatusCode == http.StatusOK {
			break
		}
		if i == maxRetries-1 {
			if err != nil {
				if err, ok := err.(net.Error); ok && err.Timeout() {
					return fmt.Errorf("request timed out after %s: %w", PROOF_STATUS_TIMEOUT, err)
				}
				return fmt.Errorf("failed to send request: %w", err)
			}
			return fmt.Errorf("server not healthy after %d retries", maxRetries)
		}

		l.Log.Info("server not ready, retrying", "attempt", i+1, "backoff", backoff)
		time.Sleep(backoff)
		backoff *= 2
	}
	defer resp.Body.Close()

	// Read the response body
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return fmt.Errorf("error reading the response body: %v", err)
	}

	// Create a variable of the ValidateConfigResponse type
	var response ValidateConfigResponse

	// Unmarshal the JSON into the response variable
	err = json.Unmarshal(body, &response)
	if err != nil {
		return fmt.Errorf("error decoding JSON response: %v", err)
	}

	var invalidConfigs []string
	if !response.RollupConfigHashValid {
		invalidConfigs = append(invalidConfigs, "rollup config hash")
	}
	if !response.AggVkeyValid {
		invalidConfigs = append(invalidConfigs, "aggregation verification key")
	}
	if !response.RangeVkeyValid {
		invalidConfigs = append(invalidConfigs, "range verification key")
	}
	if len(invalidConfigs) > 0 {
		return fmt.Errorf("config is invalid: %s", strings.Join(invalidConfigs, ", "))
	}

	return nil
}
