#![doc = include_str!("../README.md")]

mod error;
pub use error::CryptoError;

mod secp256k1;
pub use secp256k1::Secp256k1;

mod secp256r1;
pub use secp256r1::Secp256r1;
