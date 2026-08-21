//! Hinted `CertManager` contract bindings.
//!
//! Cache writes accept caller-supplied P-384 inverse hints. Hint generation and
//! registration orchestration intentionally remain outside this client.

use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::RootProvider;
use alloy_sol_types::{SolCall, SolError, sol};
use async_trait::async_trait;

use crate::ContractError;

sol! {
    /// Raw metadata stored for a verified certificate.
    struct CertManagerVerifiedCert {
        bool ca;
        uint64 notAfter;
        int64 maxPathLen;
        bytes32 subjectHash;
        bytes pubKey;
    }

    /// Registrar subset of the active hinted `CertManager` interface.
    #[sol(rpc)]
    interface ICertManager {
        /// Thrown when an owner-only operation is called by another account.
        error NotOwner();

        /// Thrown when a non-root revocation is called by another account.
        error NotRevoker();

        /// Returns the account that controls owner-only operations.
        function owner() external view returns (address);

        /// Returns the account allowed to revoke non-root certificates.
        function revoker() external view returns (address);

        /// Verifies and caches a CA certificate using caller-supplied inverse hints.
        function verifyCACertWithHints(
            bytes calldata cert,
            bytes32 parentCertHash,
            bytes calldata signatureHints
        ) external returns (bytes32 certHash);

        /// Verifies and caches a leaf certificate using caller-supplied inverse hints.
        function verifyClientCertWithHints(
            bytes calldata cert,
            bytes32 parentCertHash,
            bytes calldata signatureHints
        ) external returns (CertManagerVerifiedCert memory verifiedCert);

        /// Returns raw cached metadata, which may be expired or revoked.
        function loadVerified(bytes32 certHash)
            external
            view
            returns (CertManagerVerifiedCert memory cert);

        /// Returns whether a certificate issuer/serial identity is revoked.
        function isRevoked(bytes32 certId) external view returns (bool);

        /// Computes the stable issuer/serial identity for a DER certificate.
        function computeCertId(bytes calldata cert) external pure returns (bytes32 certId);

        /// Revokes a certificate issuer/serial identity.
        function revokeCert(bytes32 certId) external;
    }
}

/// Ergonomic Rust representation of a raw `CertManager` cache entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedCert {
    /// Whether the cached certificate is a CA certificate.
    pub ca: bool,
    /// X.509 `notAfter` timestamp in Unix seconds.
    pub not_after: u64,
    /// Remaining CA path length, or `-1` when unconstrained.
    pub max_path_len: i64,
    /// Keccak hash of the certificate subject.
    pub subject_hash: B256,
    /// Raw P-384 public key bytes.
    pub public_key: Bytes,
}

impl From<CertManagerVerifiedCert> for VerifiedCert {
    fn from(cert: CertManagerVerifiedCert) -> Self {
        Self {
            ca: cert.ca,
            not_after: cert.notAfter,
            max_path_len: cert.maxPathLen,
            subject_hash: cert.subjectHash,
            public_key: cert.pubKey,
        }
    }
}

/// `CertManager` authorization failure decoded from raw revert data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CertManagerAuthorizationError {
    /// An owner-only operation was attempted by another account.
    NotOwner,
    /// A revoker-only operation was attempted by another account.
    NotRevoker,
}

/// Returns the 4-byte selector for `NotOwner()`.
pub const fn cert_manager_not_owner_selector() -> [u8; 4] {
    ICertManager::NotOwner::SELECTOR
}

/// Returns the 4-byte selector for `NotRevoker()`.
pub const fn cert_manager_not_revoker_selector() -> [u8; 4] {
    ICertManager::NotRevoker::SELECTOR
}

/// Classifies a `CertManager` authorization failure from raw revert data.
pub fn decode_cert_manager_authorization_error(
    revert_data: &[u8],
) -> Option<CertManagerAuthorizationError> {
    let selector = revert_data.get(..4)?;
    if selector == cert_manager_not_owner_selector() {
        Some(CertManagerAuthorizationError::NotOwner)
    } else if selector == cert_manager_not_revoker_selector() {
        Some(CertManagerAuthorizationError::NotRevoker)
    } else {
        None
    }
}

/// Reads cache, revocation, identity, and role data from `CertManager`.
#[async_trait]
pub trait CertManagerClient: Send + Sync + std::fmt::Debug {
    /// Returns the `CertManager` address this client is bound to.
    fn address(&self) -> Address;

    /// Verifies a CA certificate with supplied hints using `eth_call`.
    async fn verify_ca_cert_with_hints(
        &self,
        cert: Bytes,
        parent_cert_hash: B256,
        signature_hints: Bytes,
    ) -> Result<B256, ContractError>;

    /// Verifies a client certificate with supplied hints using `eth_call`.
    async fn verify_client_cert_with_hints(
        &self,
        cert: Bytes,
        parent_cert_hash: B256,
        signature_hints: Bytes,
    ) -> Result<VerifiedCert, ContractError>;

    /// Returns raw cached metadata, which may be expired or revoked.
    async fn load_verified(&self, cert_hash: B256) -> Result<VerifiedCert, ContractError>;

    /// Returns whether a certificate issuer/serial identity is revoked.
    async fn is_revoked(&self, cert_id: B256) -> Result<bool, ContractError>;

    /// Returns the owner account.
    async fn owner(&self) -> Result<Address, ContractError>;

    /// Returns the non-root certificate revoker account.
    async fn revoker(&self) -> Result<Address, ContractError>;

    /// Computes the onchain issuer/serial identity for a DER certificate.
    async fn compute_cert_id(&self, cert: Bytes) -> Result<B256, ContractError>;
}

/// Concrete implementation backed by Alloy's sol-generated contract bindings.
#[derive(Debug)]
pub struct CertManagerContractClient {
    contract: ICertManager::ICertManagerInstance<RootProvider>,
}

impl CertManagerContractClient {
    /// Creates a client for the `CertManager` returned by `NitroValidator.certManager()`.
    pub fn new(address: Address, l1_rpc_url: url::Url) -> Self {
        let provider = RootProvider::new_http(l1_rpc_url);
        let contract = ICertManager::ICertManagerInstance::new(address, provider);
        Self { contract }
    }
}

#[async_trait]
impl CertManagerClient for CertManagerContractClient {
    fn address(&self) -> Address {
        *self.contract.address()
    }

    async fn verify_ca_cert_with_hints(
        &self,
        cert: Bytes,
        parent_cert_hash: B256,
        signature_hints: Bytes,
    ) -> Result<B256, ContractError> {
        contract_call!(
            self.contract.verifyCACertWithHints(cert, parent_cert_hash, signature_hints).call(),
            "verifyCACertWithHints(bytes,bytes32,bytes)"
        )
    }

    async fn verify_client_cert_with_hints(
        &self,
        cert: Bytes,
        parent_cert_hash: B256,
        signature_hints: Bytes,
    ) -> Result<VerifiedCert, ContractError> {
        let cert = contract_call!(
            self.contract.verifyClientCertWithHints(cert, parent_cert_hash, signature_hints).call(),
            "verifyClientCertWithHints(bytes,bytes32,bytes)"
        )?;
        Ok(cert.into())
    }

    async fn load_verified(&self, cert_hash: B256) -> Result<VerifiedCert, ContractError> {
        let cert = contract_call!(
            self.contract.loadVerified(cert_hash).call(),
            format!("loadVerified({cert_hash})")
        )?;
        Ok(cert.into())
    }

    async fn is_revoked(&self, cert_id: B256) -> Result<bool, ContractError> {
        contract_call!(self.contract.isRevoked(cert_id).call(), format!("isRevoked({cert_id})"))
    }

    async fn owner(&self) -> Result<Address, ContractError> {
        contract_call!(self.contract.owner().call(), "owner()")
    }

    async fn revoker(&self) -> Result<Address, ContractError> {
        contract_call!(self.contract.revoker().call(), "revoker()")
    }

    async fn compute_cert_id(&self, cert: Bytes) -> Result<B256, ContractError> {
        contract_call!(self.contract.computeCertId(cert).call(), "computeCertId(bytes)")
    }
}

/// Encodes a permissionless CA cache transaction using supplied signature hints.
pub fn encode_verify_ca_cert_with_hints_calldata(
    cert: Bytes,
    parent_cert_hash: B256,
    signature_hints: Bytes,
) -> Bytes {
    Bytes::from(
        ICertManager::verifyCACertWithHintsCall {
            cert,
            parentCertHash: parent_cert_hash,
            signatureHints: signature_hints,
        }
        .abi_encode(),
    )
}

/// Encodes a permissionless leaf cache transaction using supplied signature hints.
pub fn encode_verify_client_cert_with_hints_calldata(
    cert: Bytes,
    parent_cert_hash: B256,
    signature_hints: Bytes,
) -> Bytes {
    Bytes::from(
        ICertManager::verifyClientCertWithHintsCall {
            cert,
            parentCertHash: parent_cert_hash,
            signatureHints: signature_hints,
        }
        .abi_encode(),
    )
}

/// Encodes a revocation transaction for an issuer/serial certificate identity.
pub fn encode_revoke_cert_calldata(cert_id: B256) -> Bytes {
    Bytes::from(ICertManager::revokeCertCall { certId: cert_id }.abi_encode())
}

#[cfg(test)]
mod tests {
    use alloy_sol_types::SolCall as _;

    use super::*;

    #[test]
    fn cert_manager_selectors_match_final_contract_abi() {
        assert_eq!(ICertManager::verifyCACertWithHintsCall::SELECTOR, [0x03, 0x1c, 0xf0, 0x67]);
        assert_eq!(ICertManager::verifyClientCertWithHintsCall::SELECTOR, [0x77, 0xf2, 0xdd, 0x74]);
        assert_eq!(ICertManager::loadVerifiedCall::SELECTOR, [0x9c, 0x32, 0x56, 0xed]);
        assert_eq!(ICertManager::isRevokedCall::SELECTOR, [0x42, 0x94, 0x85, 0x7f]);
        assert_eq!(ICertManager::computeCertIdCall::SELECTOR, [0xad, 0x1f, 0xd8, 0xbd]);
        assert_eq!(ICertManager::revokeCertCall::SELECTOR, [0x8c, 0xb5, 0x0c, 0x44]);
        assert_eq!(ICertManager::ownerCall::SELECTOR, [0x8d, 0xa5, 0xcb, 0x5b]);
        assert_eq!(ICertManager::revokerCall::SELECTOR, [0x35, 0xde, 0xc5, 0xd1]);
    }

    #[test]
    fn cache_transaction_calldata_roundtrips() {
        let cert = Bytes::from_static(b"certificate-der");
        let parent_cert_hash = B256::repeat_byte(0x11);
        let signature_hints = Bytes::from(vec![0x22; 96]);

        let ca_calldata = encode_verify_ca_cert_with_hints_calldata(
            cert.clone(),
            parent_cert_hash,
            signature_hints.clone(),
        );
        let ca = ICertManager::verifyCACertWithHintsCall::abi_decode(&ca_calldata)
            .expect("CA cache calldata should decode");
        assert_eq!(ca.cert, cert);
        assert_eq!(ca.parentCertHash, parent_cert_hash);
        assert_eq!(ca.signatureHints, signature_hints);

        let leaf_calldata = encode_verify_client_cert_with_hints_calldata(
            ca.cert.clone(),
            ca.parentCertHash,
            ca.signatureHints.clone(),
        );
        let leaf = ICertManager::verifyClientCertWithHintsCall::abi_decode(&leaf_calldata)
            .expect("leaf cache calldata should decode");
        assert_eq!(leaf.cert, ca.cert);
        assert_eq!(leaf.parentCertHash, ca.parentCertHash);
        assert_eq!(leaf.signatureHints, ca.signatureHints);
    }

    #[test]
    fn cached_metadata_decodes_all_fields() {
        let raw = CertManagerVerifiedCert {
            ca: true,
            notAfter: 2_519_044_085,
            maxPathLen: -1,
            subjectHash: B256::repeat_byte(0x33),
            pubKey: Bytes::from(vec![0x44; 96]),
        };
        let encoded = ICertManager::loadVerifiedCall::abi_encode_returns_tuple(&(raw,));
        let decoded = ICertManager::loadVerifiedCall::abi_decode_returns(&encoded)
            .expect("cache metadata should decode");
        let cert = VerifiedCert::from(decoded);

        assert!(cert.ca);
        assert_eq!(cert.not_after, 2_519_044_085);
        assert_eq!(cert.max_path_len, -1);
        assert_eq!(cert.subject_hash, B256::repeat_byte(0x33));
        assert_eq!(cert.public_key, Bytes::from(vec![0x44; 96]));
    }

    #[test]
    fn warm_call_returns_decode() {
        let cert_hash = B256::repeat_byte(0x55);
        let ca_encoded = ICertManager::verifyCACertWithHintsCall::abi_encode_returns(&cert_hash);
        let ca_decoded = ICertManager::verifyCACertWithHintsCall::abi_decode_returns(&ca_encoded)
            .expect("CA verification return should decode");
        assert_eq!(ca_decoded, cert_hash);

        let raw = CertManagerVerifiedCert {
            ca: false,
            notAfter: 2_519_044_085,
            maxPathLen: 0,
            subjectHash: B256::repeat_byte(0x66),
            pubKey: Bytes::from(vec![0x77; 96]),
        };
        let client_encoded = ICertManager::verifyClientCertWithHintsCall::abi_encode_returns(&raw);
        let client_decoded =
            ICertManager::verifyClientCertWithHintsCall::abi_decode_returns(&client_encoded)
                .expect("client verification return should decode");

        assert_eq!(
            VerifiedCert::from(client_decoded),
            VerifiedCert {
                ca: false,
                not_after: 2_519_044_085,
                max_path_len: 0,
                subject_hash: B256::repeat_byte(0x66),
                public_key: Bytes::from(vec![0x77; 96]),
            }
        );
    }

    #[test]
    fn revocation_calldata_uses_issuer_serial_identity() {
        let cert_id = B256::repeat_byte(0x55);
        let calldata = encode_revoke_cert_calldata(cert_id);
        let decoded = ICertManager::revokeCertCall::abi_decode(&calldata)
            .expect("revokeCert calldata should decode");
        assert_eq!(decoded.certId, cert_id);
    }

    #[test]
    fn authorization_errors_are_classified_separately() {
        assert_eq!(cert_manager_not_owner_selector(), [0x30, 0xcd, 0x74, 0x71]);
        assert_eq!(cert_manager_not_revoker_selector(), [0x2a, 0xd3, 0xd4, 0x4f]);
        assert_eq!(
            decode_cert_manager_authorization_error(&cert_manager_not_owner_selector()),
            Some(CertManagerAuthorizationError::NotOwner)
        );
        assert_eq!(
            decode_cert_manager_authorization_error(&cert_manager_not_revoker_selector()),
            Some(CertManagerAuthorizationError::NotRevoker)
        );
        assert_eq!(decode_cert_manager_authorization_error(&[0xde, 0xad, 0xbe, 0xef]), None);
        assert_eq!(decode_cert_manager_authorization_error(&[0x30, 0xcd]), None);
    }
}
