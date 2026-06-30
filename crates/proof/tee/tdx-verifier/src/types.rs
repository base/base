//! Solidity-aligned types for the `ITDXVerifier` on-chain interface.
//!
//! Mirrors the TDX ABI surface staged in the contracts branch so offchain
//! verification code can encode and decode TDX attestation verifier journals.
//!
//! Enums put `Unknown` at discriminant 0 so uninitialized values fail closed.

use alloy_sol_types::sol;

sol! {
    #![sol(all_derives)]

    /// Supported zero-knowledge proof coprocessor types.
    ///
    /// Shared by the Nitro and TDX verifier contracts; ordering must match
    /// `INitroEnclaveVerifier.sol`.
    enum ZkCoProcessorType {
        /// Unknown / unset.
        Unknown,
        /// RISC Zero zkVM proving system.
        RiscZero,
        /// Succinct SP1 proving system.
        Succinct,
    }

    /// Configuration for a specific zero-knowledge coprocessor.
    struct ZkCoProcessorConfig {
        /// Latest program ID for single attestation verification.
        bytes32 verifierId;
        /// Latest program ID for batch/aggregated verification.
        bytes32 aggregatorId;
        /// Default ZK verifier contract address.
        address zkVerifier;
    }

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

    /// Public journal emitted by the off-chain/ZK TDX DCAP verifier.
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

    /// `TDXVerifier` contract interface.
    interface ITDXVerifier {
        /// Verifies a ZK proof of Intel TDX DCAP quote verification.
        function verify(
            bytes calldata output,
            ZkCoProcessorType zkCoprocessor,
            bytes calldata proofBytes
        )
            external
            returns (TDXVerifierJournal memory journal);

        /// Retrieves the configuration for a specific coprocessor.
        function getZkConfig(ZkCoProcessorType zkCoprocessor)
            external
            view
            returns (ZkCoProcessorConfig memory);

        /// Returns whether a TCB status is accepted by verifier policy.
        function allowedTcbStatuses(TDXTcbStatus status) external view returns (bool);
    }

}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes};
    use alloy_sol_types::{SolCall, SolValue};
    use rstest::rstest;

    use super::*;

    #[test]
    fn tdx_verifier_journal_abi_round_trips() {
        let journal = TDXVerifierJournal {
            result: TDXVerificationResult::Success,
            tcbStatus: TDXTcbStatus::UpToDate,
            timestamp: 1_711_111_111_000,
            collateralExpiration: 1_711_111_111,
            rootCaHash: B256::repeat_byte(0x01),
            pckCertHash: B256::repeat_byte(0x02),
            tcbInfoHash: B256::repeat_byte(0x03),
            qeIdentityHash: B256::repeat_byte(0x04),
            publicKey: Bytes::from(vec![0x04; 65]),
            signer: Address::repeat_byte(0x11),
            imageHash: B256::repeat_byte(0x05),
            mrTdHash: B256::repeat_byte(0x06),
            reportDataPrefix: B256::repeat_byte(0x07),
            reportDataSuffix: B256::repeat_byte(0x08),
        };

        let encoded = SolValue::abi_encode(&journal);
        let decoded = <TDXVerifierJournal as SolValue>::abi_decode_validate(&encoded)
            .expect("TDX verifier journal ABI must decode");

        assert_eq!(decoded, journal);
    }

    #[rstest]
    #[case(TDXVerificationResult::Unknown, 0)]
    #[case(TDXVerificationResult::Success, 1)]
    #[case(TDXVerificationResult::InvalidQuote, 2)]
    #[case(TDXVerificationResult::QuoteSignatureInvalid, 3)]
    #[case(TDXVerificationResult::RootCaNotTrusted, 4)]
    #[case(TDXVerificationResult::PckCertChainInvalid, 5)]
    #[case(TDXVerificationResult::TcbInfoInvalid, 6)]
    #[case(TDXVerificationResult::QeIdentityInvalid, 7)]
    #[case(TDXVerificationResult::TcbStatusNotAllowed, 8)]
    #[case(TDXVerificationResult::CollateralExpired, 9)]
    #[case(TDXVerificationResult::InvalidTimestamp, 10)]
    #[case(TDXVerificationResult::ReportDataMismatch, 11)]
    fn tdx_verification_result_discriminants_match_solidity(
        #[case] result: TDXVerificationResult,
        #[case] expected: u8,
    ) {
        assert_eq!(result as u8, expected);
    }

    #[rstest]
    #[case(TDXTcbStatus::Unknown, 0)]
    #[case(TDXTcbStatus::UpToDate, 1)]
    #[case(TDXTcbStatus::SwHardeningNeeded, 2)]
    #[case(TDXTcbStatus::ConfigurationNeeded, 3)]
    #[case(TDXTcbStatus::ConfigurationAndSwHardeningNeeded, 4)]
    #[case(TDXTcbStatus::OutOfDate, 5)]
    #[case(TDXTcbStatus::OutOfDateConfigurationNeeded, 6)]
    #[case(TDXTcbStatus::Revoked, 7)]
    fn tdx_tcb_status_discriminants_match_solidity(
        #[case] status: TDXTcbStatus,
        #[case] expected: u8,
    ) {
        assert_eq!(status as u8, expected);
    }

    #[test]
    fn get_zk_config_and_allowed_tcb_status_abi_encode_correctly() {
        let get_zk_config =
            ITDXVerifier::getZkConfigCall { zkCoprocessor: ZkCoProcessorType::Succinct };
        let allowed_tcb_status =
            ITDXVerifier::allowedTcbStatusesCall { status: TDXTcbStatus::UpToDate };

        assert_eq!(get_zk_config.abi_encode().len(), 4 + 32);
        assert_eq!(allowed_tcb_status.abi_encode().len(), 4 + 32);
    }

    #[test]
    fn verify_abi_encodes_correctly() {
        let call = ITDXVerifier::verifyCall {
            output: Bytes::new(),
            zkCoprocessor: ZkCoProcessorType::Succinct,
            proofBytes: Bytes::new(),
        };
        let encoded = call.abi_encode();

        assert_eq!(&encoded[..4], &ITDXVerifier::verifyCall::SELECTOR);
    }

    #[test]
    fn zk_coprocessor_type_discriminants_match_solidity() {
        assert_eq!(ZkCoProcessorType::Unknown as u8, 0);
        assert_eq!(ZkCoProcessorType::RiscZero as u8, 1);
        assert_eq!(ZkCoProcessorType::Succinct as u8, 2);
    }

    #[test]
    fn zk_coprocessor_config_abi_round_trips() {
        let config = ZkCoProcessorConfig {
            verifierId: B256::repeat_byte(0x09),
            aggregatorId: B256::repeat_byte(0x10),
            zkVerifier: Address::ZERO,
        };

        let encoded = SolValue::abi_encode(&config);
        let decoded = <ZkCoProcessorConfig as SolValue>::abi_decode_validate(&encoded)
            .expect("ZK coprocessor config ABI must decode");

        assert_eq!(decoded, config);
    }
}
