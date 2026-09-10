//! Proof execution program.

mod error;
pub use error::FaultProofProgramError;

mod epilogue;
pub use epilogue::Epilogue;

mod prologue;
pub use prologue::Prologue;

mod driver;
pub use driver::FaultProofDriver;
