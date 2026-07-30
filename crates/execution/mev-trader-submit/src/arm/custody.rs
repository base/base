//! Fail-closed custody loaders for the funded hot-wallet key and the Blink
//! searcher credential. Both paths are compile-pinned; the loaded secret is
//! validated (canonical path, no symlink parent, regular file, mode 0600, owned by
//! the non-root service uid, and — for the wallet — the derived address equals the
//! funded wallet) and zeroized on drop. There is NO external `&SigningKey` seam:
//! the wallet signs only through [`HotWalletKey::sign_unsigned`].

use std::{os::unix::fs::MetadataExt, path::Path};

use alloy_consensus::TxEip1559;
use alloy_primitives::{Address, address};
use k256::ecdsa::SigningKey;
use zeroize::Zeroizing;

use crate::signer::{SignedRaw, SignerError, address_from_verifying_key, sign_with_key};

/// Compile-pinned absolute path of the funded hot-wallet key (`0x` + 64 hex = 66
/// bytes).
pub(crate) const HOT_WALLET_PATH: &str = "/home/ubuntu/.config/mev-trading-hotwallet";

/// Compile-pinned absolute path of the Blink searcher credential (64 hex bytes, no
/// `0x`).
#[cfg(all(feature = "arm-live-egress", not(test)))]
pub(crate) const BLINK_CREDENTIAL_PATH: &str = "/home/ubuntu/.blink-searcher-key";

/// The funded hot-wallet address the loaded key MUST derive to.
pub(crate) const FUNDED_WALLET: Address = address!("98e1e2A84557D49496D1BFE31EA7b5a6C59FD0f9");

/// Exact byte length of the hot-wallet file: `0x` + 64 hex.
const HOT_WALLET_LEN: usize = 66;

/// Exact byte length of the Blink credential file: 64 hex.
#[cfg(any(test, feature = "arm-live-egress"))]
const BLINK_CREDENTIAL_LEN: usize = 64;

/// A custody load failure. Every variant is fail-closed (no key returned).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CustodyError {
    /// The path was not absolute/canonical.
    NotCanonical,
    /// A component of the parent chain is a symlink.
    SymlinkParent,
    /// The credential file is not a regular file.
    NotRegularFile,
    /// The file mode was not exactly 0600.
    BadMode,
    /// The file owner uid did not equal the running service uid.
    WrongUid,
    /// The running uid is root (0) — refused.
    RootUid,
    /// The parent directory owner uid did not equal the running service uid.
    ParentUidMismatch,
    /// The file contents were the wrong length or not valid hex.
    BadFormat,
    /// The derived address did not equal the funded wallet.
    AddressMismatch,
    /// A filesystem read/metadata error.
    Io,
}

/// Stable bounded class for production custody failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionCustodyFailure {
    /// The pinned path was not canonical.
    NotCanonical,
    /// A parent component was a symlink.
    SymlinkParent,
    /// The object was not a regular file.
    NotRegularFile,
    /// The mode was not exactly `0600`.
    BadMode,
    /// The file owner did not match the service user.
    WrongUid,
    /// Running as root was refused.
    RootUid,
    /// The parent owner did not match the service user.
    ParentUidMismatch,
    /// Key bytes were malformed.
    BadFormat,
    /// The derived address did not match the compile-pinned wallet.
    AddressMismatch,
    /// Filesystem inspection or reading failed.
    Io,
}

impl From<CustodyError> for ProductionCustodyFailure {
    fn from(error: CustodyError) -> Self {
        match error {
            CustodyError::NotCanonical => Self::NotCanonical,
            CustodyError::SymlinkParent => Self::SymlinkParent,
            CustodyError::NotRegularFile => Self::NotRegularFile,
            CustodyError::BadMode => Self::BadMode,
            CustodyError::WrongUid => Self::WrongUid,
            CustodyError::RootUid => Self::RootUid,
            CustodyError::ParentUidMismatch => Self::ParentUidMismatch,
            CustodyError::BadFormat => Self::BadFormat,
            CustodyError::AddressMismatch => Self::AddressMismatch,
            CustodyError::Io => Self::Io,
        }
    }
}

/// Returns the running process's real uid, libc-free, by reading the owner of
/// `/proc/self` (owned by the process uid on Linux). `None` if unavailable.
fn service_uid() -> Option<u32> {
    std::fs::metadata("/proc/self").ok().map(|meta| meta.uid())
}

/// Shared fail-closed filesystem policy: absolute path, no symlink in the parent
/// chain, a regular file at mode 0600, owned by the (non-root) service uid, and a
/// parent directory owned by the same uid.
fn verify_custody_path(path: &Path) -> Result<(), CustodyError> {
    if !path.is_absolute() {
        return Err(CustodyError::NotCanonical);
    }
    let uid = service_uid().ok_or(CustodyError::Io)?;
    if uid == 0 {
        return Err(CustodyError::RootUid);
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(CustodyError::NotCanonical)?;
    // No symlink anywhere in the parent chain.
    for ancestor in parent.ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        if let Ok(meta) = std::fs::symlink_metadata(ancestor)
            && meta.file_type().is_symlink()
        {
            return Err(CustodyError::SymlinkParent);
        }
    }
    // Parent directory must be owned by the service uid.
    let parent_meta = std::fs::symlink_metadata(parent).map_err(|_| CustodyError::Io)?;
    if !parent_meta.file_type().is_dir() {
        return Err(CustodyError::NotRegularFile);
    }
    if parent_meta.uid() != uid {
        return Err(CustodyError::ParentUidMismatch);
    }
    // The credential file itself: regular, 0600, owned by the service uid.
    let file_meta = std::fs::symlink_metadata(path).map_err(|_| CustodyError::Io)?;
    if !file_meta.file_type().is_file() {
        return Err(CustodyError::NotRegularFile);
    }
    if file_meta.mode() & 0o777 != 0o600 {
        return Err(CustodyError::BadMode);
    }
    if file_meta.uid() != uid {
        return Err(CustodyError::WrongUid);
    }
    Ok(())
}

/// The funded hot-wallet signing key. Holds a k256 [`SigningKey`] (which zeroizes
/// its scalar on drop) plus the derived address. No accessor returns the key.
pub(crate) struct HotWalletKey {
    signing_key: SigningKey,
    address: Address,
}

impl core::fmt::Debug for HotWalletKey {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("HotWalletKey")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl HotWalletKey {
    /// Loads the funded hot wallet from the compile-pinned path, enforcing the full
    /// fail-closed policy AND that the derived address equals [`FUNDED_WALLET`].
    pub(crate) fn load() -> Result<Self, CustodyError> {
        Self::load_verified(Path::new(HOT_WALLET_PATH), FUNDED_WALLET)
    }

    /// Test-only seam: load from an explicit path/address so every fail-closed
    /// branch AND the success path are exercisable with a temp file + a test key's
    /// own address. `#[cfg(test)]` so production/arm-wiring code can NEVER load a key
    /// from an arbitrary path/address — the only production entry is [`load`], which
    /// hardwires the pinned funded path + address.
    #[cfg(test)]
    pub(crate) fn load_from(path: &Path, expected: Address) -> Result<Self, CustodyError> {
        Self::load_verified(path, expected)
    }

    /// PRIVATE path/address-parametrized loader (module-private `fn`, not
    /// `pub`). The pinned [`load`](Self::load) hardwires the funded path +
    /// address; only the test seam can call it with other values.
    fn load_verified(path: &Path, expected: Address) -> Result<Self, CustodyError> {
        verify_custody_path(path)?;
        let raw = Zeroizing::new(std::fs::read(path).map_err(|_| CustodyError::Io)?);
        if raw.len() != HOT_WALLET_LEN {
            return Err(CustodyError::BadFormat);
        }
        if &raw[..2] != b"0x" {
            return Err(CustodyError::BadFormat);
        }
        let secret = Zeroizing::new(
            alloy_primitives::hex::decode(&raw[2..]).map_err(|_| CustodyError::BadFormat)?,
        );
        let signing_key = SigningKey::from_slice(&secret).map_err(|_| CustodyError::BadFormat)?;
        let address = address_from_verifying_key(signing_key.verifying_key());
        if address != expected {
            return Err(CustodyError::AddressMismatch);
        }
        Ok(Self { signing_key, address })
    }

    /// Sign an unsigned envelope with the loaded key. The key never escapes: only
    /// the signed bytes are returned (via the shared [`sign_with_key`] core).
    pub(crate) fn sign_unsigned(&self, unsigned: &TxEip1559) -> Result<SignedRaw, SignerError> {
        sign_with_key(&self.signing_key, unsigned)
    }

    /// The funded wallet address (public, never the key).
    pub(crate) const fn address(&self) -> Address {
        self.address
    }
}

/// Opens the compile-pinned production key once, verifies its identity, and immediately drops it.
pub fn production_custody_preflight() -> Result<(), ProductionCustodyFailure> {
    HotWalletKey::load().map(drop).map_err(ProductionCustodyFailure::from)
}

/// The Blink searcher credential — a SEPARATE type and format (64 hex, no `0x`).
/// The bytes are zeroized on drop; only [`expose`](Self::expose) (crate-private,
/// used solely by the live-egress backend) reveals them. Compiled ONLY where it
/// has a user (`arm-live-egress` `ProdBackend`, or the custody tests) — the bare
/// `arm` build never constructs it, so there is no dead field and no allow.
#[cfg(any(test, feature = "arm-live-egress"))]
pub(crate) struct BlinkCredential {
    secret: Zeroizing<Vec<u8>>,
}

#[cfg(any(test, feature = "arm-live-egress"))]
impl core::fmt::Debug for BlinkCredential {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never print the secret.
        formatter.debug_struct("BlinkCredential").field("secret", &"<redacted>").finish()
    }
}

#[cfg(any(test, feature = "arm-live-egress"))]
impl BlinkCredential {
    /// Loads the Blink credential from the compile-pinned path.
    #[cfg(all(feature = "arm-live-egress", not(test)))]
    pub(crate) fn load() -> Result<Self, CustodyError> {
        Self::load_verified(Path::new(BLINK_CREDENTIAL_PATH))
    }

    /// Test-only seam: load from an explicit path. `#[cfg(test)]` so
    /// production/arm-wiring code can NEVER load a credential from an arbitrary
    /// path — the only production entry is [`load`], which pins the path.
    #[cfg(test)]
    pub(crate) fn load_from(path: &Path) -> Result<Self, CustodyError> {
        Self::load_verified(path)
    }

    /// PRIVATE path-parametrized loader (module-private `fn`, not `pub`).
    fn load_verified(path: &Path) -> Result<Self, CustodyError> {
        verify_custody_path(path)?;
        let raw = Zeroizing::new(std::fs::read(path).map_err(|_| CustodyError::Io)?);
        if raw.len() != BLINK_CREDENTIAL_LEN {
            return Err(CustodyError::BadFormat);
        }
        // Validate ASCII hex IN PLACE (no decoded temp copy to zeroize): the raw
        // ASCII hex IS the credential (used verbatim as the URL path segment).
        if !raw.iter().all(u8::is_ascii_hexdigit) {
            return Err(CustodyError::BadFormat);
        }
        Ok(Self { secret: Zeroizing::new(raw.to_vec()) })
    }

    /// Crate-private raw credential bytes, used by the live-egress backend when
    /// composing the Blink auth URL path segment (and read by the custody test that
    /// checks confinement). Compiled only where `BlinkCredential` exists.
    pub(crate) fn expose(&self) -> &[u8] {
        &self.secret
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    use super::*;
    use crate::arm::testkit as tk;

    #[test]
    fn load_success_and_signs() {
        let dir = tk::TempDir::new("custody-ok");
        let (key, address) = tk::hot_wallet_key();
        let path = tk::write_hot_wallet(&dir.path, &key);
        let loaded = HotWalletKey::load_from(&path, address).expect("load");
        assert_eq!(loaded.address(), address);
        // It can sign an envelope (key never escapes).
        let (vtx, _victim) = tk::validated_tx(tk::EXECUTOR);
        assert!(loaded.sign_unsigned(vtx.unsigned_tx()).is_ok());
    }

    #[test]
    fn address_mismatch_is_fail_closed() {
        let dir = tk::TempDir::new("custody-addr");
        let (key, _address) = tk::hot_wallet_key();
        let path = tk::write_hot_wallet(&dir.path, &key);
        let wrong = Address::repeat_byte(0x11);
        assert_eq!(
            HotWalletKey::load_from(&path, wrong).unwrap_err(),
            CustodyError::AddressMismatch
        );
    }

    #[test]
    fn bad_mode_is_fail_closed() {
        let dir = tk::TempDir::new("custody-mode");
        let (key, address) = tk::hot_wallet_key();
        let path = tk::write_hot_wallet(&dir.path, &key);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(HotWalletKey::load_from(&path, address).unwrap_err(), CustodyError::BadMode);
    }

    #[test]
    fn bad_length_is_fail_closed() {
        let dir = tk::TempDir::new("custody-len");
        let path = dir.path.join("hotwallet");
        std::fs::write(&path, b"0xdeadbeef").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            HotWalletKey::load_from(&path, Address::ZERO).unwrap_err(),
            CustodyError::BadFormat
        );
    }

    #[test]
    fn symlink_parent_is_fail_closed() {
        let dir = tk::TempDir::new("custody-sym");
        let real = dir.path.join("real");
        std::fs::DirBuilder::new().mode(0o700).create(&real).unwrap();
        let (key, address) = tk::hot_wallet_key();
        let path = tk::write_hot_wallet(&real, &key);
        let _ = path;
        // Access the same file through a symlinked parent.
        let link = dir.path.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let via_link = link.join("hotwallet");
        assert_eq!(
            HotWalletKey::load_from(&via_link, address).unwrap_err(),
            CustodyError::SymlinkParent
        );
    }

    #[test]
    fn relative_path_is_fail_closed() {
        assert_eq!(
            HotWalletKey::load_from(Path::new("relative/hotwallet"), Address::ZERO).unwrap_err(),
            CustodyError::NotCanonical
        );
    }

    #[test]
    fn blink_credential_load_and_length() {
        let dir = tk::TempDir::new("blink-ok");
        let path = dir.path.join("cred");
        // 64 hex chars.
        let hex = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        std::fs::write(&path, hex).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let cred = BlinkCredential::load_from(&path).expect("load");
        // The credential is the raw ASCII hex (used verbatim as the URL path segment).
        assert_eq!(cred.expose(), hex);

        // Wrong length.
        let short = dir.path.join("cred2");
        std::fs::write(&short, b"0123").unwrap();
        std::fs::set_permissions(&short, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(BlinkCredential::load_from(&short).unwrap_err(), CustodyError::BadFormat);
    }
}
