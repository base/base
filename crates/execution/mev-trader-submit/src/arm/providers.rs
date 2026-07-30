//! Production, keyless authorities for arm freshness inputs.

use std::{
    fs::File,
    io::{self, Read},
};

#[cfg(test)]
use std::path::Path;

use alloy_primitives::{Address, B256, keccak256};
use base_mev_trader::{ArmedCriteria, DrawdownInput, StoreIdentity, VictimClaimStore};
use sha2::{Digest, Sha256};

use super::{
    proofs::{CodeHashProvider, DeploymentEvidence, ProviderError},
    witness::{
        ArmRuntime, ArmRuntimeOpenError, DeploymentIdentity, DeploymentIdentitySource,
        DrawdownSource, FreshnessSources,
    },
};

/// Maximum runtime bytecode accepted from the committed-state authority.
pub const MAX_RUNTIME_CODE_BYTES: usize = 24_576;
/// Maximum process image size hashed during installation.
pub const MAX_PROCESS_IMAGE_BYTES: u64 = 512 * 1024 * 1024;

/// Node-local authority for canonical committed account code and head height.
///
/// Implementations must read the canonical database directly. Network/RPC implementations are
/// outside this contract and would violate the arm single-egress seal.
pub trait CommittedStateAuthority {
    /// Returns runtime bytecode at the latest committed canonical head.
    fn code_at_latest_committed(&self, address: Address) -> Result<Vec<u8>, ProviderError>;

    /// Returns the latest committed canonical block number.
    fn latest_committed_block(&self) -> Result<u64, ProviderError>;

    /// Returns the native balance when the account is present at the latest committed head.
    ///
    /// Account absence is distinct from a present account with zero balance.
    fn native_balance_at_latest_committed(
        &self,
        address: Address,
    ) -> Result<Option<alloy_primitives::U256>, ProviderError>;
}

impl<A: CommittedStateAuthority + ?Sized> CommittedStateAuthority for std::sync::Arc<A> {
    fn code_at_latest_committed(&self, address: Address) -> Result<Vec<u8>, ProviderError> {
        (**self).code_at_latest_committed(address)
    }

    fn latest_committed_block(&self) -> Result<u64, ProviderError> {
        (**self).latest_committed_block()
    }

    fn native_balance_at_latest_committed(
        &self,
        address: Address,
    ) -> Result<Option<alloy_primitives::U256>, ProviderError> {
        (**self).native_balance_at_latest_committed(address)
    }
}

/// Production adapter from the node's committed-state authority to arm code-hash freshness.
#[derive(Debug)]
pub struct ProductionCodeHashProvider<A> {
    authority: A,
}

impl<A> ProductionCodeHashProvider<A> {
    /// Installs an explicit node-local committed-state authority.
    pub const fn install(authority: A) -> Self {
        Self { authority }
    }
}

impl<A: CommittedStateAuthority> CodeHashProvider for ProductionCodeHashProvider<A> {
    fn code_hash_at_latest_committed(&self, address: Address) -> Result<B256, ProviderError> {
        let code = self.authority.code_at_latest_committed(address)?;
        if code.is_empty() {
            return Err(ProviderError::Invalid("committed account has no runtime code"));
        }
        if code.len() > MAX_RUNTIME_CODE_BYTES {
            return Err(ProviderError::TooLarge {
                subject: "committed runtime code",
                limit: MAX_RUNTIME_CODE_BYTES as u64,
                actual: code.len() as u64,
            });
        }
        Ok(keccak256(code))
    }

    fn current_block(&self) -> Result<u64, ProviderError> {
        self.authority.latest_committed_block()
    }

    fn native_balance_at_latest_committed(
        &self,
        address: Address,
    ) -> Result<Option<alloy_primitives::U256>, ProviderError> {
        self.authority.native_balance_at_latest_committed(address)
    }
}

/// Authoritative settled-loss projection used by the production drawdown adapter.
pub trait DrawdownAuthority {
    /// Loads the latest complete drawdown projection.
    fn load_drawdown(&self) -> Result<DrawdownInput, ProviderError>;
}

/// Production drawdown adapter. Authority errors are deliberately reduced to
/// [`DrawdownInput::Error`], which closes `submit_gate`.
#[derive(Debug)]
pub struct ProductionDrawdownSource<A> {
    authority: A,
}

impl<A> ProductionDrawdownSource<A> {
    /// Installs an explicit realized-loss authority.
    pub const fn install(authority: A) -> Self {
        Self { authority }
    }
}

impl<A: DrawdownAuthority> DrawdownSource for ProductionDrawdownSource<A> {
    fn load(&self) -> DrawdownInput {
        self.authority.load_drawdown().unwrap_or(DrawdownInput::Error)
    }
}

/// Deterministic installation failure for process/store deployment identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentIdentityError {
    /// The process image could not be opened or read.
    ProcessImageIo { operation: &'static str, kind: io::ErrorKind },
    /// The process image exceeds the installation bound.
    ProcessImageTooLarge { limit: u64, actual: u64 },
    /// The measured process image differs from the owner-attested digest.
    BinaryDigestMismatch { expected: B256, actual: B256 },
    /// The opened R9 store differs from the owner-attested store identity.
    StoreIdentityMismatch { expected: StoreIdentity, actual: StoreIdentity },
}

impl core::fmt::Display for DeploymentIdentityError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ProcessImageIo { operation, kind } => {
                write!(formatter, "process image {operation} failed: {kind:?}")
            }
            Self::ProcessImageTooLarge { limit, actual } => {
                write!(formatter, "process image exceeds {limit} bytes: {actual}")
            }
            Self::BinaryDigestMismatch { .. } => write!(formatter, "process image digest mismatch"),
            Self::StoreIdentityMismatch { .. } => write!(formatter, "R9 store identity mismatch"),
        }
    }
}

impl core::error::Error for DeploymentIdentityError {}

/// SHA-256 identity of the immutable image backing the running process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessBinaryIdentity(B256);

impl ProcessBinaryIdentity {
    /// Opens `/proc/self/exe` once and hashes that opened image with bounded streaming I/O.
    pub fn install() -> Result<Self, DeploymentIdentityError> {
        let file = File::open("/proc/self/exe").map_err(|error| {
            DeploymentIdentityError::ProcessImageIo { operation: "open", kind: error.kind() }
        })?;
        Self::from_open_file(file)
    }

    #[cfg(test)]
    fn from_path(path: &Path) -> Result<Self, DeploymentIdentityError> {
        let file = File::open(path).map_err(|error| DeploymentIdentityError::ProcessImageIo {
            operation: "open",
            kind: error.kind(),
        })?;
        Self::from_open_file(file)
    }

    fn from_open_file(mut file: File) -> Result<Self, DeploymentIdentityError> {
        let metadata = file.metadata().map_err(|error| {
            DeploymentIdentityError::ProcessImageIo { operation: "metadata", kind: error.kind() }
        })?;
        if metadata.len() > MAX_PROCESS_IMAGE_BYTES {
            return Err(DeploymentIdentityError::ProcessImageTooLarge {
                limit: MAX_PROCESS_IMAGE_BYTES,
                actual: metadata.len(),
            });
        }

        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let read = file.read(&mut buffer).map_err(|error| {
                DeploymentIdentityError::ProcessImageIo { operation: "read", kind: error.kind() }
            })?;
            if read == 0 {
                break;
            }
            total = total.checked_add(read as u64).ok_or(
                DeploymentIdentityError::ProcessImageTooLarge {
                    limit: MAX_PROCESS_IMAGE_BYTES,
                    actual: u64::MAX,
                },
            )?;
            if total > MAX_PROCESS_IMAGE_BYTES {
                return Err(DeploymentIdentityError::ProcessImageTooLarge {
                    limit: MAX_PROCESS_IMAGE_BYTES,
                    actual: total,
                });
            }
            hasher.update(&buffer[..read]);
        }
        Ok(Self(B256::from_slice(&hasher.finalize())))
    }

    /// Returns the measured SHA-256 digest.
    pub const fn binary_digest(self) -> B256 {
        self.0
    }
}

/// Production deployment identity derived from verified owner evidence, the running process image,
/// and the already-open authoritative R9 claim store.
#[derive(Debug)]
pub struct ProductionDeploymentIdentitySource {
    identity: DeploymentIdentity,
}

impl ProductionDeploymentIdentitySource {
    /// Installs the source only when both independently produced identities match the signed
    /// deployment evidence. No file path or identity constant can be injected by a caller.
    pub fn install(
        evidence: &DeploymentEvidence,
        process: ProcessBinaryIdentity,
        claim_store: &VictimClaimStore,
    ) -> Result<Self, DeploymentIdentityError> {
        let actual_binary = process.binary_digest();
        if actual_binary != evidence.binary_digest() {
            return Err(DeploymentIdentityError::BinaryDigestMismatch {
                expected: evidence.binary_digest(),
                actual: actual_binary,
            });
        }
        let actual_store = claim_store.store_identity();
        if actual_store != evidence.r9_store_identity() {
            return Err(DeploymentIdentityError::StoreIdentityMismatch {
                expected: evidence.r9_store_identity(),
                actual: actual_store,
            });
        }
        Ok(Self {
            identity: DeploymentIdentity {
                binary_digest: actual_binary,
                deployment_digest: evidence.deployment_digest(),
                r9_store_identity: actual_store,
            },
        })
    }
}

impl DeploymentIdentitySource for ProductionDeploymentIdentitySource {
    fn current(&self) -> Option<DeploymentIdentity> {
        Some(self.identity)
    }
}

/// Fail-closed installation error for the production B5 freshness runtime.
#[derive(Debug)]
pub enum ProductionB5RuntimeInstallError {
    /// The running process image or open R9 store did not match signed deployment evidence.
    DeploymentIdentity(DeploymentIdentityError),
    /// The compile-pinned arm stores or kill-state anchor could not be opened.
    ArmRuntime(ArmRuntimeOpenError),
}

impl core::fmt::Display for ProductionB5RuntimeInstallError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::DeploymentIdentity(error) => {
                write!(formatter, "production deployment identity rejected: {error}")
            }
            Self::ArmRuntime(error) => {
                write!(formatter, "production arm runtime rejected: {error}")
            }
        }
    }
}

impl core::error::Error for ProductionB5RuntimeInstallError {}

/// Installed production owner of all three keyless B5 freshness providers.
#[derive(Debug)]
pub struct ProductionB5Runtime<C, D> {
    arm: ArmRuntime,
    code_hash: ProductionCodeHashProvider<C>,
    drawdown: ProductionDrawdownSource<D>,
    deployment_identity: ProductionDeploymentIdentitySource,
}

impl<C: CommittedStateAuthority, D: DrawdownAuthority> ProductionB5Runtime<C, D> {
    /// Installs only after the running image and open R9 store match signed deployment evidence.
    ///
    /// The process image is measured from `/proc/self/exe`; callers cannot inject a path or digest.
    /// Provider authorities remain node-local and keyless.
    pub fn install(
        evidence: &DeploymentEvidence,
        claim_store: &VictimClaimStore,
        committed_state: C,
        drawdown: D,
    ) -> Result<Self, ProductionB5RuntimeInstallError> {
        let process = ProcessBinaryIdentity::install()
            .map_err(ProductionB5RuntimeInstallError::DeploymentIdentity)?;
        let deployment_identity =
            ProductionDeploymentIdentitySource::install(evidence, process, claim_store)
                .map_err(ProductionB5RuntimeInstallError::DeploymentIdentity)?;
        let arm = ArmRuntime::open().map_err(ProductionB5RuntimeInstallError::ArmRuntime)?;
        Ok(Self {
            arm,
            code_hash: ProductionCodeHashProvider::install(committed_state),
            drawdown: ProductionDrawdownSource::install(drawdown),
            deployment_identity,
        })
    }

    /// Returns the committed-head provider used by T4e deadline and arm code-hash checks.
    pub const fn code_hash_provider(&self) -> &ProductionCodeHashProvider<C> {
        &self.code_hash
    }

    /// Builds one egress-moment freshness view from the installed production providers.
    pub fn freshness<'a>(&'a self, armed: &'a ArmedCriteria) -> FreshnessSources<'a> {
        self.arm.freshness(armed, &self.drawdown, &self.code_hash, &self.deployment_identity)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, fs};

    use alloy_primitives::U256;
    use base_mev_trader::{DrawdownInput, LossProvenance, VictimClaimConfig, VictimClaimStore};

    use super::*;
    use crate::arm::testkit as tk;

    #[derive(Debug)]
    struct StateAuthority {
        code: Result<Vec<u8>, ProviderError>,
        block: Result<u64, ProviderError>,
    }

    impl CommittedStateAuthority for StateAuthority {
        fn code_at_latest_committed(&self, _address: Address) -> Result<Vec<u8>, ProviderError> {
            self.code.clone()
        }

        fn latest_committed_block(&self) -> Result<u64, ProviderError> {
            self.block.clone()
        }

        fn native_balance_at_latest_committed(
            &self,
            _address: Address,
        ) -> Result<Option<U256>, ProviderError> {
            Ok(Some(U256::ZERO))
        }
    }

    #[derive(Debug)]
    struct HeadAuthority {
        result: Result<u64, ProviderError>,
        reads: Cell<u64>,
    }

    impl CommittedStateAuthority for HeadAuthority {
        fn code_at_latest_committed(&self, _address: Address) -> Result<Vec<u8>, ProviderError> {
            Err(ProviderError::Invalid("code read not expected"))
        }

        fn latest_committed_block(&self) -> Result<u64, ProviderError> {
            self.reads.set(self.reads.get() + 1);
            self.result.clone()
        }

        fn native_balance_at_latest_committed(
            &self,
            _address: Address,
        ) -> Result<Option<U256>, ProviderError> {
            Err(ProviderError::Invalid("balance read not expected"))
        }
    }

    #[derive(Debug)]
    struct BalanceAuthority {
        balance: Result<Option<U256>, ProviderError>,
    }

    impl CommittedStateAuthority for BalanceAuthority {
        fn code_at_latest_committed(&self, _address: Address) -> Result<Vec<u8>, ProviderError> {
            Err(ProviderError::Invalid("code read not expected"))
        }

        fn latest_committed_block(&self) -> Result<u64, ProviderError> {
            Err(ProviderError::Invalid("head read not expected"))
        }

        fn native_balance_at_latest_committed(
            &self,
            _address: Address,
        ) -> Result<Option<U256>, ProviderError> {
            self.balance.clone()
        }
    }

    fn consume_current_block(provider: &dyn CodeHashProvider) -> Result<u64, ProviderError> {
        provider.current_block()
    }

    #[test]
    fn production_current_block_forwards_each_committed_head_read_to_consumers() {
        let provider = ProductionCodeHashProvider::install(HeadAuthority {
            result: Ok(123),
            reads: Cell::new(0),
        });

        assert_eq!(consume_current_block(&provider), Ok(123));
        assert_eq!(consume_current_block(&provider), Ok(123));
        assert_eq!(provider.authority.reads.get(), 2);
    }

    #[test]
    fn production_current_block_preserves_unavailable_head_failure() {
        let error = ProviderError::Unavailable("canonical head unavailable".to_string());
        let provider = ProductionCodeHashProvider::install(HeadAuthority {
            result: Err(error.clone()),
            reads: Cell::new(0),
        });

        assert_eq!(consume_current_block(&provider), Err(error));
        assert_eq!(provider.authority.reads.get(), 1);
    }

    #[test]
    fn production_balance_preserves_present_zero_absence_and_error() {
        let zero =
            ProductionCodeHashProvider::install(BalanceAuthority { balance: Ok(Some(U256::ZERO)) });
        assert_eq!(
            zero.native_balance_at_latest_committed(crate::arm::custody::FUNDED_WALLET),
            Ok(Some(U256::ZERO))
        );

        let absent = ProductionCodeHashProvider::install(BalanceAuthority { balance: Ok(None) });
        assert_eq!(
            absent.native_balance_at_latest_committed(crate::arm::custody::FUNDED_WALLET),
            Ok(None)
        );

        let error = ProductionCodeHashProvider::install(BalanceAuthority {
            balance: Err(ProviderError::Unavailable("canonical state".to_owned())),
        });
        assert!(
            error.native_balance_at_latest_committed(crate::arm::custody::FUNDED_WALLET).is_err()
        );
    }

    #[test]
    fn production_code_hashes_committed_code_and_classifies_invalid_values() {
        let code = vec![0x60, 0x00, 0x56];
        let provider = ProductionCodeHashProvider::install(StateAuthority {
            code: Ok(code.clone()),
            block: Ok(123),
        });
        assert_eq!(provider.code_hash_at_latest_committed(tk::EXECUTOR), Ok(keccak256(code)));
        assert_eq!(provider.current_block(), Ok(123));

        let empty = ProductionCodeHashProvider::install(StateAuthority {
            code: Ok(Vec::new()),
            block: Ok(1),
        });
        assert!(matches!(
            empty.code_hash_at_latest_committed(tk::EXECUTOR),
            Err(ProviderError::Invalid(_))
        ));

        let oversized = ProductionCodeHashProvider::install(StateAuthority {
            code: Ok(vec![0; MAX_RUNTIME_CODE_BYTES + 1]),
            block: Ok(1),
        });
        assert!(matches!(
            oversized.code_hash_at_latest_committed(tk::EXECUTOR),
            Err(ProviderError::TooLarge { .. })
        ));

        let unavailable = ProductionCodeHashProvider::install(StateAuthority {
            code: Err(ProviderError::Unavailable("canonical state unavailable".to_string())),
            block: Err(ProviderError::Unavailable("canonical head unavailable".to_string())),
        });
        assert!(matches!(
            unavailable.code_hash_at_latest_committed(tk::EXECUTOR),
            Err(ProviderError::Unavailable(_))
        ));
        assert!(matches!(unavailable.current_block(), Err(ProviderError::Unavailable(_))));
    }

    #[derive(Debug)]
    struct Drawdown {
        fail: bool,
        calls: Cell<u64>,
    }

    impl DrawdownAuthority for Drawdown {
        fn load_drawdown(&self) -> Result<DrawdownInput, ProviderError> {
            self.calls.set(self.calls.get() + 1);
            if self.fail {
                Err(ProviderError::Unavailable("projection unavailable".to_string()))
            } else {
                Ok(DrawdownInput::Complete {
                    cumulative_realized_loss_wei: U256::from(7),
                    provenance: LossProvenance::OnchainRealized,
                })
            }
        }
    }

    #[test]
    fn production_drawdown_reads_each_time_and_fails_closed() {
        let good = ProductionDrawdownSource::install(Drawdown { fail: false, calls: Cell::new(0) });
        assert!(matches!(good.load(), DrawdownInput::Complete { .. }));
        assert!(matches!(good.load(), DrawdownInput::Complete { .. }));
        assert_eq!(good.authority.calls.get(), 2);

        let failed =
            ProductionDrawdownSource::install(Drawdown { fail: true, calls: Cell::new(0) });
        assert_eq!(failed.load(), DrawdownInput::Error);
    }

    #[test]
    fn process_binary_digest_is_sha256_and_io_failures_are_classified() {
        let dir = tk::TempDir::new("binary-identity");
        let image = dir.path.join("image");
        fs::write(&image, b"immutable image").expect("write image");
        let identity = ProcessBinaryIdentity::from_path(&image).expect("hash image");
        assert_eq!(identity.binary_digest(), B256::from_slice(&Sha256::digest(b"immutable image")));

        let missing = ProcessBinaryIdentity::from_path(&dir.path.join("missing"));
        assert!(matches!(
            missing,
            Err(DeploymentIdentityError::ProcessImageIo {
                operation: "open",
                kind: io::ErrorKind::NotFound
            })
        ));
        let unreadable = ProcessBinaryIdentity::from_path(&dir.path);
        assert!(matches!(
            unreadable,
            Err(DeploymentIdentityError::ProcessImageIo { operation: "read", .. })
        ));

        let oversized = dir.path.join("oversized");
        let oversized_file = File::create(&oversized).expect("create sparse image");
        oversized_file.set_len(MAX_PROCESS_IMAGE_BYTES + 1).expect("size sparse image");
        assert!(matches!(
            ProcessBinaryIdentity::from_path(&oversized),
            Err(DeploymentIdentityError::ProcessImageTooLarge { .. })
        ));
    }

    #[test]
    fn production_process_binary_install_hashes_proc_self_exe() {
        let identity = ProcessBinaryIdentity::install().expect("hash running executable");
        let image = fs::read("/proc/self/exe").expect("read running executable independently");
        assert_eq!(identity.binary_digest(), B256::from_slice(&Sha256::digest(image)));
    }

    fn claim_store(path: &Path) -> VictimClaimStore {
        VictimClaimStore::bootstrap(&VictimClaimConfig { db_path: path.to_path_buf() })
            .expect("bootstrap claim store")
    }

    #[test]
    fn deployment_identity_uses_measured_binary_and_open_store_authorities() {
        let dir = tk::TempDir::new("deployment-identity");
        let image = dir.path.join("image");
        fs::write(&image, b"release-a").expect("write image");
        let process = ProcessBinaryIdentity::from_path(&image).expect("hash image");
        let store = claim_store(&dir.path.join("claims-a.redb"));
        let provider =
            tk::FakeProvider { code_hash: B256::repeat_byte(0x33), block: 100, fail: false };
        let evidence = tk::deployment(
            &provider,
            tk::EXECUTOR,
            B256::repeat_byte(0x33),
            process.binary_digest(),
            B256::repeat_byte(0x44),
            store.store_identity(),
        );
        let source = ProductionDeploymentIdentitySource::install(&evidence, process, &store)
            .expect("install identity");
        let current = source.current().expect("identity");
        assert_eq!(current.binary_digest, process.binary_digest());
        assert_eq!(current.r9_store_identity, store.store_identity());

        fs::write(&image, b"release-b").expect("replace image");
        let changed_process = ProcessBinaryIdentity::from_path(&image).expect("hash replacement");
        assert!(matches!(
            ProductionDeploymentIdentitySource::install(&evidence, changed_process, &store),
            Err(DeploymentIdentityError::BinaryDigestMismatch { .. })
        ));

        let other_store = claim_store(&dir.path.join("claims-b.redb"));
        assert!(matches!(
            ProductionDeploymentIdentitySource::install(&evidence, process, &other_store),
            Err(DeploymentIdentityError::StoreIdentityMismatch { .. })
        ));
    }

    #[test]
    fn production_b5_install_rejects_binary_mismatch() {
        let dir = tk::TempDir::new("production-b5-runtime");
        let store = claim_store(&dir.path.join("claims.redb"));
        let provider =
            tk::FakeProvider { code_hash: B256::repeat_byte(0x33), block: 100, fail: false };
        let evidence = tk::deployment(
            &provider,
            tk::EXECUTOR,
            B256::repeat_byte(0x33),
            B256::ZERO,
            B256::repeat_byte(0x44),
            store.store_identity(),
        );

        let result = ProductionB5Runtime::install(
            &evidence,
            &store,
            StateAuthority { code: Ok(vec![0x60, 0x00, 0x56]), block: Ok(100) },
            Drawdown { fail: false, calls: Cell::new(0) },
        );
        assert!(matches!(
            result,
            Err(ProductionB5RuntimeInstallError::DeploymentIdentity(
                DeploymentIdentityError::BinaryDigestMismatch { .. }
            ))
        ));
    }
}
