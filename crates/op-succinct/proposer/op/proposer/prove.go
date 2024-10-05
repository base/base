package proposer

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"time"

	"github.com/ethereum/go-ethereum/accounts/abi/bind"
	"github.com/succinctlabs/op-succinct-go/proposer/db/ent"
	"github.com/succinctlabs/op-succinct-go/proposer/db/ent/proofrequest"
)

// Process all of the pending proofs.
func (l *L2OutputSubmitter) ProcessPendingProofs() error {
	// Retrieve all proofs that failed without reaching the prover network (specifically, proofs that failed with no proof ID).
	failedReqs, err := l.db.GetProofsFailedOnServer()
	if err != nil {
		return fmt.Errorf("failed to get proofs failed on server: %w", err)
	}

	// Get all proofs that failed to reach the prover network with a timeout.
	timedOutReqs, err := l.db.GetWitnessGenerationTimeoutProofsOnServer()
	if err != nil {
		return fmt.Errorf("failed to get witness generation timeout proofs on server: %w", err)
	}

	// Combine the two lists of proofs.
	reqsToRetry := append(failedReqs, timedOutReqs...)

	if len(reqsToRetry) > 0 {
		l.Log.Info("Retrying failed and timed out proofs.", "failed", len(failedReqs), "timedOut", len(timedOutReqs))
	}

	for _, req := range reqsToRetry {
		err = l.RetryRequest(req)
		if err != nil {
			return fmt.Errorf("failed to retry request: %w", err)
		}
	}

	// Get all pending proofs with a status of requested and a prover ID that is not empty.
	// TODO: There should be a separate proofrequest status for proofs that failed before reaching the prover network,
	// and those that failed after reaching the prover network.
	reqs, err := l.db.GetAllPendingProofs()
	if err != nil {
		return err
	}
	l.Log.Info("Number of Pending Proofs.", "count", len(reqs))
	for _, req := range reqs {
		status, proof, err := l.GetProofStatus(req.ProverRequestID)
		if err != nil {
			l.Log.Error("failed to get proof status for ID", "id", req.ProverRequestID, "err", err)
			return err
		}
		if status == "PROOF_FULFILLED" {
			// Update the proof in the DB and update status to COMPLETE.
			l.Log.Info("Fulfilled Proof", "id", req.ProverRequestID)
			err = l.db.AddProof(req.ID, proof)
			if err != nil {
				l.Log.Error("failed to update completed proof status", "err", err)
				return err
			}
			continue
		}

		timeout := uint64(time.Now().Unix()) > req.ProofRequestTime+l.DriverSetup.Cfg.ProofTimeout
		if timeout || status == "PROOF_UNCLAIMED" {
			// update status in db to "FAILED"
			l.Log.Info("proof timed out", "id", req.ProverRequestID)
			err = l.db.UpdateProofStatus(req.ID, "FAILED")
			if err != nil {
				l.Log.Error("failed to update failed proof status", "err", err)
				return err
			}

			err = l.RetryRequest(req)
			if err != nil {
				return fmt.Errorf("failed to retry request: %w", err)
			}
		}
	}

	return nil
}

func (l *L2OutputSubmitter) RetryRequest(req *ent.ProofRequest) error {
	// If an AGG proof failed, something went deeply wrong. Simply try again.
	if req.Type == proofrequest.TypeAGG {
		l.Log.Error("Agg proof failed. Adding to DB to retry.", "req", req)

		err := l.db.NewEntry("AGG", req.StartBlock, req.EndBlock)
		if err != nil {
			l.Log.Error("failed to add new proof request", "err")
			return err
		}
	} else {
		// Set the existing request to FAILED.
		err := l.db.UpdateProofStatus(req.ID, "FAILED")
		if err != nil {
			l.Log.Error("failed to update proof status", "err", err)
			return err
		}

		// Always just retry SPAN proofs to avoid spamming the cluster with requests.
		// TODO: We may need to split the SPAN proof in half to avoid OOMing the SP1 runtime.
		l.Log.Info("Span proof failed. Adding to DB to retry.", "req", req)
		err = l.db.NewEntry("SPAN", req.StartBlock, req.EndBlock)
		if err != nil {
			l.Log.Error("failed to add new proof request", "err", err)
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
		currentRequestedProofs, err := l.db.GetNumberOfProofsWithStatus(proofrequest.StatusREQ)
		if err != nil {
			return fmt.Errorf("failed to count requested proofs: %w", err)
		}
		if currentRequestedProofs >= int(l.Cfg.MaxConcurrentProofRequests) {
			l.Log.Info("max concurrent proof requests reached, waiting for next cycle")
			return nil
		}
	}
	go func(p ent.ProofRequest) {
		l.Log.Info("requesting proof from server", "type", p.Type, "start", p.StartBlock, "end", p.EndBlock, "id", p.ID)
		err = l.db.UpdateProofStatus(nextProofToRequest.ID, "REQ")
		if err != nil {
			l.Log.Error("failed to update proof status", "err", err)
			return
		}

		err = l.RequestOPSuccinctProof(p)
		if err != nil {
			l.Log.Error("failed to request proof from the OP Succinct server", "err", err, "proof", p)
			err = l.db.UpdateProofStatus(nextProofToRequest.ID, "FAILED")
			if err != nil {
				l.Log.Error("failed to revert proof status", "err", err, "proverRequestID", nextProofToRequest.ID)
			}

			// If the proof fails to be requested, we should add it to the queue to be retried.
			err = l.RetryRequest(nextProofToRequest)
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
	from := latest.Uint64() + 1

	// This fetches the next block number, which is the currentBlock + submissionInterval.
	minTo, err := l.l2ooContract.NextBlockNumber(&bind.CallOpts{Context: ctx})
	if err != nil {
		return fmt.Errorf("failed to get next L2OO output: %w", err)
	}

	l.Log.Info("Checking for AGG proof", "blocksToProve", minTo.Uint64()-from, "latestProvenBlock", from, "minBlockToProveToAgg", minTo.Uint64())
	created, end, err := l.db.TryCreateAggProofFromSpanProofs(from, minTo.Uint64())
	if err != nil {
		return fmt.Errorf("failed to create agg proof from span proofs: %w", err)
	}
	if created {
		l.Log.Info("created new AGG proof", "from", from, "to", end)
	}

	return nil
}

// Request a proof from the OP Succinct server.
func (l *L2OutputSubmitter) RequestOPSuccinctProof(p ent.ProofRequest) error {
	prevConfirmedBlock := p.StartBlock - 1
	var proofId string
	var err error

	if p.Type == proofrequest.TypeAGG {
		proofId, err = l.RequestAggProof(prevConfirmedBlock, p.EndBlock, p.L1BlockHash)
		if err != nil {
			return fmt.Errorf("failed to request AGG proof: %w", err)
		}
	} else if p.Type == proofrequest.TypeSPAN {
		proofId, err = l.RequestSpanProof(prevConfirmedBlock, p.EndBlock)
		if err != nil {
			return fmt.Errorf("failed to request SPAN proof: %w", err)
		}
	} else {
		return fmt.Errorf("unknown proof type: %s", p.Type)
	}

	err = l.db.SetProverRequestID(p.ID, proofId)
	if err != nil {
		return fmt.Errorf("failed to set prover request ID: %w", err)
	}

	return nil
}

type SpanProofRequest struct {
	Start uint64 `json:"start"`
	End   uint64 `json:"end"`
}

type AggProofRequest struct {
	Subproofs [][]byte `json:"subproofs"`
	L1Head    string   `json:"head"`
}
type ProofResponse struct {
	ProofID string `json:"proof_id"`
}

// Request a span proof for the range [l2Start, l2End].
func (l *L2OutputSubmitter) RequestSpanProof(l2Start, l2End uint64) (string, error) {
	if l2Start >= l2End {
		return "", fmt.Errorf("l2Start must be less than l2End")
	}

	l.Log.Info("requesting span proof", "start", l2Start, "end", l2End)
	requestBody := SpanProofRequest{
		Start: l2Start,
		End:   l2End,
	}
	jsonBody, err := json.Marshal(requestBody)
	if err != nil {
		return "", fmt.Errorf("failed to marshal request body: %w", err)
	}

	return l.RequestProofFromServer("request_span_proof", jsonBody)
}

// Request an aggregate proof for the range [start+1, end]. If there is not a consecutive set of span proofs,
// which cover the range, the request will error.
func (l *L2OutputSubmitter) RequestAggProof(start, end uint64, l1BlockHash string) (string, error) {
	l.Log.Info("requesting agg proof", "start", start, "end", end)

	// Query the DB for the consecutive span proofs that cover the range [start+1, end].
	subproofs, err := l.db.GetConsecutiveSpanProofs(start+1, end)
	if err != nil {
		return "", fmt.Errorf("failed to get subproofs: %w", err)
	}
	requestBody := AggProofRequest{
		Subproofs: subproofs,
		L1Head:    l1BlockHash,
	}
	jsonBody, err := json.Marshal(requestBody)
	if err != nil {
		return "", fmt.Errorf("failed to marshal request body: %w", err)
	}

	// Request the agg proof from the server.
	return l.RequestProofFromServer("request_agg_proof", jsonBody)
}

// Request a proof from the OP Succinct server, given the path and the body of the request. Returns
// the proof ID on a successful request.
func (l *L2OutputSubmitter) RequestProofFromServer(urlPath string, jsonBody []byte) (string, error) {
	req, err := http.NewRequest("POST", l.Cfg.OPSuccinctServerUrl+"/"+urlPath, bytes.NewBuffer(jsonBody))
	if err != nil {
		return "", fmt.Errorf("failed to create request: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")

	/// The witness generation for larger proofs can take up to 20 minutes.
	// TODO: Given that the timeout will take a while, we should have a mechanism for querying the status of the witness generation.
	client := &http.Client{
		Timeout: 20 * time.Minute,
	}
	resp, err := client.Do(req)
	if err != nil {
		if netErr, ok := err.(net.Error); ok && netErr.Timeout() {
			return "", fmt.Errorf("request timed out after 10 minutes: %w", err)
		}
		return "", fmt.Errorf("failed to send request: %w", err)
	}
	defer resp.Body.Close()

	// Read the response body.
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return "", fmt.Errorf("error reading the response body: %v", err)
	}

	// Create a variable of the Response type.
	var response ProofResponse

	// Unmarshal the JSON into the response variable.
	err = json.Unmarshal(body, &response)
	if err != nil {
		return "", fmt.Errorf("error decoding JSON response: %v", err)
	}
	l.Log.Info("successfully submitted proof", "proofID", response.ProofID)

	return response.ProofID, nil
}

type ProofStatus struct {
	Status string `json:"status"`
	Proof  []byte `json:"proof"`
}

// Get the status of a proof given its ID.
func (l *L2OutputSubmitter) GetProofStatus(proofId string) (string, []byte, error) {
	req, err := http.NewRequest("GET", l.Cfg.OPSuccinctServerUrl+"/status/"+proofId, nil)
	if err != nil {
		return "", nil, fmt.Errorf("failed to create request: %w", err)
	}

	client := &http.Client{
		Timeout: 30 * time.Second,
	}
	resp, err := client.Do(req)
	if err != nil {
		if err, ok := err.(net.Error); ok && err.Timeout() {
			return "", nil, fmt.Errorf("request timed out after 30 seconds: %w", err)
		}
		return "", nil, fmt.Errorf("failed to send request: %w", err)
	}
	defer resp.Body.Close()

	// Read the response body
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return "", nil, fmt.Errorf("error reading the response body: %v", err)
	}

	// Create a variable of the Response type
	var response ProofStatus

	// Unmarshal the JSON into the response variable
	err = json.Unmarshal(body, &response)
	if err != nil {
		return "", nil, fmt.Errorf("error decoding JSON response: %v", err)
	}

	return response.Status, response.Proof, nil
}
