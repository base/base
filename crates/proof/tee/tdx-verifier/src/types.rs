//! Solidity-aligned types for the TDX verifier onchain interface.
//!
//! Mirrors the TDX ABI surface staged in the contracts branch so offchain
//! verification code can encode and decode TDX attestation verifier journals.
//!
//! Enums put `Unknown` at discriminant 0 so uninitialized values fail closed.

use alloy_sol_types::sol;

sol! {
    #![sol(extra_derives(Debug))]

    /// Statuses emitted by the TDX quote/collateral verifier.
    enum TDXVerificationResult {
        /// Unknown / unset.
        Unknown,
        /// TDX quote and collateral verification succeeded.
        Success,
        /// Quote parsing or structural validation failed.
        InvalidQuote,
        /// Quote signature validation failed.
        QuoteSignatureInvalid,
        /// Intel root CA was not trusted.
        RootCaNotTrusted,
        /// PCK certificate chain validation failed.
        PckCertChainInvalid,
        /// TCB info collateral validation failed.
        TcbInfoInvalid,
        /// QE identity collateral validation failed.
        QeIdentityInvalid,
        /// TCB status was not accepted by verifier policy.
        TcbStatusNotAllowed,
        /// Required quote collateral had expired.
        CollateralExpired,
        /// Quote timestamp was outside the configured policy window.
        InvalidTimestamp,
        /// TD report data did not match the expected signer binding.
        ReportDataMismatch,
    }

    /// Intel TDX TCB status reduced to the contract policy statuses.
    enum TDXTcbStatus {
        /// Unknown / unset.
        Unknown,
        /// Platform TCB is up to date.
        UpToDate,
        /// Platform needs software hardening.
        SwHardeningNeeded,
        /// Platform needs configuration hardening.
        ConfigurationNeeded,
        /// Platform needs configuration and software hardening.
        ConfigurationAndSwHardeningNeeded,
        /// Platform TCB is out of date.
        OutOfDate,
        /// Platform TCB is out of date and needs configuration hardening.
        OutOfDateConfigurationNeeded,
        /// Platform TCB has been revoked.
        Revoked,
    }

    /// Public journal emitted by the offchain/ZK TDX DCAP verifier.
    struct TDXVerifierJournal {
        /// Overall verification result after quote and collateral validation.
        TDXVerificationResult result;
        /// Intel TDX TCB status for the platform.
        TDXTcbStatus tcbStatus;
        /// Quote timestamp in milliseconds since Unix epoch.
        uint64 timestamp;
        /// Earliest expiration timestamp in seconds across accepted collateral.
        uint64 collateralExpiration;
        /// Hash of the Intel root CA used for validation.
        bytes32 rootCaHash;
        /// Hash of the PCK leaf certificate.
        bytes32 pckCertHash;
        /// Hash of the TCB info collateral.
        bytes32 tcbInfoHash;
        /// Hash of the QE identity collateral.
        bytes32 qeIdentityHash;
        /// Uncompressed secp256k1 public key: `0x04 || x || y`.
        bytes publicKey;
        /// Ethereum address derived from `publicKey`.
        address signer;
        /// Multiproof-compatible image hash derived from MRTD and RTMR0-3.
        bytes32 imageHash;
        /// Keccak256 hash of the MRTD measurement.
        bytes32 mrTdHash;
        /// First 32 bytes of `TDREPORT.REPORTDATA`.
        bytes32 reportDataPrefix;
        /// Last 32 bytes of `TDREPORT.REPORTDATA`.
        bytes32 reportDataSuffix;
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminants_match_solidity() {
        for (actual, expected) in [
            (TDXVerificationResult::Unknown as u8, 0),
            (TDXVerificationResult::Success as u8, 1),
            (TDXVerificationResult::InvalidQuote as u8, 2),
            (TDXVerificationResult::QuoteSignatureInvalid as u8, 3),
            (TDXVerificationResult::RootCaNotTrusted as u8, 4),
            (TDXVerificationResult::PckCertChainInvalid as u8, 5),
            (TDXVerificationResult::TcbInfoInvalid as u8, 6),
            (TDXVerificationResult::QeIdentityInvalid as u8, 7),
            (TDXVerificationResult::TcbStatusNotAllowed as u8, 8),
            (TDXVerificationResult::CollateralExpired as u8, 9),
            (TDXVerificationResult::InvalidTimestamp as u8, 10),
            (TDXVerificationResult::ReportDataMismatch as u8, 11),
            (TDXTcbStatus::Unknown as u8, 0),
            (TDXTcbStatus::UpToDate as u8, 1),
            (TDXTcbStatus::SwHardeningNeeded as u8, 2),
            (TDXTcbStatus::ConfigurationNeeded as u8, 3),
            (TDXTcbStatus::ConfigurationAndSwHardeningNeeded as u8, 4),
            (TDXTcbStatus::OutOfDate as u8, 5),
            (TDXTcbStatus::OutOfDateConfigurationNeeded as u8, 6),
            (TDXTcbStatus::Revoked as u8, 7),
        ] {
            assert_eq!(actual, expected);
        }
    }
}
