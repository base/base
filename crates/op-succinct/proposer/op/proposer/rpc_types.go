package proposer

type SpanProofRequest struct {
	Start uint64 `json:"start"`
	End   uint64 `json:"end"`
}

type AggProofRequest struct {
	Subproofs [][]byte `json:"subproofs"`
	L1Head    string   `json:"head"`
}

type ValidateConfigRequest struct {
	Address string `json:"address"`
}

type ValidateConfigResponse struct {
	RollupConfigHashValid bool `json:"rollup_config_hash_valid"`
	AggVkeyValid          bool `json:"agg_vkey_valid"`
	RangeVkeyValid        bool `json:"range_vkey_valid"`
}

// WitnessGenerationResponse is the response type for the `request_span_proof` and `request_agg_proof`
// RPCs from the op-succinct-server.
type WitnessGenerationResponse struct {
	ProofID []byte `json:"proof_id"`
}

// UnclaimDescription is the description of why a proof was unclaimed.
type UnclaimDescription int

const (
	UnexpectedProverError UnclaimDescription = iota
	ProgramExecutionError
	CycleLimitExceeded
	// Other is a catch-all for any other unclaim description that doesn't fit into the above categories.
	// Typically, this is used for proofs that are forcibly unclaimed by the cluster.
	Other
)

func (d UnclaimDescription) String() string {
	switch d {
	case UnexpectedProverError:
		return "UnexpectedProverError"
	case ProgramExecutionError:
		return "ProgramExecutionError" 
	case CycleLimitExceeded:
		return "CycleLimitExceeded"
	case Other:
		return "Other"
	default:
		return "Unknown"
	}
}

// SP1FulfillmentStatus represents the fulfillment status of a proof in the SP1 network.
type SP1FulfillmentStatus int

const (
	SP1FulfillmentStatusUnspecified SP1FulfillmentStatus = iota
	SP1FulfillmentStatusRequested
	SP1FulfillmentStatusAssigned
	SP1FulfillmentStatusFulfilled
	SP1FulfillmentStatusUnfulfillable
)

// SP1ExecutionStatus represents the execution status of a proof in the SP1 network.
type SP1ExecutionStatus int

const (
	SP1ExecutionStatusUnspecified SP1ExecutionStatus = iota
	SP1ExecutionStatusUnexecuted
	SP1ExecutionStatusExecuted
	SP1ExecutionStatusUnexecutable
)

// ProofStatusResponse is the response type for the `/status/:proof_id` RPC from the op-succinct-server.
type ProofStatusResponse struct {
	FulfillmentStatus SP1FulfillmentStatus `json:"fulfillment_status"`
	ExecutionStatus   SP1ExecutionStatus   `json:"execution_status"`
	Proof             []byte               `json:"proof"`
}

