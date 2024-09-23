package db

import (
	"context"
	"fmt"
	"os"
	"time"

	"github.com/succinctlabs/op-succinct-go/proposer/db/ent"
	"github.com/succinctlabs/op-succinct-go/proposer/db/ent/proofrequest"

	_ "github.com/mattn/go-sqlite3"
)

type ProofDB struct {
	client *ent.Client
}

// Initialize the database and return a handle to it. If useCachedDb is false, the existing DB at the path will be deleted (if it exists).
func InitDB(dbPath string, useCachedDb bool) (*ProofDB, error) {
	if !useCachedDb {
		os.Remove(dbPath)
	} else {
		fmt.Printf("Using cached DB at %s\n", dbPath)
	}
	connectionString := fmt.Sprintf("file:%s?_fk=1", dbPath)
	client, err := ent.Open("sqlite3", connectionString)
	if err != nil {
		return nil, fmt.Errorf("failed opening connection to sqlite: %v", err)
	}

	// Run the auto migration tool.
	if err := client.Schema.Create(context.Background()); err != nil {
		return nil, fmt.Errorf("failed creating schema resources: %v", err)
	}

	return &ProofDB{client: client}, nil
}

func (db *ProofDB) CloseDB() error {
	if db.client != nil {
		if err := db.client.Close(); err != nil {
			return fmt.Errorf("error closing database: %w", err)
		}
	}
	return nil
}

func (db *ProofDB) NewEntry(proofType string, start, end uint64) error {
	return db.newEntryWithReqAddedTimestamp(proofType, start, end, uint64(time.Now().Unix()))
}

func (db *ProofDB) newEntryWithReqAddedTimestamp(proofType string, start, end, now uint64) error {
	// Convert string to proofrequest.Type
	var pType proofrequest.Type
	switch proofType {
	case "SPAN":
		pType = proofrequest.TypeSPAN
	case "AGG":
		pType = proofrequest.TypeAGG
	default:
		return fmt.Errorf("invalid proof type: %s", proofType)
	}

	_, err := db.client.ProofRequest.
		Create().
		SetType(pType).
		SetStartBlock(start).
		SetEndBlock(end).
		SetStatus(proofrequest.StatusUNREQ).
		SetRequestAddedTime(now).
		Save(context.Background())

	if err != nil {
		return fmt.Errorf("failed to create new entry: %w", err)
	}

	return nil
}

func (db *ProofDB) UpdateProofStatus(id int, newStatus string) error {
	// Convert string to proofrequest.Type
	var pStatus proofrequest.Status
	switch newStatus {
	case "UNREQ":
		pStatus = proofrequest.StatusUNREQ
	case "REQ":
		pStatus = proofrequest.StatusREQ
	case "COMPLETE":
		pStatus = proofrequest.StatusCOMPLETE
	case "FAILED":
		pStatus = proofrequest.StatusFAILED
	default:
		return fmt.Errorf("invalid proof status: %s", newStatus)
	}

	_, err := db.client.ProofRequest.Update().
		Where(proofrequest.ID(id)).
		SetStatus(pStatus).
		Save(context.Background())

	return err
}

func (db *ProofDB) SetProverRequestID(id int, proverRequestID string) error {
	_, err := db.client.ProofRequest.Update().
		Where(proofrequest.ID(id)).
		SetProverRequestID(proverRequestID).
		SetProofRequestTime(uint64(time.Now().Unix())).
		Save(context.Background())

	if err != nil {
		return fmt.Errorf("failed to set prover network id: %w", err)
	}

	return nil
}

func (db *ProofDB) AddProof(id int, proof []byte) error {
	// Start a transaction
	tx, err := db.client.Tx(context.Background())
	if err != nil {
		return fmt.Errorf("failed to start transaction: %w", err)
	}
	defer tx.Rollback()

	// Query the existing proof request
	existingProof, err := tx.ProofRequest.
		Query().
		Where(proofrequest.ID(id)).
		Only(context.Background())

	if err != nil {
		return fmt.Errorf("failed to find existing proof: %w", err)
	}

	// Check if the status is REQ
	if existingProof.Status != proofrequest.StatusREQ {
		return fmt.Errorf("proof request status is not REQ: %v", id)
	}

	// Check if the proof is already set
	if existingProof.Proof != nil {
		return fmt.Errorf("proof is already set: %v", id)
	}

	// Update the proof and status
	_, err = tx.ProofRequest.
		UpdateOne(existingProof).
		SetProof(proof).
		SetStatus(proofrequest.StatusCOMPLETE).
		Save(context.Background())

	if err != nil {
		return fmt.Errorf("failed to update proof and status: %w", err)
	}

	// Commit the transaction
	if err := tx.Commit(); err != nil {
		return fmt.Errorf("failed to commit transaction: %w", err)
	}

	return nil
}

func (db *ProofDB) AddL1BlockInfoToAggRequest(startBlock, endBlock, l1BlockNumber uint64, l1BlockHash string) (*ent.ProofRequest, error) {
	// Perform the update
	rowsAffected, err := db.client.ProofRequest.Update().
		Where(
			proofrequest.TypeEQ(proofrequest.TypeAGG),
			proofrequest.StatusEQ(proofrequest.StatusUNREQ),
			proofrequest.StartBlockEQ(startBlock),
			proofrequest.EndBlockEQ(endBlock),
		).
		SetL1BlockNumber(l1BlockNumber).
		SetL1BlockHash(l1BlockHash).
		Save(context.Background())

	if err != nil {
		return nil, fmt.Errorf("failed to update L1 block info: %w", err)
	}

	if rowsAffected == 0 {
		return nil, fmt.Errorf("no matching proof request found to update")
	}

	// Fetch the updated ProofRequest
	updatedProof, err := db.client.ProofRequest.Query().
		Where(
			proofrequest.TypeEQ(proofrequest.TypeAGG),
			proofrequest.StatusEQ(proofrequest.StatusUNREQ),
			proofrequest.StartBlockEQ(startBlock),
			proofrequest.EndBlockEQ(endBlock),
			proofrequest.L1BlockNumberEQ(l1BlockNumber),
			proofrequest.L1BlockHashEQ(l1BlockHash),
		).
		Only(context.Background())

	if err != nil {
		return nil, fmt.Errorf("failed to fetch updated proof request: %w", err)
	}

	return updatedProof, nil
}

func (db *ProofDB) GetLatestEndBlock() (uint64, error) {
	maxEnd, err := db.client.ProofRequest.Query().
		Order(ent.Desc(proofrequest.FieldEndBlock)).
		Select(proofrequest.FieldEndBlock).
		First(context.Background())
	if err != nil {
		if ent.IsNotFound(err) {
			return 0, err
		}
		return 0, fmt.Errorf("failed to get latest end requested: %w", err)
	}
	return uint64(maxEnd.EndBlock), nil
}

// When restarting the L2OutputSubmitter, some proofs may have been left in a "requested" state without a prover request ID on the server. Until we
// implement a mechanism for querying the status of the witness generation, we need to time out these proofs after a period of time so they can be requested.
func (db *ProofDB) GetWitnessGenerationTimeoutProofsOnServer() ([]*ent.ProofRequest, error) {
	currentTime := time.Now().Unix()
	twentyMinutesAgo := currentTime - 20*60

	proofs, err := db.client.ProofRequest.Query().
		Where(
			proofrequest.StatusEQ(proofrequest.StatusREQ),
			proofrequest.ProverRequestIDIsNil(),
			proofrequest.RequestAddedTimeLT(uint64(twentyMinutesAgo)),
		).
		All(context.Background())

	if err != nil {
		return nil, fmt.Errorf("failed to query witness generation timeout proofs: %w", err)
	}

	return proofs, nil
}

// If a proof failed to be sent to the prover network, it's status will be set to FAILED, but the prover request ID will be empty.
// This function returns all such proofs.
func (db *ProofDB) GetProofsFailedOnServer() ([]*ent.ProofRequest, error) {
	proofs, err := db.client.ProofRequest.Query().
		Where(
			proofrequest.StatusEQ(proofrequest.StatusFAILED),
			proofrequest.ProverRequestIDEQ(""),
		).
		All(context.Background())

	if err != nil {
		if ent.IsNotFound(err) {
			return nil, nil
		}
		return nil, fmt.Errorf("failed to query failed proof: %w", err)
	}

	return proofs, nil
}

// Get all pending proofs with a status of requested and a prover ID that is not empty.
func (db *ProofDB) GetAllPendingProofs() ([]*ent.ProofRequest, error) {
	proofs, err := db.client.ProofRequest.Query().
		Where(
			proofrequest.StatusEQ(proofrequest.StatusREQ),
			proofrequest.ProverRequestIDNEQ(""),
		).
		All(context.Background())

	if err != nil {
		return nil, fmt.Errorf("failed to query pending proofs: %w", err)
	}
	return proofs, nil
}

// GetAllProofsWithStatus returns all proofs with the given status.
func (db *ProofDB) GetAllProofsWithStatus(status proofrequest.Status) ([]*ent.ProofRequest, error) {
	proofs, err := db.client.ProofRequest.Query().
		Where(
			proofrequest.StatusEQ(status),
		).
		All(context.Background())

	if err != nil {
		if ent.IsNotFound(err) {
			return nil, nil
		}
		return nil, fmt.Errorf("failed to query proofs with status %s: %w", status, err)
	}

	return proofs, nil
}

// Return the number of proofs with the given status.
func (db *ProofDB) GetNumberOfProofsWithStatus(status proofrequest.Status) (int, error) {
	count, err := db.client.ProofRequest.Query().
		Where(
			proofrequest.StatusEQ(status),
		).
		Count(context.Background())

	if err != nil {
		return 0, fmt.Errorf("failed to count proofs with status %s: %w", status, err)
	}

	return count, nil
}

func (db *ProofDB) GetNextUnrequestedProof() (*ent.ProofRequest, error) {
	// First, try to get an AGG type proof
	aggProof, err := db.client.ProofRequest.Query().
		Where(
			proofrequest.StatusEQ(proofrequest.StatusUNREQ),
			proofrequest.TypeEQ(proofrequest.TypeAGG),
		).
		Order(ent.Asc(proofrequest.FieldRequestAddedTime)).
		First(context.Background())

	if err == nil {
		// We found an AGG proof, return it
		return aggProof, nil
	} else if !ent.IsNotFound(err) {
		// An error occurred that wasn't "not found"
		return nil, fmt.Errorf("failed to query AGG unrequested proof: %w", err)
	}

	// If we're here, it means no AGG proof was found. Let's try SPAN proof.
	spanProof, err := db.client.ProofRequest.Query().
		Where(
			proofrequest.StatusEQ(proofrequest.StatusUNREQ),
			proofrequest.TypeEQ(proofrequest.TypeSPAN),
		).
		Order(ent.Asc(proofrequest.FieldRequestAddedTime)).
		First(context.Background())

	if err != nil {
		if ent.IsNotFound(err) {
			// No SPAN proof found either
			return nil, nil
		}
		return nil, fmt.Errorf("failed to query SPAN unrequested proof: %w", err)
	}

	// We found a SPAN proof
	return spanProof, nil
}

func (db *ProofDB) GetAllCompletedAggProofs(startBlock uint64) ([]*ent.ProofRequest, error) {
	proofs, err := db.client.ProofRequest.Query().
		Where(
			proofrequest.TypeEQ(proofrequest.TypeAGG),
			proofrequest.StartBlockEQ(startBlock),
			proofrequest.StatusEQ(proofrequest.StatusCOMPLETE),
		).
		All(context.Background())

	if err != nil {
		if ent.IsNotFound(err) {
			return nil, nil
		}
		return nil, fmt.Errorf("failed to query completed AGG proof: %w", err)
	}

	return proofs, nil
}

// Try to create an AGG proof from the span proofs that cover the range [from, minTo).
// Returns true if a new AGG proof was created, false otherwise.
func (db *ProofDB) TryCreateAggProofFromSpanProofs(from, minTo uint64) (bool, uint64, error) {
	// If there's already an AGG proof in progress/completed with the same start block, return.
	count, err := db.client.ProofRequest.Query().
		Where(
			proofrequest.TypeEQ(proofrequest.TypeAGG),
			proofrequest.StartBlockEQ(from),
			proofrequest.StatusNEQ(proofrequest.StatusFAILED),
		).
		Count(context.Background())
	if err != nil {
		return false, 0, fmt.Errorf("failed to query DB for AGG proof with start block %d: %w", from, err)
	}
	if count > 0 {
		// There's already an AGG proof in progress with the same start block.
		return false, 0, nil
	}

	// Get the longest contiguous span proof chain so far.
	maxContigousEnd, err := db.GetMaxContiguousSpanProofRange(from)
	if err != nil {
		return false, 0, fmt.Errorf("failed to get max contiguous span proof range: %w", err)
	}

	if maxContigousEnd < minTo {
		// There's no contiguous span proof chain that ends before minTo, so we can't create an AGG proof.
		return false, 0, nil
	}

	// Create a new AGG proof request
	err = db.NewEntry("AGG", from, maxContigousEnd)
	if err != nil {
		return false, 0, fmt.Errorf("failed to insert AGG proof request: %w", err)
	}

	return true, maxContigousEnd, nil
}

// Returns the start and end of the contiguous span proof chain. Use this to determine when to create an AGG proof.
func (db *ProofDB) GetMaxContiguousSpanProofRange(start uint64) (uint64, error) {
	ctx := context.Background()
	client := db.client

	query := client.ProofRequest.Query().
		Where(
			proofrequest.TypeEQ(proofrequest.TypeSPAN),
			proofrequest.StatusEQ(proofrequest.StatusCOMPLETE),
			proofrequest.StartBlockGTE(start),
		).
		Order(ent.Asc(proofrequest.FieldStartBlock))

	// Execute the query.
	spans, err := query.All(ctx)
	if err != nil {
		return 0, fmt.Errorf("failed to query span proofs: %w", err)
	}

	// Keep iterating until we can't find another contiguous span proof.
	currentBlock := start
	for _, span := range spans {
		if span.StartBlock != currentBlock {
			break
		}
		currentBlock = span.EndBlock + 1
	}

	return max(start, currentBlock-1), nil
}

// Get the span proofs that cover the range [start, end]. If there's a gap in the proofs, or the proofs
// don't fully cover the range, return an error.
func (db *ProofDB) GetConsecutiveSpanProofs(start, end uint64) ([][]byte, error) {
	ctx := context.Background()
	client := db.client

	// Query the DB for the span proofs that cover the range [start, end].
	query := client.ProofRequest.Query().
		Where(
			proofrequest.TypeEQ(proofrequest.TypeSPAN),
			proofrequest.StatusEQ(proofrequest.StatusCOMPLETE),
			proofrequest.StartBlockGTE(start),
			proofrequest.EndBlockLTE(end),
		).
		Order(ent.Asc(proofrequest.FieldStartBlock))

	// Execute the query.
	spans, err := query.All(ctx)
	if err != nil {
		return nil, fmt.Errorf("failed to query span proofs: %w", err)
	}

	// Verify that the proofs are consecutive and cover the entire range.
	var result [][]byte
	currentBlock := start

	for _, span := range spans {
		if span.StartBlock != currentBlock {
			return nil, fmt.Errorf("gap in proof chain: expected start block %d, got %d", currentBlock, span.StartBlock)
		}
		result = append(result, span.Proof)
		currentBlock = span.EndBlock + 1
	}

	if currentBlock-1 != end {
		return nil, fmt.Errorf("incomplete proof chain: ends at block %d, expected %d", currentBlock-1, end)
	}

	return result, nil
}
