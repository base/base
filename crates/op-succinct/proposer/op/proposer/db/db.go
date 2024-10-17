package db

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"time"

	"entgo.io/ent/dialect/sql"

	"github.com/succinctlabs/op-succinct-go/proposer/db/ent"
	"github.com/succinctlabs/op-succinct-go/proposer/db/ent/proofrequest"

	_ "github.com/mattn/go-sqlite3"
)

type ProofDB struct {
	writeClient *ent.Client
	readClient  *ent.Client
}

// InitDB initializes the database and returns a handle to it.
// If useCachedDb is false, the existing DB at the path will be deleted (if it exists).
func InitDB(dbPath string, useCachedDb bool) (*ProofDB, error) {
	if !useCachedDb {
		os.Remove(dbPath)
	} else {
		fmt.Printf("Using cached DB at %s\n", dbPath)
	}

	// Create the intermediate directories if they don't exist
	dir := filepath.Dir(dbPath)
	if err := os.MkdirAll(dir, 0755); err != nil {
		return nil, fmt.Errorf("failed to create directories for DB: %w", err)
	}

	// Use the TL;DR SQLite settings from https://kerkour.com/sqlite-for-servers.
	connectionUrl := fmt.Sprintf("file:%s?_fk=1&journal_mode=WAL&synchronous=normal&cache_size=100000000&busy_timeout=15000&_txlock=immediate", dbPath)

	writeDrv, err := sql.Open("sqlite3", connectionUrl)
	if err != nil {
		return nil, fmt.Errorf("failed opening connection to sqlite: %v", err)
	}
	writeDb := writeDrv.DB()

	// The write lock only allows one connection to the DB at a time.
	writeDb.SetMaxOpenConns(1)
	writeDb.SetConnMaxLifetime(time.Hour)

	readDrv, err := sql.Open("sqlite3", connectionUrl)
	if err != nil {
		return nil, fmt.Errorf("failed opening connection to sqlite: %v", err)
	}
	readDb := readDrv.DB()
	readDb.SetMaxOpenConns(max(4, runtime.NumCPU()/4))
	readDb.SetConnMaxLifetime(time.Hour)

	readClient := ent.NewClient(ent.Driver(readDrv))
	writeClient := ent.NewClient(ent.Driver(writeDrv))

	if err := readClient.Schema.Create(context.Background()); err != nil {
		return nil, fmt.Errorf("failed creating schema resources: %v", err)
	}
	if err := writeClient.Schema.Create(context.Background()); err != nil {
		return nil, fmt.Errorf("failed creating schema resources: %v", err)
	}

	return &ProofDB{writeClient: writeClient, readClient: readClient}, nil
}

// CloseDB closes the connection to the database.
func (db *ProofDB) CloseDB() error {
	if db.writeClient != nil {
		if err := db.writeClient.Close(); err != nil {
			return fmt.Errorf("error closing database: %w", err)
		}
	}
	if db.readClient != nil {
		if err := db.readClient.Close(); err != nil {
			return fmt.Errorf("error closing database: %w", err)
		}
	}
	return nil
}

// NewEntry creates a new proof request entry in the database.
func (db *ProofDB) NewEntry(proofType proofrequest.Type, start, end uint64) error {
	now := uint64(time.Now().Unix())
	_, err := db.writeClient.ProofRequest.
		Create().
		SetType(proofType).
		SetStartBlock(start).
		SetEndBlock(end).
		SetStatus(proofrequest.StatusUNREQ).
		SetRequestAddedTime(now).
		SetLastUpdatedTime(now).
		Save(context.Background())

	if err != nil {
		return fmt.Errorf("failed to create new entry: %w", err)
	}

	return nil
}

// UpdateProofStatus updates the status of a proof request in the database.
func (db *ProofDB) UpdateProofStatus(id int, proofStatus proofrequest.Status) error {
	_, err := db.writeClient.ProofRequest.Update().
		Where(proofrequest.ID(id)).
		SetStatus(proofStatus).
		SetLastUpdatedTime(uint64(time.Now().Unix())).
		Save(context.Background())

	return err
}

// SetProverRequestID sets the prover request ID for a proof request in the database.
func (db *ProofDB) SetProverRequestID(id int, proverRequestID string) error {
	_, err := db.writeClient.ProofRequest.Update().
		Where(proofrequest.ID(id)).
		SetProverRequestID(proverRequestID).
		SetProofRequestTime(uint64(time.Now().Unix())).
		SetLastUpdatedTime(uint64(time.Now().Unix())).
		Save(context.Background())

	if err != nil {
		return fmt.Errorf("failed to set prover network id: %w", err)
	}

	return nil
}

// AddFulfilledProof adds a proof to a proof request in the database and sets the status to COMPLETE.
func (db *ProofDB) AddFulfilledProof(id int, proof []byte) error {
	// Start a transaction
	tx, err := db.writeClient.Tx(context.Background())
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

	// Check if the status is PROVING.
	if existingProof.Status != proofrequest.StatusPROVING {
		return fmt.Errorf("proof request status is not PROVING: %v", id)
	}

	// Check if the proof is already set.
	if existingProof.Proof != nil {
		return fmt.Errorf("proof is already set: %v", id)
	}

	// Update the proof and status
	_, err = tx.ProofRequest.
		UpdateOne(existingProof).
		SetProof(proof).
		SetStatus(proofrequest.StatusCOMPLETE).
		SetLastUpdatedTime(uint64(time.Now().Unix())).
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

// GetNumberOfProofsWithStatuses returns the number of proofs with the given status(es).
func (db *ProofDB) GetNumberOfRequestsWithStatuses(statuses ...proofrequest.Status) (int, error) {
	count, err := db.readClient.ProofRequest.Query().
		Where(
			proofrequest.StatusIn(statuses...),
		).
		Count(context.Background())

	if err != nil {
		return 0, fmt.Errorf("failed to count requests with statuses %v: %w", statuses, err)
	}

	return count, nil
}

// AddL1BlockInfoToAggRequest adds the L1 block info to the existing AGG proof request.
func (db *ProofDB) AddL1BlockInfoToAggRequest(startBlock, endBlock, l1BlockNumber uint64, l1BlockHash string) (*ent.ProofRequest, error) {
	// Perform the update
	rowsAffected, err := db.writeClient.ProofRequest.Update().
		Where(
			proofrequest.TypeEQ(proofrequest.TypeAGG),
			proofrequest.StatusEQ(proofrequest.StatusUNREQ),
			proofrequest.StartBlockEQ(startBlock),
			proofrequest.EndBlockEQ(endBlock),
		).
		SetL1BlockNumber(l1BlockNumber).
		SetL1BlockHash(l1BlockHash).
		SetLastUpdatedTime(uint64(time.Now().Unix())).
		Save(context.Background())

	if err != nil {
		return nil, fmt.Errorf("failed to update L1 block info: %w", err)
	}

	if rowsAffected == 0 {
		return nil, fmt.Errorf("no matching proof request found to update")
	}

	// Fetch the updated ProofRequest
	updatedProof, err := db.readClient.ProofRequest.Query().
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

// GetLatestEndBlock returns the latest end block of a proof request in the database.
func (db *ProofDB) GetLatestEndBlock() (uint64, error) {
	maxEnd, err := db.readClient.ProofRequest.Query().
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

	proofs, err := db.readClient.ProofRequest.Query().
		Where(
			proofrequest.StatusEQ(proofrequest.StatusWITNESSGEN),
			proofrequest.ProverRequestIDIsNil(),
			proofrequest.LastUpdatedTimeLT(uint64(twentyMinutesAgo)),
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
	proofs, err := db.readClient.ProofRequest.Query().
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

// GetAllProofsWithStatus returns all proofs with the given status.
func (db *ProofDB) GetAllProofsWithStatus(status proofrequest.Status) ([]*ent.ProofRequest, error) {
	proofs, err := db.readClient.ProofRequest.Query().
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

// GetNextUnrequestedProof returns the next unrequested proof in the database.
func (db *ProofDB) GetNextUnrequestedProof() (*ent.ProofRequest, error) {
	// Get the unrequested AGG proof with the lowest start block.
	aggProof, err := db.readClient.ProofRequest.Query().
		Where(
			proofrequest.StatusEQ(proofrequest.StatusUNREQ),
			proofrequest.TypeEQ(proofrequest.TypeAGG),
		).
		Order(ent.Asc(proofrequest.FieldStartBlock)).
		First(context.Background())

	if err == nil {
		// We found an AGG proof, return it
		return aggProof, nil
	} else if !ent.IsNotFound(err) {
		// An error occurred that wasn't "not found"
		return nil, fmt.Errorf("failed to query AGG unrequested proof: %w", err)
	}

	// If there's no AGG proof available, get the unrequested SPAN proof with the lowest start block.
	spanProof, err := db.readClient.ProofRequest.Query().
		Where(
			proofrequest.StatusEQ(proofrequest.StatusUNREQ),
			proofrequest.TypeEQ(proofrequest.TypeSPAN),
		).
		Order(ent.Asc(proofrequest.FieldStartBlock)).
		First(context.Background())

	if err != nil {
		if ent.IsNotFound(err) {
			// No SPAN proof found either
			return nil, nil
		}
		return nil, fmt.Errorf("failed to query SPAN unrequested proof: %w", err)
	}

	// Return the SPAN proof
	return spanProof, nil
}

// GetAllCompletedAggProofs returns all completed AGG proofs for a given start block.
func (db *ProofDB) GetAllCompletedAggProofs(startBlock uint64) ([]*ent.ProofRequest, error) {
	proofs, err := db.readClient.ProofRequest.Query().
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

// TryCreateAggProofFromSpanProofs tries to create an AGG proof from the span proofs that cover the range [from, minTo).
// Returns true if a new AGG proof was created, false otherwise.
func (db *ProofDB) TryCreateAggProofFromSpanProofs(from, minTo uint64) (bool, uint64, error) {
	// If there's already an AGG proof in progress/completed with the same start block, return.
	count, err := db.readClient.ProofRequest.Query().
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

// GetMaxContiguousSpanProofRange returns the start and end of the contiguous span proof chain.
func (db *ProofDB) GetMaxContiguousSpanProofRange(start uint64) (uint64, error) {
	ctx := context.Background()

	query := db.readClient.ProofRequest.Query().
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
		currentBlock = span.EndBlock
	}

	return max(start, currentBlock), nil
}

// GetConsecutiveSpanProofs returns the span proofs that cover the range [start, end].
// If there's a gap in the proofs, or the proofs don't fully cover the range, return an error.
func (db *ProofDB) GetConsecutiveSpanProofs(start, end uint64) ([][]byte, error) {
	ctx := context.Background()

	// Query the DB for the span proofs that cover the range [start, end].
	query := db.readClient.ProofRequest.Query().
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
		currentBlock = span.EndBlock
	}

	if currentBlock != end {
		return nil, fmt.Errorf("incomplete proof chain: ends at block %d, expected %d", currentBlock, end)
	}

	return result, nil
}
