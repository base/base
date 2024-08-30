package schema

import (
	"entgo.io/ent"
	"entgo.io/ent/schema/field"
)

// ProofRequest holds the schema definition for the ProofRequest entity.
type ProofRequest struct {
	ent.Schema
}

// Fields of the ProofRequest.
func (ProofRequest) Fields() []ent.Field {
	return []ent.Field{
		field.Enum("type").Values("SPAN", "AGG"),
		field.Uint64("start_block"),
		field.Uint64("end_block"),
		field.Enum("status").Values("UNREQ", "REQ", "FAILED", "COMPLETE"),
		field.Uint64("request_added_time"),
		field.String("prover_request_id").Optional(),
		field.Uint64("proof_request_time").Optional(),
		field.Uint64("l1_block_number").Optional(),
		field.String("l1_block_hash").Optional(),
		field.Bytes("proof").Optional(),
	}
}
