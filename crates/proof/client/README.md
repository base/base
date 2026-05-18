# `base-proof-client`

Oracle-backed derivation and execution orchestration for the Base ZK proof program.

## Overview

This crate defines the three phases of a fault proof (or ZK proof) run: prologue, execution,
and epilogue. It is intentionally decoupled from any specific runtime or FPVM binary so that
the same logic can be reused across proof systems. The concrete EVM factory is injected by the
caller, which means the crate itself has no dependency on target-specific file descriptors,
precompile wiring, or hardware-specific oracle channels.

The prologue loads boot information through the preimage oracle, constructs a caching oracle
wrapper, initializes the L1 and L2 chain providers, and builds the full derivation pipeline
cursor. If the agreed and claimed output roots are equal the prologue returns a trace extension
error immediately so the caller can exit cleanly. Otherwise it hands off a fully initialized
`FaultProofDriver` ready to run.

The driver advances the derivation pipeline to the claimed L2 block number, builds a
`BaseExecutor` over the oracle-backed trie, and returns an `Epilogue` containing the safe head
block, the computed output root, and the claimed output root.

The epilogue performs the final comparison. If the two output roots match it logs the result
and returns `Ok(())`; if they differ it returns an `InvalidClaim` error carrying both values
so the caller can surface them in the proof system's exit code or halt mechanism.

## Usage

Callers provide three things:

1. A `PreimageOracleClient` implementation.
2. A `HintWriterClient` implementation.
3. An `EvmFactory` implementation for the target runtime.

With those pieces in place, the orchestration flow is:

```rust,ignore
use base_proof_client::Prologue;

async fn run<P, H, F>(
    oracle_client: P,
    hint_writer: H,
    evm_factory: F,
) -> Result<(), base_proof_client::FaultProofProgramError>
{
    let prologue = Prologue::new(oracle_client, hint_writer, evm_factory);
    let driver = prologue.load().await?;
    let epilogue = driver.execute().await?;
    epilogue.validate()?;
    Ok(())
}
```

The three phases map to the three types exported by this crate.

`Prologue::load` reads `BootInfo` from the oracle, detects trace extension, resolves the safe
head, and constructs the pipeline. It returns a `FaultProofDriver` on success.

`FaultProofDriver::execute` drives the pipeline to `claimed_l2_block_number`, executing each
derived payload through the EVM, and returns an `Epilogue` on success.

`Epilogue::validate` compares `output_root` against `claimed_output_root`. A mismatch surfaces
as `FaultProofProgramError::InvalidClaim { computed, claimed }` which the caller should
translate into a non-zero exit code or a MIPS `halt` instruction depending on the target.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
