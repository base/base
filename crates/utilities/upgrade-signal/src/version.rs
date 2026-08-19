//! Ordered comparison of OP-Stack packed-semver protocol versions.

use core::cmp::Ordering;

use alloy_primitives::U256;

/// An OP-Stack `ProtocolVersions` packed-semver value ordered by its semver fields.
///
/// The L1 `ProtocolVersions` contract stores each version as a version-type `0` `uint256`:
/// `reserved || build || major || minor || patch || pre-release`, where `major`, `minor`, `patch`,
/// and `pre-release` are the low four 32-bit fields and `build` occupies the next 64 bits.
///
/// Ordering follows the OP-Stack superchain-upgrade rules rather than the raw integer value:
///
/// * compare `major`, then `minor`, then `patch`;
/// * a pre-release (`pre-release != 0`) sorts *below* its matching release (`pre-release == 0`),
///   and pre-releases of the same `major.minor.patch` sort by their pre-release counter;
/// * `build` and the reserved/version-type high bytes are ignored.
///
/// A raw `U256` comparison instead sorts a pre-release *above* its release, because the pre-release
/// field holds a larger integer than the release's zero. That inverts the intended order and can
/// reject a node running a final release under a minimum that is a release candidate of the same
/// version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackedProtocolVersion(U256);

impl PackedProtocolVersion {
    /// Wraps a packed protocol version read from the L1 `ProtocolVersions` contract.
    pub const fn new(packed: U256) -> Self {
        Self(packed)
    }

    /// Packs the ordered semver fields into the version-type `0` layout, leaving `build` and the
    /// reserved/version-type bytes zero.
    pub const fn pack(major: u32, minor: u32, patch: u32, prerelease: u32) -> Self {
        Self(U256::from_limbs([
            ((patch as u64) << 32) | prerelease as u64,
            ((major as u64) << 32) | minor as u64,
            0,
            0,
        ]))
    }

    /// Returns the underlying packed `U256` value.
    pub const fn into_inner(self) -> U256 {
        self.0
    }

    /// Major version field.
    pub const fn major(self) -> u32 {
        (self.0.as_limbs()[1] >> 32) as u32
    }

    /// Minor version field.
    pub const fn minor(self) -> u32 {
        self.0.as_limbs()[1] as u32
    }

    /// Patch version field.
    pub const fn patch(self) -> u32 {
        (self.0.as_limbs()[0] >> 32) as u32
    }

    /// Pre-release counter field; `0` denotes a final release.
    pub const fn prerelease(self) -> u32 {
        self.0.as_limbs()[0] as u32
    }

    /// Ordering key that applies the semver pre-release rule.
    ///
    /// A final release (`prerelease == 0`) is promoted above every pre-release of the same
    /// `major.minor.patch` by ranking it as [`u64::MAX`], which no 32-bit pre-release counter can
    /// reach.
    const fn ordering_key(self) -> (u32, u32, u32, u64) {
        let prerelease_rank =
            if self.prerelease() == 0 { u64::MAX } else { self.prerelease() as u64 };
        (self.major(), self.minor(), self.patch(), prerelease_rank)
    }
}

impl Ord for PackedProtocolVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.ordering_key().cmp(&other.ordering_key())
    }
}

impl PartialOrd for PackedProtocolVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_packed_semver_fields() {
        let version = PackedProtocolVersion::pack(1, 2, 3, 4);

        assert_eq!(version.major(), 1);
        assert_eq!(version.minor(), 2);
        assert_eq!(version.patch(), 3);
        assert_eq!(version.prerelease(), 4);
    }

    #[test]
    fn release_sorts_above_its_prerelease() {
        let release = PackedProtocolVersion::pack(1, 2, 3, 0);
        let prerelease = PackedProtocolVersion::pack(1, 2, 3, 1);

        // The raw integers order the prerelease higher; the semver ordering must not.
        assert!(prerelease.into_inner() > release.into_inner());
        assert!(prerelease < release);
    }

    #[test]
    fn prereleases_sort_by_counter() {
        let rc1 = PackedProtocolVersion::pack(1, 2, 3, 1);
        let rc2 = PackedProtocolVersion::pack(1, 2, 3, 2);

        assert!(rc1 < rc2);
    }

    #[test]
    fn orders_by_major_then_minor_then_patch() {
        assert!(PackedProtocolVersion::pack(1, 2, 3, 0) < PackedProtocolVersion::pack(1, 2, 4, 0));
        assert!(PackedProtocolVersion::pack(1, 2, 3, 0) < PackedProtocolVersion::pack(1, 3, 0, 0));
        assert!(PackedProtocolVersion::pack(1, 2, 3, 0) < PackedProtocolVersion::pack(2, 0, 0, 0));

        // A higher patch release outranks a lower-patch pre-release regardless of pre-release rank.
        assert!(PackedProtocolVersion::pack(1, 2, 3, 9) < PackedProtocolVersion::pack(1, 2, 4, 0));
    }

    #[test]
    fn build_field_does_not_affect_ordering() {
        // Set the build field (bits 128..192) on one operand; ordering must ignore it.
        let plain = PackedProtocolVersion::pack(1, 2, 3, 0);
        let with_build = PackedProtocolVersion::new(plain.into_inner() | (U256::from(7) << 128));

        assert_eq!(plain.cmp(&with_build), Ordering::Equal);
    }
}
