# Prover Server

A proving server that can request `range` and `aggregate` proofs from the server. The server also
exposes a route to get the status of a proof request.

## Usage

```bash
cargo run --bin server --release
```

## Cost Estimator

To run the cost estimator as if it was a full workload, follow the doc [here](./COST_ESTIMATOR.md).

## Misc

To fetch an existing proof from the prover network and save it locally run:

```bash
cargo run --bin fetch_and_save_proof --release -- --request-id <proofrequest_id> --start <start_block> --end <end_block>
```

Ex. `cargo run --bin fetch_and_save_proof --release -- --request-id proofrequest_01j4ze00ftfjpbd4zkf250qwey --start 123812410 --end 123812412`