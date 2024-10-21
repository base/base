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

type ProofResponse struct {
	ProofID string `json:"proof_id"`
}

type ProofStatus struct {
	Status string `json:"status"`
	Proof  []byte `json:"proof"`
}
