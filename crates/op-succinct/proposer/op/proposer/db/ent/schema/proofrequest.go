package schema

import (
	"entgo.io/ent"
	"entgo.io/ent/dialect/entsql"
	"entgo.io/ent/schema"
	"entgo.io/ent/schema/field"
)

// ProofRequest holds the schema definition for the ProofRequest entity.
type ProofRequest struct {
	ent.Schema
}

func (ProofRequest) Annotations() []schema.Annotation {
	// Use STRICT mode to enforce strong typing.
	return []schema.Annotation{
		entsql.Annotation{Table: "proof_requests", Options: "STRICT"},
	}
}

// Fields of the ProofRequest.
func (ProofRequest) Fields() []ent.Field {
	return []ent.Field{
		field.Enum("type").Values("SPAN", "AGG"),
		field.Uint64("start_block"),
		field.Uint64("end_block"),
		field.Enum("status").Values("UNREQ", "WITNESSGEN", "PROVING", "FAILED", "COMPLETE"),
		field.Uint64("request_added_time"),
		field.String("prover_request_id").Optional(),
		field.Uint64("proof_request_time").Optional(),
		field.Uint64("last_updated_time"),
		field.Uint64("l1_block_number").Optional(),
		field.String("l1_block_hash").Optional(),
		field.Bytes("proof").Optional(),
	}
}
