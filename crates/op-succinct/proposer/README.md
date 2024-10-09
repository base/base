# Overview

The proposer is responsible for watching the current L2 chain, and requesting new proofs from the OP Succinct server
when a new span or aggregate proof is needed.

# Setup

The proposer uses a SQLite database to keep track of the status of the proofs it is currently running. 

## Database Schema

The schema for this database can be found in `op/proposer/db/ent/schema`. To update the schema, modify [proofrequest.go](op/proposer/db/ent/schema/proofrequest.go) and then run:

```bash
cd op/proposer/db
go generate ./...
```

# Running

The proposer can be run in either single-span or full-span mode. In single-span mode, the proposer will request a proof for
the most recent span of the L2 chain. In full-span mode, the proposer will request a proof for the entire L2 chain from the
genesis block to the most recent block.

