import sqlite3
from enum import Enum
import os
from dotenv import load_dotenv
import time
import sys
import datetime

# Types of proofs
class ProofType(Enum):
    SPAN = "SPAN"
    AGG = "AGG"

# Possible statuses for a proof request
class ProofStatus(Enum):
    UNREQ = "UNREQ"
    WITNESSGEN = "WITNESSGEN"
    PROVING = "PROVING"
    COMPLETE = "COMPLETE"
    FAILED = "FAILED"

# Represents a proof request with all its attributes
class ProofRequest:
    id: int
    type: ProofType
    start_block: int
    end_block: int
    status: ProofStatus
    request_added_time: int
    prover_request_id: str
    proof_request_time: int
    l1_block_number: int
    l1_block_hash: str
    proof: bytes

    def __init__(self, id: int, type: ProofType, start_block: int, end_block: int,
                 status: ProofStatus, request_added_time: int, prover_request_id: str,
                 proof_request_time: int, l1_block_number: int, l1_block_hash: str, proof: bytes):
        self.id = id
        self.type = type
        self.start_block = start_block
        self.end_block = end_block
        self.status = status
        self.request_added_time = request_added_time
        self.prover_request_id = prover_request_id
        self.proof_request_time = proof_request_time
        self.l1_block_number = l1_block_number
        self.l1_block_hash = l1_block_hash
        self.proof = proof

# Converts a database result to a ProofRequest object
def convert_to_proof_request(result):
    return ProofRequest(
        id=result[0],
        type=ProofType(result[1]),
        start_block=result[2],
        end_block=result[3],
        status=ProofStatus(result[4]),
        request_added_time=result[5],
        prover_request_id=result[6],
        proof_request_time=result[7],
        l1_block_number=result[8],
        l1_block_hash=result[9],
        proof=result[10]
    )

# Queries proofs of a specific type from the database
def query_proofs(db_path, proof_type) -> [ProofRequest]:
    conn = sqlite3.connect(db_path)
    cursor = conn.cursor()

    query = f"""
    SELECT * FROM proof_requests
    WHERE type = '{proof_type}'
    """
    cursor.execute(query)
    
    results = cursor.fetchall()
    conn.close()

    return [convert_to_proof_request(result) for result in results] if results else []

# Queries span proofs from the database
def query_span_proofs(db_path) -> [ProofRequest]:
    return query_proofs(db_path, 'SPAN')

# Queries aggregation proofs from the database
def query_agg_proofs(db_path) -> [ProofRequest]:
    return query_proofs(db_path, 'AGG')


if __name__ == "__main__":
    # Load environment variables from .env file
    load_dotenv()

    # Get chain ID from command line args
    if len(sys.argv) != 2:
        print("Usage: python query_proofs.py <chain_id>")
        sys.exit(1)
    chain_id = sys.argv[1]

    db_path = f"../../db/{chain_id}/proofs.db"

    print(f"DB Path: {db_path}")

    # Get all span proofs
    print("\nSpan Proofs")
    print("-" * 50)
    span_proofs = query_span_proofs(db_path)
    for proof in span_proofs:
        if proof.status == ProofStatus.FAILED:
            print(f"Request ID: {proof.id}, Date: {datetime.datetime.fromtimestamp(proof.request_added_time).strftime('%Y-%m-%d %H:%M:%S')}, Start Block: {proof.start_block}, End Block: {proof.end_block}")

    # Print unique failed blocks
    failed_blocks = set()
    for proof in span_proofs:
        if proof.status == ProofStatus.FAILED:
            failed_blocks.add((proof.start_block, proof.end_block))
    
    print("\nUnique Failed Block Ranges:")
    for start, end in sorted(failed_blocks):
        print(f"Start Block: {start}, End Block: {end}")
    print("-" * 50)

    # Query for aggregation proofs
    print("\nAggregation Proofs") 
    print("-" * 50)
    agg_proofs = query_agg_proofs(db_path)
    for proof in agg_proofs:
        print(f"Proof ID: {proof.id}, Type: {proof.type}, Start Block: {proof.start_block}, End Block: {proof.end_block}, Status: {proof.status}, Prover Request ID: {proof.prover_request_id}")
    print("-" * 50)