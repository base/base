# `base-proof-client`

Oracle-backed derivation and execution orchestration for the Base ZK proof program.

## Overview

This crate defines the three phases of a fault proof (or ZK proof) run: prologue, execution,
and epilogue. It is intentionally decoupled from any specific runtime or FPVM binary so that
the same logic can be reused across proof systems. The concrete EVM factory (including
FPVM-accelerated precompiles) is injected by the caller, which means the crate itself has no
dependency on FPVM file descriptors or hardware-specific oracle channels.

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

The entry point for a minimal binary looks exactly like the refactored `bin/client` that
introduced this crate. An `OracleReader` and `HintWriter` come from the FPVM file descriptors
provided by `base-proof-std-fpvm`, and a `FpvmOpEvmFactory` (from `base-proof-fpvm-precompiles`)
is constructed with the same channels so that accelerated precompiles can write hints and read
preimages during execution.

```rust,ignore
use base_proof::block_on;
use base_proof_client::Prologue;
use base_proof_fpvm_precompiles::FpvmOpEvmFactory;
use base_proof_preimage::{HintWriter, OracleReader};
use base_proof_std_fpvm::{FileChannel, FileDescriptor};

// FPVM file descriptors wired up by the host before executing the guest.
const ORACLE_READER: OracleReader<FileChannel> =
    OracleReader::new(FileChannel::from_raw_fds(FileDescriptor::PreimageRead, FileDescriptor::PreimageWrite));
const HINT_WRITER: HintWriter<FileChannel> =
    HintWriter::new(FileChannel::from_raw_fds(FileDescriptor::HintRead, FileDescriptor::HintWrite));

fn main() -> Result<(), alloc::string::String> {
    block_on(run())
}

async fn run() -> Result<(), alloc::string::String> {
    let evm_factory = FpvmOpEvmFactory::new(HINT_WRITER, ORACLE_READER);
    let prologue = Prologue::new(ORACLE_READER, HINT_WRITER, evm_factory);
    let driver = prologue
        .load()
        .await
        .map_err(|e| alloc::format!("{e}"))?;
    let epilogue = driver
        .execute()
        .await
        .map_err(|e| alloc::format!("{e}"))?;
    epilogue.validate().map_err(|e| alloc::format!("{e}"))?;
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
