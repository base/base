#![doc = "Fault proof client entrypoint for the FPVM."]
#![no_std]
#![cfg_attr(any(target_arch = "mips64", target_arch = "riscv64"), no_main)]

extern crate alloc;

use alloc::string::String;

use base_proof_preimage::{HintWriter, OracleReader};
use base_proof_std_fpvm::{FileChannel, FileDescriptor};
use base_proof_std_fpvm_proc::client_entry;

/// The global preimage oracle reader pipe.
static ORACLE_READER_PIPE: FileChannel =
    FileChannel::new(FileDescriptor::PreimageRead, FileDescriptor::PreimageWrite);

/// The global hint writer pipe.
static HINT_WRITER_PIPE: FileChannel =
    FileChannel::new(FileDescriptor::HintRead, FileDescriptor::HintWrite);

/// The global preimage oracle reader.
static ORACLE_READER: OracleReader<FileChannel> = OracleReader::new(ORACLE_READER_PIPE);

/// The global hint writer.
static HINT_WRITER: HintWriter<FileChannel> = HintWriter::new(HINT_WRITER_PIPE);

#[client_entry]
fn main() -> Result<(), String> {
    #[cfg(feature = "client-tracing")]
    {
        use base_proof_std_fpvm::FpvmTracingSubscriber;

        let subscriber = FpvmTracingSubscriber::new(tracing::Level::INFO);
        tracing::subscriber::set_global_default(subscriber)
            .expect("Failed to set tracing subscriber");
    }

    base_proof::block_on(base_client::run(ORACLE_READER, HINT_WRITER))
}
